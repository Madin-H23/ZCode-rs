//! git 子进程（packages/services/src/git/config.ts、providers/gitEnvironmentProvider.ts、
//! providers/gitCommandProvider.ts）。
//!
//! 与 TS 相同：无 shell、无 stdin；每类命令单独的超时与输出上限；超时先温和终止，2 s 后强杀，
//! 再过 2 s 仍未结束就放弃等待（orphaned）。spawn 失败不抛错，按 exitCode -2 返回。
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::{Notify, OnceCell};
use zcode_host_kit::RpcError;

use crate::support::env::ProcessEnv;
use crate::support::node_error;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);
const KILL_GRACE: Duration = Duration::from_secs(2);
const FORCE_KILL_GRACE: Duration = Duration::from_secs(2);

/// pre-push hook 会注入所在仓库的 GIT_DIR 等变量；原样透传会让命令“串仓”。
const LOCAL_ENV_VARS: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_CONFIG",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_DIR",
    "GIT_GRAFT_FILE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_INTERNAL_SUPER_PREFIX",
    "GIT_NAMESPACE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
    "GIT_REPLACE_REF_BASE",
    "GIT_SHALLOW_FILE",
    "GIT_WORK_TREE",
];

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub timeout: Duration,
    pub max_bytes: usize,
}

impl Limits {
    pub const COMMAND: Self = Self {
        timeout: Duration::from_secs(15),
        max_bytes: 512 * 1024,
    };
    pub const DIFF: Self = Self {
        timeout: Duration::from_secs(20),
        max_bytes: 1024 * 1024,
    };
    /// push 可能被 pre-push hook 拖很久、输出完整测试日志，单独放宽。
    pub const PUSH: Self = Self {
        timeout: Duration::from_secs(10 * 60),
        max_bytes: 8 * 1024 * 1024,
    };
}

/// 一次 git 调用的结果（`GitCommandExecutionResult`）。
#[derive(Debug, Clone, Default)]
pub struct GitOutput {
    pub stdout: String,
    pub stderr: String,
    /// None：被信号终止或放弃等待；-2：spawn 失败。
    pub exit_code: Option<i64>,
    pub timed_out: bool,
    pub output_truncated: bool,
    timeout_ms: u128,
    duration_ms: u128,
    timeout_elapsed_ms: Option<u128>,
    force_kill_attempted: bool,
    orphaned: bool,
}

impl GitOutput {
    #[cfg(test)]
    pub fn exited(code: i64, stdout: &str, stderr: &str) -> Self {
        Self {
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
            exit_code: Some(code),
            ..Self::default()
        }
    }

    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0)
    }

    /// stderr、stdout 依次取非空者，否则 `exitCode=<n>`。
    pub fn message(&self) -> String {
        let stderr = self.stderr.trim();
        if !stderr.is_empty() {
            return stderr.to_owned();
        }
        let stdout = self.stdout.trim();
        if !stdout.is_empty() {
            return stdout.to_owned();
        }
        match self.exit_code {
            Some(code) => format!("exitCode={code}"),
            None => "exitCode=null".to_owned(),
        }
    }

    pub fn ensure(&self, label: &str) -> Result<(), RpcError> {
        self.ensure_codes(label, &[0])
    }

    /// `ensureGitCommandSucceeded`：超时、截断、退出码不在允许列表时报错。
    pub fn ensure_codes(&self, label: &str, allowed: &[i64]) -> Result<(), RpcError> {
        if self.timed_out {
            let mut details = vec![format!("elapsed={}ms", self.duration_ms)];
            if let Some(kill_at) = self.timeout_elapsed_ms {
                details.push(format!("killAt={kill_at}ms"));
                details.push(format!(
                    "cleanup={}ms",
                    self.duration_ms.saturating_sub(kill_at)
                ));
            }
            if self.force_kill_attempted {
                details.push("forceKill=true".to_owned());
            }
            if self.orphaned {
                details.push("orphaned=true".to_owned());
            }
            return Err(git_error(format!(
                "{label} timed out after {}ms ({})",
                self.timeout_ms,
                details.join(", ")
            )));
        }
        if self.output_truncated {
            return Err(git_error(format!("{label} output exceeded limit")));
        }
        if self.exit_code.is_some_and(|code| allowed.contains(&code)) {
            return Ok(());
        }
        Err(git_error(format!("{label} failed: {}", self.message())))
    }
}

pub fn git_error(message: impl Into<String>) -> RpcError {
    RpcError::new("Error", message)
}

#[derive(Clone)]
pub struct GitRunner {
    inner: Arc<Inner>,
}

struct Inner {
    env: ProcessEnv,
    candidates: Vec<String>,
    /// 首次探测结果（包括“没有 git”）在 service 生命周期内缓存。
    binary: OnceCell<Option<String>>,
}

fn binary_candidates(base: &ProcessEnv) -> Vec<String> {
    let mut candidates: Vec<String> = base
        .non_empty("ZCODE_GIT_BINARY")
        .map(str::to_owned)
        .into_iter()
        .collect();
    candidates.push("git".to_owned());
    if cfg!(windows) {
        let program_files = base
            .get("ProgramW6432")
            .or_else(|| base.get("ProgramFiles"))
            .unwrap_or(r"C:\Program Files");
        let program_files_x86 = base
            .get("ProgramFiles(x86)")
            .unwrap_or(r"C:\Program Files (x86)");
        for root in [program_files, program_files_x86] {
            for dir in ["cmd", "bin"] {
                candidates.push(format!(r"{root}\Git\{dir}\git.exe"));
            }
        }
    }
    candidates
}

/// `getGitCommandEnv`：清掉仓库局部变量，固定分页器与英文输出（错误解析依赖英文 stderr）。
fn command_env(base: &ProcessEnv) -> ProcessEnv {
    let mut env = base.clone();
    for name in LOCAL_ENV_VARS {
        env.remove(name);
    }
    for (key, value) in [
        ("GIT_OPTIONAL_LOCKS", "0"),
        ("GIT_PAGER", "cat"),
        ("PAGER", "cat"),
        ("TERM", "dumb"),
        ("LC_ALL", "C"),
        ("LANG", "C"),
    ] {
        env.set(key, value);
    }
    env
}

fn command(program: &str, env: &ProcessEnv) -> Command {
    let mut command = zcode_proc::command(program);
    command.env_clear().envs(env.vars()).stdin(Stdio::null());
    #[cfg(windows)]
    {
        // windowsHide：不为 git 弹出控制台窗口。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

async fn can_execute(candidate: &str, env: &ProcessEnv) -> bool {
    let mut probe = command(candidate, env);
    probe
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let Ok(mut child) = probe.spawn() else {
        return false;
    };
    matches!(
        tokio::time::timeout(DISCOVERY_TIMEOUT, child.wait()).await,
        Ok(Ok(status)) if status.success()
    )
}

/// 读满一个管道；任一管道超出上限后两边都不再保留输出（TS 共用一个 outputTruncated）。
async fn read_capped(
    mut pipe: impl AsyncRead + Unpin,
    max_bytes: usize,
    truncated: &AtomicBool,
    overflow: &Notify,
) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let read = match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => return kept,
            Ok(read) => read,
        };
        if truncated.load(Ordering::Relaxed) {
            continue;
        }
        if kept.len() + read > max_bytes {
            truncated.store(true, Ordering::Relaxed);
            overflow.notify_one();
            continue;
        }
        kept.extend_from_slice(&chunk[..read]);
    }
}

#[cfg(windows)]
fn force_kill(child: &mut tokio::process::Child, env: &ProcessEnv) {
    // Windows：taskkill /T 连同 git 拉起的 helper/filter 子进程一起结束。
    if let Some(pid) = child.id() {
        let mut killer = command("taskkill", env);
        killer
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Ok(mut killer) = killer.spawn() {
            tokio::spawn(async move {
                let _ = tokio::time::timeout(KILL_GRACE, killer.wait()).await;
            });
        }
    }
}

#[cfg(not(windows))]
fn force_kill(child: &mut tokio::process::Child, _env: &ProcessEnv) {
    let _ = child.start_kill();
}

/// Node `child.kill()`：POSIX 上是 SIGTERM，Windows 上是 TerminateProcess。
fn soft_kill(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        crate::support::signal::terminate(pid);
    }
    #[cfg(not(unix))]
    let _ = child.start_kill();
}

impl GitRunner {
    pub fn new(base: &ProcessEnv) -> Self {
        Self {
            inner: Arc::new(Inner {
                env: command_env(base),
                candidates: binary_candidates(base),
                binary: OnceCell::new(),
            }),
        }
    }

    /// 第一个 `--version` 在 3 s 内退出 0 的候选。
    pub async fn binary(&self) -> Option<String> {
        self.inner
            .binary
            .get_or_init(|| async {
                for candidate in &self.inner.candidates {
                    if can_execute(candidate, &self.inner.env).await {
                        return Some(candidate.clone());
                    }
                }
                None
            })
            .await
            .clone()
    }

    pub async fn run(
        &self,
        cwd: &str,
        args: &[&str],
        limits: Limits,
    ) -> Result<GitOutput, RpcError> {
        self.run_with_env(cwd, args, limits, &[]).await
    }

    pub async fn run_with_env(
        &self,
        cwd: &str,
        args: &[&str],
        limits: Limits,
        extra_env: &[(&str, &str)],
    ) -> Result<GitOutput, RpcError> {
        let Some(binary) = self.binary().await else {
            return Err(git_error("Git binary is not available"));
        };
        let started = Instant::now();
        let mut env = self.inner.env.clone();
        for (key, value) in extra_env {
            env.set(*key, *value);
        }
        let mut output = GitOutput {
            timeout_ms: limits.timeout.as_millis(),
            ..GitOutput::default()
        };
        let mut spawn = command(&binary, &env);
        spawn
            .args(args)
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match spawn.spawn() {
            Ok(child) => child,
            Err(error) => {
                // cwd 被并发删除等情况：降级成失败结果，由调用方按退出码处理。
                output.exit_code = Some(-2);
                output.stderr = node_error::spawn_message(&binary, &error);
                output.duration_ms = started.elapsed().as_millis();
                return Ok(output);
            }
        };
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(git_error("git pipes are unavailable"));
        };

        let truncated = AtomicBool::new(false);
        let overflow = Notify::new();
        let reading = async {
            tokio::join!(
                read_capped(stdout, limits.max_bytes, &truncated, &overflow),
                read_capped(stderr, limits.max_bytes, &truncated, &overflow),
            )
        };
        tokio::pin!(reading);
        let deadline = tokio::time::sleep(limits.timeout);
        tokio::pin!(deadline);
        let mut pipes: Option<(Vec<u8>, Vec<u8>)> = None;
        let mut status: Option<Option<i64>> = None;
        let mut kill_stage = 0;
        // 与 Node 的 'close' 相同：进程退出且两个管道都读完才算结束。
        while pipes.is_none() || status.is_none() {
            tokio::select! {
                result = child.wait(), if status.is_none() => {
                    status = Some(result.ok().and_then(|status| status.code()).map(i64::from));
                }
                result = &mut reading, if pipes.is_none() => pipes = Some(result),
                () = overflow.notified() => {
                    if status.is_none() {
                        soft_kill(&mut child);
                    }
                }
                () = &mut deadline => {
                    let next = match kill_stage {
                        0 => {
                            output.timed_out = true;
                            output.timeout_elapsed_ms = Some(started.elapsed().as_millis());
                            if status.is_none() {
                                soft_kill(&mut child);
                            }
                            KILL_GRACE
                        }
                        1 => {
                            output.force_kill_attempted = true;
                            if status.is_none() {
                                force_kill(&mut child, &env);
                            }
                            FORCE_KILL_GRACE
                        }
                        _ => {
                            // helper/filter 进程拖住管道：放弃等待，避免卡住调用链。
                            output.orphaned = true;
                            break;
                        }
                    };
                    kill_stage += 1;
                    deadline.as_mut().reset(tokio::time::Instant::now() + next);
                }
            }
        }

        let (stdout, stderr) = pipes.unwrap_or_default();
        output.stdout = String::from_utf8_lossy(&stdout).into_owned();
        output.stderr = String::from_utf8_lossy(&stderr).into_owned();
        output.exit_code = status.flatten();
        output.output_truncated = truncated.load(Ordering::Relaxed);
        output.duration_ms = started.elapsed().as_millis();
        Ok(output)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn runner_with(vars: &[(&str, &str)]) -> GitRunner {
        GitRunner::new(&ProcessEnv::new(
            vars.iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
        ))
    }

    /// 假 git：`--version` 成功，其余参数交给 /bin/sh，用来构造超时、超量输出等场景。
    #[cfg(unix)]
    fn shell_runner(dir: &std::path::Path) -> GitRunner {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("fake-git");
        std::fs::write(
            &script,
            "#!/bin/sh\nif [ \"$1\" = --version ]; then exit 0; fi\nexec /bin/sh \"$@\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        runner_with(&[
            ("ZCODE_GIT_BINARY", script.to_str().unwrap()),
            ("PATH", "/usr/bin:/bin"),
        ])
    }

    #[test]
    fn env_scrubs_repository_variables() {
        let env = command_env(&ProcessEnv::new(
            [
                ("GIT_DIR", "/x/.git"),
                ("LANG", "zh_CN.UTF-8"),
                ("PATH", "/bin"),
            ]
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
        ));
        assert_eq!(env.get("GIT_DIR"), None);
        assert_eq!(env.get("LANG"), Some("C"));
        assert_eq!(env.get("LC_ALL"), Some("C"));
        assert_eq!(env.get("PATH"), Some("/bin"));
    }

    #[test]
    fn ensure_formats_like_ts() {
        let failed = GitOutput::exited(128, "", "fatal: bad\n");
        assert_eq!(
            failed.ensure("git add").unwrap_err().message,
            "git add failed: fatal: bad"
        );
        assert!(
            GitOutput::exited(1, "", "")
                .ensure_codes("x", &[0, 1])
                .is_ok()
        );
        assert_eq!(
            GitOutput::exited(3, "", "")
                .ensure("x")
                .unwrap_err()
                .message,
            "x failed: exitCode=3"
        );
    }

    #[tokio::test]
    async fn missing_binary_is_cached_as_unavailable() {
        let runner = runner_with(&[("PATH", "/nonexistent"), ("ZCODE_GIT_BINARY", "/nope/git")]);
        assert_eq!(runner.binary().await, None);
        let error = runner
            .run("/", &["status"], Limits::COMMAND)
            .await
            .unwrap_err();
        assert_eq!(error.message, "Git binary is not available");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_failure_is_a_result_and_output_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let runner = shell_runner(dir.path());
        let missing = runner
            .run("/definitely/missing/dir", &["-c", "true"], Limits::COMMAND)
            .await
            .unwrap();
        assert_eq!(missing.exit_code, Some(-2));
        assert!(missing.stderr.to_lowercase().contains("enoent"));

        let limits = Limits {
            timeout: Duration::from_secs(5),
            max_bytes: 16,
        };
        let big = runner
            .run("/", &["-c", "yes | head -c 100000"], limits)
            .await
            .unwrap();
        assert!(big.output_truncated);
        assert!(big.stdout.len() <= 16);
        assert_eq!(
            big.ensure("x").unwrap_err().message,
            "x output exceeded limit"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_terminates_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let runner = shell_runner(dir.path());
        let limits = Limits {
            timeout: Duration::from_millis(100),
            max_bytes: 1024,
        };
        let output = runner
            .run("/", &["-c", "exec sleep 30"], limits)
            .await
            .unwrap();
        assert!(output.timed_out);
        assert!(!output.force_kill_attempted);
        assert_eq!(output.exit_code, None);
        let message = output.ensure("git status").unwrap_err().message;
        assert!(message.starts_with("git status timed out after 100ms (elapsed="));
    }
}
