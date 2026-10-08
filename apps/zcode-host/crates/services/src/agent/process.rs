//! CLI 子进程的启动与进程树回收（zcodeAgentProcessManager.ts startClient、zcodeStdioTransport.ts
//! disposeAndWaitOnce、agentStderrCollector.ts）。
//!
//! - POSIX 上子进程自成进程组（pgid = pid），回收时对整组发信号，连带工具子进程。
//! - 回收顺序：关闭 stdin（app-server 把 EOF 当退出请求）→ 等 1.8 s → SIGTERM 整组 → 再等 2 s →
//!   SIGKILL；Windows 用 `taskkill /T /F`。主进程已退出时仍清理同组残留进程。
//! - stderr 只用于诊断：逐行进 debug 日志，保留脱敏后的最后 20 行，异常退出时附在 warn 日志里。
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use regex::Regex;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use zcode_host_kit::log;

use super::command::AgentCommand;

const GRACEFUL_EXIT_WAIT: Duration = Duration::from_millis(1800);
const TERMINATE_WAIT: Duration = Duration::from_secs(2);
const STDERR_TAIL_LINES: usize = 20;

pub struct Spawned {
    pub child: Child,
    pub pid: Option<u32>,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

pub fn spawn(
    command: &AgentCommand,
    cwd: &Path,
    env: &BTreeMap<String, String>,
) -> std::io::Result<Spawned> {
    let mut process = Command::new(&command.program);
    process
        .args(&command.args)
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false);
    #[cfg(unix)]
    process.process_group(0);
    #[cfg(windows)]
    process.creation_flags(CREATE_NO_WINDOW);
    let mut child = process.spawn()?;
    let pid = child.id();
    let missing = || std::io::Error::other("child stdio was not captured");
    Ok(Spawned {
        stdin: child.stdin.take().ok_or_else(missing)?,
        stdout: child.stdout.take().ok_or_else(missing)?,
        stderr: child.stderr.take().ok_or_else(missing)?,
        child,
        pid,
    })
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// stderr 的最后若干行（已脱敏）。
#[derive(Clone, Default)]
pub struct StderrTail(Arc<Mutex<VecDeque<String>>>);

impl StderrTail {
    pub fn lines(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    fn push(&self, line: String) {
        let mut lines = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if lines.len() == STDERR_TAIL_LINES {
            lines.pop_front();
        }
        lines.push_back(line);
    }
}

fn secret_patterns() -> &'static [Regex; 2] {
    static PATTERNS: std::sync::OnceLock<[Regex; 2]> = std::sync::OnceLock::new();
    PATTERNS.get_or_init(|| {
        let compile = |pattern: &str| {
            Regex::new(pattern).unwrap_or_else(|_| unreachable!("static pattern"))
        };
        [
            compile(
                r#"(?i)((?:api[_-]?key|token|secret|password|authorization)["']?\s*[:=]\s*["']?)(?:bearer\s+)?[^\s"',]+"#,
            ),
            compile(r#"(?i)(bearer\s+)[^\s"',]+"#),
        ]
    })
}

pub fn redact(line: &str) -> String {
    let [assignment, bearer] = secret_patterns();
    let line = assignment.replace_all(line, "${1}[REDACTED]");
    bearer.replace_all(&line, "${1}[REDACTED]").into_owned()
}

/// 一直读到 EOF，避免管道写满后子进程阻塞。
pub fn drain_stderr(stderr: ChildStderr, label: String) -> StderrTail {
    let tail = StderrTail::default();
    let collected = tail.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let line = redact(&line);
            log::debug(format!("{label} stderr: {line}"));
            collected.push(line);
        }
    });
    tail
}

/// 等子进程自行退出，超时后逐级升级；调用前应已关闭它的 stdin。
pub async fn terminate(child: &mut Child, pid: Option<u32>) {
    if tokio::time::timeout(GRACEFUL_EXIT_WAIT, child.wait())
        .await
        .is_ok()
    {
        kill_leftovers(pid).await;
        return;
    }
    signal_tree(pid, Signal::Terminate).await;
    if tokio::time::timeout(TERMINATE_WAIT, child.wait())
        .await
        .is_ok()
    {
        kill_leftovers(pid).await;
        return;
    }
    signal_tree(pid, Signal::Kill).await;
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
}

#[derive(Clone, Copy)]
enum Signal {
    Terminate,
    Kill,
}

/// 主进程已退出后，同组里还活着的工具进程（被 CLI 拉起又没跟着退出的）一并结束。
async fn kill_leftovers(pid: Option<u32>) {
    #[cfg(unix)]
    signal_tree(pid, Signal::Kill).await;
    // Windows 上主进程退出后进程树关系即断开，taskkill /T 已无从追溯，不再处理。
    #[cfg(not(unix))]
    let _ = pid;
}

#[cfg(unix)]
async fn signal_tree(pid: Option<u32>, signal: Signal) {
    let Some(pgid) = pid.and_then(|pid| libc::pid_t::try_from(pid).ok()) else {
        return;
    };
    let signal = match signal {
        Signal::Terminate => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    #[allow(unsafe_code)]
    // SAFETY: kill 只投递信号，不读写内存；负 pid 指向 spawn 时以子进程 pid 建立的进程组。
    unsafe {
        libc::kill(-pgid, signal);
    }
}

#[cfg(windows)]
async fn signal_tree(pid: Option<u32>, _signal: Signal) {
    let Some(pid) = pid else { return };
    let mut command = Command::new("taskkill");
    command
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    if let Ok(mut child) = command.spawn() {
        let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
    }
}

#[cfg(not(any(unix, windows)))]
async fn signal_tree(_pid: Option<u32>, _signal: Signal) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secret_values() {
        assert_eq!(
            redact("request failed: Authorization: Bearer abc.def"),
            "request failed: Authorization: [REDACTED]"
        );
        assert_eq!(redact("apiKey=sk-123 ok"), "apiKey=[REDACTED] ok");
        assert_eq!(redact(r#"{"token": "xyz"}"#), r#"{"token": "[REDACTED]"}"#);
        assert_eq!(redact("got Bearer abc"), "got Bearer [REDACTED]");
        assert_eq!(redact("plain line"), "plain line");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminate_escalates_when_stdin_eof_is_ignored() {
        let command = AgentCommand {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "trap '' TERM; sleep 30".into()],
            cwd: None,
            env: BTreeMap::new(),
            supports_storage_startup: false,
        };
        let env = BTreeMap::from([("PATH".to_owned(), "/usr/bin:/bin".to_owned())]);
        let mut spawned = spawn(&command, Path::new("/"), &env).unwrap();
        drop(spawned.stdin);
        let started = std::time::Instant::now();
        terminate(&mut spawned.child, spawned.pid).await;
        assert!(spawned.child.try_wait().unwrap().is_some());
        assert!(started.elapsed() >= GRACEFUL_EXIT_WAIT + TERMINATE_WAIT);
    }
}
