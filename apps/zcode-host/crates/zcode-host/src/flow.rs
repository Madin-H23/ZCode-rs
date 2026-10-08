//! 连接发送积压的观测（docs/specs/rust-host-resilience.md §2.3）。
//!
//! 计的是 core 已交给写任务、但还没写进 socket 的字节数。Renderer 读得慢时 TCP 发送缓冲写满，
//! WebSocket `send` 挂起，积压随之增长。越过高水位进入 saturated，回落到低水位以下发 drained，
//! 只在边沿通知；水位与 `PersistentProtocol` 的默认值相同。
use std::sync::Mutex;

use tokio::sync::watch;
use zcode_host_kit::FlowState;

pub const HIGH_WATER_MARK_BYTES: usize = 1024 * 1024;
pub const LOW_WATER_MARK_BYTES: usize = HIGH_WATER_MARK_BYTES / 4;

pub struct FlowGauge {
    high: usize,
    low: usize,
    state: Mutex<(usize, bool)>,
    edges: watch::Sender<FlowState>,
}

impl FlowGauge {
    pub fn new() -> (Self, watch::Receiver<FlowState>) {
        Self::with_marks(HIGH_WATER_MARK_BYTES, LOW_WATER_MARK_BYTES)
    }

    pub fn with_marks(high: usize, low: usize) -> (Self, watch::Receiver<FlowState>) {
        let (edges, receiver) = watch::channel(FlowState::Drained);
        (
            Self {
                high,
                low,
                state: Mutex::new((0, false)),
                edges,
            },
            receiver,
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, (usize, bool)> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 一条消息交给写任务。
    pub fn queued(&self, bytes: usize) {
        let mut state = self.lock();
        state.0 += bytes;
        if !state.1 && state.0 > self.high {
            state.1 = true;
            // 边沿在锁内发出，保证与积压变化同序。
            self.edges.send_replace(FlowState::Saturated);
        }
    }

    /// 一条消息已写进 socket。
    pub fn written(&self, bytes: usize) {
        let mut state = self.lock();
        state.0 = state.0.saturating_sub(bytes);
        if state.1 && state.0 <= self.low {
            state.1 = false;
            self.edges.send_replace(FlowState::Drained);
        }
    }

    #[cfg(test)]
    fn backlog(&self) -> usize {
        self.lock().0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges_fire_once_per_crossing() {
        let (gauge, mut edges) = FlowGauge::with_marks(100, 25);
        gauge.queued(60);
        gauge.queued(40);
        assert!(
            !edges.has_changed().unwrap(),
            "100 bytes is not above the mark"
        );
        gauge.queued(1);
        assert!(edges.has_changed().unwrap());
        assert_eq!(*edges.borrow_and_update(), FlowState::Saturated);
        gauge.queued(500);
        assert!(!edges.has_changed().unwrap(), "no repeated saturated");
        gauge.written(501);
        assert!(
            !edges.has_changed().unwrap(),
            "100 bytes left, still above low"
        );
        gauge.written(75);
        assert_eq!(*edges.borrow_and_update(), FlowState::Drained);
        assert_eq!(gauge.backlog(), 25);
        gauge.written(25);
        assert!(!edges.has_changed().unwrap(), "no repeated drained");
        gauge.written(10);
        assert_eq!(gauge.backlog(), 0);
    }
}
