//! 日志出口。info / warn / error 经 [`set_sink`] 安装的出口发给 Main（控制通道上的结构化 `log`
//! 消息，由 hostLogRelay 实时落盘）；debug 只在 `ZCODE_HOST_LOG=debug` 时写 stderr，不进生产日志。
//! 出口安装之前（例如解析参数失败）退回 stderr。stdout 只放控制消息，这里从不写 stdout。
//!
//! 所有离开进程的文本（发给 Main 落盘的日志与 stderr）都先经 `zcode_kit::redact::redact_secrets`
//! 按值脱敏：日志消息常由错误信息拼成，可能带上 provider 返回的 key 或请求里的 Bearer token。
//! Host 自己不创建日志文件（由 Main 落盘），所以没有文件权限要设置。
use std::borrow::Cow;
use std::fmt::Display;
use std::io::Write;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use zcode_host_control::LogLevel;
use zcode_kit::redact::redact_secrets;

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

fn redacted(message: impl Display) -> String {
    let message = message.to_string();
    match redact_secrets(&message) {
        Cow::Borrowed(_) => message,
        Cow::Owned(redacted) => redacted,
    }
}

fn stderr_line(level: &str, message: &str) {
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "[zcode-host] [{level}] {message}");
}

fn emit(level: LogLevel, message: impl Display) {
    let message = redacted(message);
    match SINK.get() {
        Some(sink) => sink.log(level, message),
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
        stderr_line("debug", &redacted(message));
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

#[cfg(test)]
#[path = "log_tests.rs"]
mod tests;
