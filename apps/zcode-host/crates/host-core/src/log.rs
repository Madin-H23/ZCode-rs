//! 日志出口。info / warn / error 经 [`set_sink`] 安装的出口发给 Main（控制通道上的结构化 `log`
//! 消息，由 hostLogRelay 实时落盘）；debug 只在 `ZCODE_HOST_LOG=debug` 时写 stderr，不进生产日志。
//! 出口安装之前（例如解析参数失败）退回 stderr。stdout 只放控制消息，这里从不写 stdout。
use std::fmt::Display;
use std::io::Write;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use zcode_host_control::LogLevel;

pub trait LogSink: Send + Sync {
    fn log(&self, level: LogLevel, message: String);
}

static DEBUG: AtomicBool = AtomicBool::new(false);
static SINK: OnceLock<Box<dyn LogSink>> = OnceLock::new();

pub fn set_debug(enabled: bool) {
    DEBUG.store(enabled, Ordering::Relaxed);
}

/// 只能安装一次；composition root 在控制通道就绪后调用。
pub fn set_sink(sink: Box<dyn LogSink>) {
    let _ = SINK.set(sink);
}

fn stderr_line(level: &str, message: &dyn Display) {
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "[zcode-host] [{level}] {message}");
}

fn emit(level: LogLevel, message: impl Display) {
    match SINK.get() {
        Some(sink) => sink.log(level, message.to_string()),
        None => {
            let name = match level {
                LogLevel::Info => "info",
                LogLevel::Warn => "warn",
                LogLevel::Error => "error",
            };
            stderr_line(name, &message);
        }
    }
}

pub fn debug(message: impl Display) {
    if DEBUG.load(Ordering::Relaxed) {
        stderr_line("debug", &message);
    }
}

pub fn info(message: impl Display) {
    emit(LogLevel::Info, message);
}

pub fn warn(message: impl Display) {
    emit(LogLevel::Warn, message);
}

pub fn error(message: impl Display) {
    emit(LogLevel::Error, message);
}
