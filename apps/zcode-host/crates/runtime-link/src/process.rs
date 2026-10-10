//! CLI 子进程的启动与进程树回收（zcodeAgentProcessManager.ts startClient、zcodeStdioTransport.ts
//! disposeAndWaitOnce、agentStderrCollector.ts）：子进程链路的基础，远程连接器进程也用它。
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
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use zcode_host_kit::log;

const GRACEFUL_EXIT_WAIT: Duration = Duration::from_millis(1800);
const TERMINATE_WAIT: Duration = Duration::from_secs(2);
const STDERR_TAIL_LINES: usize = 20;
const STDERR_LINE_MAX_CHARS: usize = 1_000;

pub struct Spawned {
    pub child: Child,
    pub pid: Option<u32>,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

/// 启动子进程：自成进程组，环境只用 `env`，三个 stdio 都接管道。
pub fn spawn(
    program: &Path,
    args: &[String],
    cwd: &Path,
    env: &BTreeMap<String, String>,
) -> std::io::Result<Spawned> {
    let mut process = zcode_proc::command(program);
    process
        .args(args)
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

/// Node `errno.code` 的常见取值（spawn 失败上报 `agent-process-error.errorCode`）。
pub fn errno_code(error: &std::io::Error) -> Option<&'static str> {
    use std::io::ErrorKind;
    Some(match error.kind() {
        ErrorKind::NotFound => "ENOENT",
        ErrorKind::PermissionDenied => "EACCES",
        ErrorKind::AlreadyExists => "EEXIST",
        ErrorKind::InvalidInput => "EINVAL",
        ErrorKind::OutOfMemory => "ENOMEM",
        ErrorKind::NotADirectory => "ENOTDIR",
        _ => return None,
    })
}

/// 进程被信号结束时的信号名（Node `exit` 事件的 signal）。
pub fn exit_signal_name(status: std::process::ExitStatus) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        let signal = status.signal()?;
        let name = match signal {
            libc::SIGHUP => "SIGHUP",
            libc::SIGINT => "SIGINT",
            libc::SIGQUIT => "SIGQUIT",
            libc::SIGILL => "SIGILL",
            libc::SIGABRT => "SIGABRT",
            libc::SIGBUS => "SIGBUS",
            libc::SIGFPE => "SIGFPE",
            libc::SIGKILL => "SIGKILL",
            libc::SIGSEGV => "SIGSEGV",
            libc::SIGPIPE => "SIGPIPE",
            libc::SIGTERM => "SIGTERM",
            other => return Some(format!("SIG{other}")),
        };
        Some(name.to_owned())
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

/// stderr 的最后若干行（已脱敏、截断）与总行数（agentStderrCollector / createAgentStderrTail）。
#[derive(Clone, Default)]
pub struct StderrTail(Arc<Mutex<TailState>>);

#[derive(Default)]
struct TailState {
    lines: VecDeque<String>,
    count: u64,
}

impl StderrTail {
    pub fn lines(&self) -> Vec<String> {
        self.lock().lines.iter().cloned().collect()
    }

    /// 进入 tail 的总行数（进程诊断行不计入）。
    pub fn line_count(&self) -> u64 {
        self.lock().count
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TailState> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn push(&self, line: String) {
        let mut state = self.lock();
        state.count += 1;
        if state.lines.len() == STDERR_TAIL_LINES {
            state.lines.pop_front();
        }
        let line = if line.chars().count() > STDERR_LINE_MAX_CHARS {
            let mut bounded: String = line.chars().take(STDERR_LINE_MAX_CHARS).collect();
            bounded.push_str("…[truncated]");
            bounded
        } else {
            line
        };
        state.lines.push_back(line);
    }
}

fn secret_patterns() -> &'static [Regex; 3] {
    static PATTERNS: std::sync::OnceLock<[Regex; 3]> = std::sync::OnceLock::new();
    PATTERNS.get_or_init(|| {
        let compile = |pattern: &str| {
            Regex::new(pattern).unwrap_or_else(|_| unreachable!("static pattern"))
        };
        [
            // Node AGENT_STDERR_SENSITIVE_ASSIGNMENT_PATTERN；另外吞掉值前的 Bearer/Basic，
            // 否则 `Authorization: Bearer x` 只遮住 scheme 名，令牌本身仍会进日志。
            compile(
                r#"(?i)(["']?(?:api[-_]?key|authorization|cookie|credential|password|secret|token)["']?\s*[:=]\s*)(?:(?:bearer|basic)\s+)?(?:"[^"]*"|'[^']*'|[^\s,;}]+)"#,
            ),
            compile(r"(?i)\b(Bearer|Basic)\s+[A-Za-z0-9._~+/=-]+"),
            compile(r"(?i)\b(sk-)[A-Za-z0-9_-]{16,}\b"),
        ]
    })
}

/// redactAgentDiagnostic：stderr tail 与进程诊断上报 Main 前的脱敏。
fn redact(line: &str) -> String {
    let [assignment, scheme, api_key] = secret_patterns();
    let line = assignment.replace_all(line, "${1}<redacted>");
    let line = scheme.replace_all(&line, "${1} <redacted>");
    api_key.replace_all(&line, "${1}<redacted>").into_owned()
}

const DIAGNOSTIC_PREFIX: &str = "[zcode-process-exception] ";
const DIAGNOSTIC_MAX_LINE_CHARS: usize = 128 * 1024;
const DIAGNOSTIC_NAME_MAX_CHARS: usize = 128;
const DIAGNOSTIC_MESSAGE_MAX_CHARS: usize = 4_000;
const DIAGNOSTIC_STACK_MAX_CHARS: usize = 16_000;

/// JS 字符串长度按 UTF-16 码元计。
fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn truncate_utf16(value: &str, max: usize) -> String {
    let mut units = 0;
    let mut out = String::new();
    for ch in value.chars() {
        units += ch.len_utf16();
        if units > max {
            break;
        }
        out.push(ch);
    }
    out
}

fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
        && matches!(bytes[14], b'1'..=b'8')
        && matches!(bytes[19], b'8' | b'9' | b'a' | b'b' | b'A' | b'B')
}

/// parseZCodeProcessDiagnostic + 上报前的脱敏与截断；不是诊断行时返回 None。
fn parse_diagnostic(line: &str) -> Option<serde_json::Value> {
    use serde_json::Value;
    let body = line.strip_prefix(DIAGNOSTIC_PREFIX)?;
    if utf16_len(line) > DIAGNOSTIC_MAX_LINE_CHARS {
        return None;
    }
    let Value::Object(mut object) = serde_json::from_str::<Value>(body).ok()? else {
        return None;
    };
    const KEYS: &[&str] = &[
        "version",
        "errorId",
        "kind",
        "origin",
        "name",
        "message",
        "stack",
        "occurredAt",
    ];
    if object.keys().any(|key| !KEYS.contains(&key.as_str())) {
        return None;
    }
    let kind_ok = |value: Option<&Value>| {
        matches!(
            value.and_then(Value::as_str),
            Some("uncaughtException" | "unhandledRejection")
        )
    };
    let version_ok = object.get("version").and_then(Value::as_f64) == Some(1.0);
    let error_id_ok = object
        .get("errorId")
        .and_then(Value::as_str)
        .is_some_and(is_uuid);
    let name_ok = object
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(|name| (1..=DIAGNOSTIC_NAME_MAX_CHARS).contains(&utf16_len(name)));
    let message_ok = object
        .get("message")
        .and_then(Value::as_str)
        .is_some_and(|message| utf16_len(message) <= DIAGNOSTIC_MESSAGE_MAX_CHARS);
    let stack_ok = match object.get("stack") {
        None => true,
        Some(stack) => stack
            .as_str()
            .is_some_and(|stack| utf16_len(stack) <= DIAGNOSTIC_STACK_MAX_CHARS),
    };
    let occurred_ok = object
        .get("occurredAt")
        .and_then(Value::as_f64)
        .is_some_and(|at| at >= 0.0 && at.fract() == 0.0 && at <= 9_007_199_254_740_991.0);
    if !(version_ok
        && error_id_ok
        && kind_ok(object.get("kind"))
        && kind_ok(object.get("origin"))
        && name_ok
        && message_ok
        && stack_ok
        && occurred_ok)
    {
        return None;
    }
    // 脱敏占位符可能比原文长，必须再次限长，否则 Main 的 schema 会拒绝合法异常。
    for (key, max) in [
        ("name", DIAGNOSTIC_NAME_MAX_CHARS),
        ("message", DIAGNOSTIC_MESSAGE_MAX_CHARS),
        ("stack", DIAGNOSTIC_STACK_MAX_CHARS),
    ] {
        if let Some(Value::String(text)) = object.get(key) {
            let bounded = truncate_utf16(&redact(text), max);
            object.insert(key.to_owned(), Value::String(bounded));
        }
    }
    Some(Value::Object(object))
}

pub type DiagnosticSink = Box<dyn Fn(serde_json::Value) + Send + Sync>;

/// stderr 排空完成的信号：退出事件要等最后几行诊断收齐。
pub type StderrDone = tokio::sync::oneshot::Receiver<()>;

/// 排空 stderr 的读循环：调用方在拥有这个子进程的作用域里运行它（docs/specs/rust-u2-task-scope.md
/// §6.1），不运行它，管道写满后子进程会阻塞。
pub type StderrDrain = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// 一直读到 EOF，避免管道写满后子进程阻塞。进程诊断行交给 `on_diagnostic`，不进 tail。返回的
/// [`StderrDrain`] 由调用方在子进程所有者的作用域里运行；作用域关闭时读循环随之结束。
pub fn drain_stderr(
    stderr: Option<ChildStderr>,
    label: String,
    on_diagnostic: Option<DiagnosticSink>,
) -> (StderrTail, StderrDone, StderrDrain) {
    let tail = StderrTail::default();
    let collected = tail.clone();
    let (done, done_rx) = tokio::sync::oneshot::channel();
    // 嵌入式 runtime 没有 stderr（它的日志走 tracing）：tail 为空，完成信号立即发出。
    let Some(stderr) = stderr else {
        let _ = done.send(());
        return (tail, done_rx, Box::pin(async {}));
    };
    let drain = Box::pin(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            if let Some(diagnostic) = parse_diagnostic(&line) {
                match &on_diagnostic {
                    Some(sink) => sink(diagnostic),
                    None => log::debug(format!("{label} process diagnostic dropped")),
                }
                continue;
            }
            let line = redact(&line);
            log::debug(format!("{label} stderr: {line}"));
            collected.push(line);
        }
        let _ = done.send(());
    });
    (tail, done_rx, drain)
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
    let mut command = zcode_proc::command("taskkill");
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
            "request failed: Authorization: <redacted>"
        );
        assert_eq!(redact("apiKey=sk-123 ok"), "apiKey=<redacted> ok");
        assert_eq!(redact(r#"{"token": "xyz"}"#), r#"{"token": <redacted>}"#);
        assert_eq!(redact("got Bearer abc"), "got Bearer <redacted>");
        assert_eq!(
            redact("key sk-abcdefghijklmnop1234 end"),
            "key sk-<redacted> end"
        );
        assert_eq!(redact("plain line"), "plain line");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminate_escalates_when_stdin_eof_is_ignored() {
        let args = ["-c".to_owned(), "trap '' TERM; sleep 30".to_owned()];
        let env = BTreeMap::from([("PATH".to_owned(), "/usr/bin:/bin".to_owned())]);
        let mut spawned = spawn(Path::new("/bin/sh"), &args, Path::new("/"), &env).unwrap();
        drop(spawned.stdin);
        let started = std::time::Instant::now();
        terminate(&mut spawned.child, spawned.pid).await;
        assert!(spawned.child.try_wait().unwrap().is_some());
        assert!(started.elapsed() >= GRACEFUL_EXIT_WAIT + TERMINATE_WAIT);
    }
}
