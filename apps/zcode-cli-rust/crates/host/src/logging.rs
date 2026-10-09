//! JSONL file logging compatible with the Node CLI log format.
//!
//! Runtime code logs through `tracing` macros with target prefix `zcode`. This
//! subscriber formats events like Node `adapters/logging/serialize.ts`, redacts
//! credential-like keys and secret-looking values (`zcode_kit::redact`), and
//! hands lines to a bounded queue drained by one writer thread. When the queue
//! is full the line is dropped and counted; the count is logged once at
//! shutdown. A logging stall never blocks the runtime. stdout is never written.
//! Log files are created owner-only (0600) on Unix.
//!
//! The global subscriber keeps a queue sender for the process lifetime, so the
//! writer thread never observes a closed channel. Shutdown therefore flushes
//! with an explicit marker and a bounded wait instead of joining the thread.
use serde_json::{Map, Value, json};
use std::{
    ffi::OsString,
    fmt::Write as _,
    io::Write as _,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
    },
    time::Duration,
};
use tracing::{
    Event, Level, Metadata, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};
use zcode_kit::redact::redact_secrets;

const QUEUE_LINES: usize = 4096;
/// Upper bound for flushing queued lines at shutdown.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(1);

enum Message {
    Line(String),
    /// Everything queued before this marker has been written when the ack fires.
    Flush(SyncSender<()>),
}
/// Node `serialize.ts` redaction rule for object keys.
const REDACTED_KEYS: &[&str] = &[
    "apikey",
    "api-key",
    "api_key",
    "authorization",
    "cookie",
    "credential",
    "password",
    "secret",
    "token",
];

pub struct LogOptions {
    pub directory: PathBuf,
    pub level: Level,
    pub console: bool,
}

impl LogOptions {
    /// `ZCODE_LOG_DIR` or `~/.zcode/cli/log`; debug in development, info otherwise.
    /// `env` looks the variables up in the caller's environment (the binary
    /// passes the process environment; the crate never reads it).
    pub fn from_env(home: &std::path::Path, env: impl Fn(&str) -> Option<OsString>) -> Self {
        let directory = env("ZCODE_LOG_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".zcode/cli/log"));
        let development = env("ZCODE_RUNTIME_ENV").is_some_and(|v| v == "development");
        Self {
            directory,
            level: if development {
                Level::DEBUG
            } else {
                Level::INFO
            },
            console: env("ZCODE_LOG_CONSOLE").is_some_and(|v| v == "1"),
        }
    }
}

/// Dropping the guard flushes queued lines (bounded by [`FLUSH_TIMEOUT`]).
pub struct LogGuard {
    tx: SyncSender<Message>,
    dropped: Arc<AtomicU64>,
}

impl LogGuard {
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        let (ack, done) = sync_channel(1);
        match self.tx.try_send(Message::Flush(ack)) {
            Ok(()) => {}
            // 队列满时由临时线程阻塞等待空位，本线程仍最多等 FLUSH_TIMEOUT；这正是丢过行的情形，
            // 不能因为排不进 Flush 就漏掉丢弃计数。写线程卡在 IO 时进程照常退出。
            Err(TrySendError::Full(flush)) => {
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(flush);
                });
            }
            Err(TrySendError::Disconnected(_)) => return,
        }
        // 队列满或写线程卡在 IO 时最多等待 FLUSH_TIMEOUT，日志不能阻止进程退出。
        let _ = done.recv_timeout(FLUSH_TIMEOUT);
    }
}

/// Install the process-wide JSONL subscriber. Returns `None` if one is already installed.
pub fn init(options: LogOptions) -> Option<LogGuard> {
    let (tx, rx) = sync_channel::<Message>(QUEUE_LINES);
    let dropped = Arc::new(AtomicU64::new(0));
    let subscriber = JsonLines {
        level: options.level,
        tx: tx.clone(),
        dropped: dropped.clone(),
    };
    tracing::subscriber::set_global_default(subscriber).ok()?;
    let writer = Writer {
        directory: options.directory,
        console: options.console,
        current: None,
        dropped: dropped.clone(),
    };
    std::thread::spawn(move || writer.run(rx));
    Some(LogGuard { tx, dropped })
}

/// The writer thread: one daily file `zcode-rust-<date>.jsonl`, opened lazily.
struct Writer {
    directory: PathBuf,
    console: bool,
    current: Option<(String, std::fs::File)>,
    dropped: Arc<AtomicU64>,
}

impl Writer {
    fn run(mut self, rx: Receiver<Message>) {
        while let Ok(message) = rx.recv() {
            match message {
                Message::Line(line) => self.write(&line),
                Message::Flush(ack) => {
                    self.report_dropped();
                    let _ = ack.send(());
                }
            }
        }
    }

    /// 被丢弃的行不会出现在文件里；不记一笔，读日志的人无从知道日志不完整。只在退出前的 Flush 写一次。
    fn report_dropped(&mut self) {
        let dropped = self.dropped.load(Ordering::Relaxed);
        if dropped == 0 {
            return;
        }
        let mut fields = Fields {
            message: "Log lines were dropped because the log queue was full".into(),
            ..Fields::default()
        };
        fields
            .top
            .insert("event".into(), "logging.lines_dropped".into());
        fields.context.insert("dropped".into(), dropped.into());
        self.write(&format_line(Level::WARN, "zcode::logging", fields));
    }

    fn write(&mut self, line: &str) {
        if self.console {
            let _ = writeln!(std::io::stderr().lock(), "{line}");
        }
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        if self.current.as_ref().is_none_or(|(day, _)| *day != date) {
            self.current = self.open(&date).map(|file| (date, file));
        }
        if let Some((_, file)) = &mut self.current {
            // 与 Node 一致：写失败只丢弃该行，不影响 runtime。
            let _ = file.write_all(format!("{line}\n").as_bytes());
        }
    }

    /// 日志可能带有路径、命令与错误详情，只给属主读写：新文件以 0600 创建，旧版本留下的文件
    /// 打开时收紧到 0600（尽力而为）。
    fn open(&self, date: &str) -> Option<std::fs::File> {
        let _ = std::fs::create_dir_all(&self.directory);
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let file = options
            .open(self.directory.join(format!("zcode-rust-{date}.jsonl")))
            .ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        Some(file)
    }
}

struct JsonLines {
    level: Level,
    tx: SyncSender<Message>,
    dropped: Arc<AtomicU64>,
}

fn redacted(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    REDACTED_KEYS.iter().any(|needle| key.contains(needle))
}

/// Promoted to top-level line fields (Node reserved keys); everything else goes to `context`.
fn reserved(key: &str) -> Option<&'static str> {
    Some(match key {
        "event" => "event",
        "module" => "module",
        "trace_id" | "traceId" => "traceId",
        "session_id" | "sessionId" => "sessionId",
        "turn_id" | "turnId" => "turnId",
        "tool_call_id" | "toolCallId" => "toolCallId",
        "duration_ms" | "durationMs" => "durationMs",
        "status" => "status",
        "error" => "error",
        _ => return None,
    })
}

#[derive(Default)]
struct Fields {
    message: String,
    top: Map<String, Value>,
    context: Map<String, Value>,
}

impl Fields {
    fn put(&mut self, field: &Field, value: Value) {
        let name = field.name();
        if name == "message" {
            self.message = value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_owned);
        } else if let Some(key) = reserved(name) {
            self.top.insert(key.into(), value);
        } else if redacted(name) {
            self.context.insert(name.into(), "[Redacted]".into());
        } else {
            self.context.insert(name.into(), value);
        }
    }
}

/// Values leave the process in the log file: secret-looking substrings are
/// replaced even when the field name looks harmless (e.g. an error message
/// that quotes a request header).
fn clean(text: &str) -> Value {
    Value::String(redact_secrets(text).into_owned())
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let mut text = String::new();
        let _ = write!(text, "{value:?}");
        self.put(field, clean(&text));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, clean(value));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, value.into());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, value.into());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, value.into());
    }
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.put(field, json!({"message": clean(&value.to_string())}));
    }
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::ERROR => "error",
        Level::WARN => "warn",
        Level::INFO => "info",
        _ => "debug",
    }
}

fn format_line(level: Level, target: &str, fields: Fields) -> String {
    let mut line = Map::new();
    line.insert(
        "timestamp".into(),
        chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string()
            .into(),
    );
    line.insert("level".into(), level_name(level).into());
    line.insert("module".into(), target.into());
    line.insert("message".into(), fields.message.into());
    line.extend(fields.top);
    if !fields.context.is_empty() {
        line.insert("context".into(), Value::Object(fields.context));
    }
    Value::Object(line).to_string()
}

impl Subscriber for JsonLines {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        // 只记录本仓库的事件；依赖库（hyper、rmcp 等）的内部事件不进入产品日志。
        *metadata.level() <= self.level && metadata.target().starts_with("zcode")
    }
    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(self.level.into())
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let metadata = event.metadata();
        let line = format_line(*metadata.level(), metadata.target(), fields);
        if let Err(TrySendError::Full(_)) = self.tx.try_send(Message::Line(line)) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

#[cfg(test)]
#[path = "logging_tests.rs"]
mod tests;
