//! 每条连接的出站队列（docs/specs/rust-u2-gateway.md §2.3；水位语义见 rust-host-resilience.md §2.3）。
//!
//! 回复与事件按入队顺序排在同一个队列里，transport 逐条取出发送，发完一条调用 [`Outbound::written`]。
//! 积压是已入队、还没写完的字节数（含 transport 已取出、正在写的那一条）：Renderer 读得慢时 TCP 发送
//! 缓冲写满，transport 的写挂起，积压随之增长。越过高水位进入 saturated，回落到低水位及以下发 drained，
//! 只在边沿通知；水位与 `PersistentProtocol` 的默认值相同。越过上限说明客户端已长时间不读，关闭这条连接。
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::{Notify, watch};
use zcode_host_kit::{FlowState, log};

/// 出站队列的阈值（字节）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutboundLimits {
    /// 积压越过它时通知各 channel `saturated`。
    pub high_water: usize,
    /// saturated 之后积压回落到它及以下时通知 `drained`。
    pub low_water: usize,
    /// 积压越过它时关闭这条连接。
    pub hard_limit: usize,
}

impl Default for OutboundLimits {
    fn default() -> Self {
        Self {
            high_water: 1024 * 1024,
            low_water: 256 * 1024,
            hard_limit: 256 * 1024 * 1024,
        }
    }
}

/// 连接的关闭开关：Gateway 的 `close_attachment`、出站队列越过上限、transport 写失败都经它关闭连接。
#[derive(Clone)]
pub(crate) struct CloseSwitch(Arc<watch::Sender<bool>>);

impl CloseSwitch {
    pub(crate) fn new() -> Self {
        Self(Arc::new(watch::Sender::new(false)))
    }

    pub(crate) fn close(&self) {
        self.0.send_replace(true);
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<bool> {
        self.0.subscribe()
    }
}

pub(crate) struct Queue {
    connection: u64,
    limits: OutboundLimits,
    state: Mutex<State>,
    /// 唯一的消费者（transport 的写循环）在这里等待新消息或关闭。
    ready: Notify,
    edges: watch::Sender<FlowState>,
    close: CloseSwitch,
}

#[derive(Default)]
struct State {
    messages: VecDeque<Vec<u8>>,
    backlog: usize,
    saturated: bool,
    closed: bool,
}

impl Queue {
    /// 返回队列与水位边沿的接收端（连接循环在那里把边沿交给各 channel）。
    pub(crate) fn new(
        connection: u64,
        limits: OutboundLimits,
        close: CloseSwitch,
    ) -> (Arc<Self>, watch::Receiver<FlowState>) {
        let (edges, receiver) = watch::channel(FlowState::Drained);
        let queue = Arc::new(Self {
            connection,
            limits,
            state: Mutex::default(),
            ready: Notify::new(),
            edges,
            close,
        });
        (queue, receiver)
    }

    /// 锁只保护队列本身，持有期间不 await。
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 入队一条消息。连接已关闭时丢弃并返回 false。
    pub(crate) fn push(&self, message: Vec<u8>) -> bool {
        let mut state = self.lock();
        if state.closed {
            return false;
        }
        let backlog = state.backlog + message.len();
        if backlog > self.limits.hard_limit {
            state.closed = true;
            let dropped = std::mem::take(&mut state.messages);
            drop(state);
            drop(dropped);
            log::warn(format!(
                "connection {} outbound backlog exceeded {} bytes; closing the connection",
                self.connection, self.limits.hard_limit
            ));
            self.ready.notify_one();
            self.close.close();
            return false;
        }
        state.backlog = backlog;
        state.messages.push_back(message);
        if !state.saturated && state.backlog > self.limits.high_water {
            state.saturated = true;
            // 边沿在锁内发出，保证与积压变化同序。
            self.edges.send_replace(FlowState::Saturated);
        }
        drop(state);
        self.ready.notify_one();
        true
    }

    /// 连接关闭：不再接受消息，尚未发送的消息丢弃，transport 的 [`Outbound::next`] 随之返回 None。
    pub(crate) fn close(&self) {
        let dropped = {
            let mut state = self.lock();
            state.closed = true;
            std::mem::take(&mut state.messages)
        };
        drop(dropped);
        self.ready.notify_one();
    }

    pub(crate) fn backlog(&self) -> usize {
        self.lock().backlog
    }
}

/// transport 侧的出站队列：取出要发送的消息，发完一条报告一次。
pub struct Outbound(pub(crate) Arc<Queue>);

impl Outbound {
    /// 下一条要发送的消息；连接关闭后返回 None（还没发送的消息随连接一起丢弃）。只能有一个消费者。
    pub async fn next(&self) -> Option<Vec<u8>> {
        loop {
            {
                let mut state = self.0.lock();
                if let Some(message) = state.messages.pop_front() {
                    return Some(message);
                }
                if state.closed {
                    return None;
                }
            }
            // `notify_one` 在没有等待者时留下一个许可，检查与等待之间到达的消息不会被错过。
            self.0.ready.notified().await;
        }
    }

    /// 一条 `bytes` 字节的消息已写出。
    pub fn written(&self, bytes: usize) {
        let mut state = self.0.lock();
        state.backlog = state.backlog.saturating_sub(bytes);
        if state.saturated && state.backlog <= self.0.limits.low_water {
            state.saturated = false;
            self.0.edges.send_replace(FlowState::Drained);
        }
    }

    /// transport 无法再写（socket 写失败）：关闭这条连接。
    pub fn close(&self) {
        self.0.close();
        self.0.close.close();
    }
}
