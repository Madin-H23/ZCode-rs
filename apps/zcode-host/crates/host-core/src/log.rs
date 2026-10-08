//! 日志写 stderr（stdout 只放控制消息）。Main 经 `hostLogRelay.onStderr` 落盘。
//! `ZCODE_HOST_LOG=debug` 打开 debug；该值由 composition root 读取后经 [`set_debug`] 传入。
use std::fmt::Display;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

static DEBUG: AtomicBool = AtomicBool::new(false);

pub fn set_debug(enabled: bool) {
    DEBUG.store(enabled, Ordering::Relaxed);
}

fn emit(level: &str, message: impl Display) {
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "[zcode-host] [{level}] {message}");
}

pub fn debug(message: impl Display) {
    if DEBUG.load(Ordering::Relaxed) {
        emit("debug", message);
    }
}

pub fn info(message: impl Display) {
    emit("info", message);
}

pub fn warn(message: impl Display) {
    emit("warn", message);
}

pub fn error(message: impl Display) {
    emit("error", message);
}
