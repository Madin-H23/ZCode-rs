//! The App Server's one output, shared by every delivery connection.
//!
//! Each event-loop step collects its lines here in production order (replies,
//! host requests and delivery frames interleaved) and writes them to the
//! [`crate::Sink`] once when the step ends. The stdout backlog is one for the
//! whole process, so congestion is too: the front-end raises it above the
//! high watermark and clears it below the low one, and every connection's
//! [`ConnectionSink::paused`] reads it (spec rust-u3-stage-b §3.2).
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use zcode_cli_delivery::{ConnectionSink, FrameRoute, notification};

#[derive(Default)]
pub(super) struct Outlet {
    lines: Mutex<Vec<String>>,
    congested: AtomicBool,
}

impl Outlet {
    pub(super) fn line(&self, line: String) {
        self.lines().push(line);
    }

    /// The lines of the step, leaving the outlet empty.
    pub(super) fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.lines())
    }

    pub(super) fn congested(&self) -> bool {
        self.congested.load(Ordering::Relaxed)
    }

    pub(super) fn congest(&self) {
        self.congested.store(true, Ordering::Relaxed);
    }

    pub(super) fn relieve(&self) {
        self.congested.store(false, Ordering::Relaxed);
    }

    fn lines(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        // 只有事件循环一个使用方，且临界区内不会 panic。
        self.lines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// stdio 只有一条输出，所有连接共用，不按路由分发。
impl ConnectionSink for Outlet {
    fn push(&self, _route: FrameRoute<'_>, frames: Vec<String>) {
        self.lines()
            .extend(frames.iter().map(|wire| notification(wire)));
    }

    fn paused(&self) -> bool {
        self.congested()
    }
}
