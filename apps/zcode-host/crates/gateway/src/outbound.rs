//! 每条连接的出站队列（docs/specs/rust-u2-gateway.md §2.3；水位语义见 rust-host-resilience.md §2.3）。
//!
//! 回复与事件按入队顺序排在同一个队列里，transport 逐条取出发送，发完一条调用 [`Outbound::written`]。
//! 积压是已入队、还没写完的字节数（含 transport 已取出、正在写的那一条）：Renderer 读得慢时 TCP 发送
//! 缓冲写满，transport 的写挂起，积压随之增长。越过高水位进入 saturated，回落到低水位及以下发 drained，
//! 只在边沿通知；水位与 `PersistentProtocol` 的默认值相同。越过上限说明客户端已长时间不读，关闭这条连接。
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// Gateway 关闭一条连接的原因（docs/specs/rust-u2-gateway.md §2.6）。第一个触发决定，之后不再改变；
/// transport 据此告诉对端（WebSocket Close 的状态码与 reason）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// 出站积压越过上限：对端长时间不读。Host 仍在运行，对端可以重新接入。
    OutboundBacklog,
    /// `close_attachment`（Main 的 `detach-service-port`）。
    Detached,
    /// 拿不到 channel 表：远端 scope 接入失败，或本地 scope 在 Host 就绪前退出。
    ScopeUnavailable,
    /// channel 表的提供方要求关闭（`Channels::closed`，远端 scope 的会话断开）。
    ScopeClosed,
    /// Host 退出（[`crate::Gateway::shutdown`]）。
    HostShutdown,
    /// transport 释放了 `Inbound`：对端发了 Close 或断开。
    PeerClosed,
    /// transport 写失败（[`Outbound::close`]）。
    TransportFailed,
    /// 连接循环没有走完收口就结束（被中止或 panic）。
    InternalError,
}

impl CloseReason {
    /// 机器可读的短原因（ASCII）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OutboundBacklog => "outbound-backlog",
            Self::Detached => "detached",
            Self::ScopeUnavailable => "scope-unavailable",
            Self::ScopeClosed => "scope-closed",
            Self::HostShutdown => "host-shutdown",
            Self::PeerClosed => "peer-closed",
            Self::TransportFailed => "transport-failed",
            Self::InternalError => "internal-error",
        }
    }
}

/// 连接的关闭开关：每个关闭触发都经它关闭连接并写下原因，第一个写入的原因生效。
#[derive(Clone)]
pub(crate) struct CloseSwitch(Arc<watch::Sender<Option<CloseReason>>>);

impl CloseSwitch {
    pub(crate) fn new() -> Self {
        Self(Arc::new(watch::Sender::new(None)))
    }

    /// 关闭连接；已经关闭时不改变原因。
    pub(crate) fn close(&self, reason: CloseReason) {
        self.0.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(reason);
            true
        });
    }

    pub(crate) fn reason(&self) -> Option<CloseReason> {
        *self.0.borrow()
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<Option<CloseReason>> {
        self.0.subscribe()
    }
}

/// 还在使用中的连接部件：连接循环与 transport 手里的 [`Outbound`]。[`crate::Gateway::shutdown`]
/// 等它们都释放（连接收口完、transport 发完 Close）。
#[derive(Default)]
pub(crate) struct InUse {
    count: AtomicUsize,
    changed: Notify,
}

impl InUse {
    pub(crate) fn hold(self: &Arc<Self>) -> Held {
        self.count.fetch_add(1, Ordering::SeqCst);
        Held(self.clone())
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.count.load(Ordering::SeqCst) == 0
    }

    pub(crate) fn changed(&self) -> tokio::sync::futures::Notified<'_> {
        self.changed.notified()
    }
}

/// [`InUse`] 的一份占用，释放时计数减一并唤醒等待方。
pub(crate) struct Held(Arc<InUse>);

impl Drop for Held {
    fn drop(&mut self) {
        self.0.count.fetch_sub(1, Ordering::SeqCst);
        self.0.changed.notify_waiters();
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
    /// 队列已关闭及其原因。
    closed: Option<CloseReason>,
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
        if state.closed.is_some() {
            return false;
        }
        let backlog = state.backlog + message.len();
        if backlog > self.limits.hard_limit {
            // 原因在队列关闭之前写入开关：transport 取到 None 时一定读得到它。
            self.close.close(CloseReason::OutboundBacklog);
            state.closed = Some(self.close.reason().unwrap_or(CloseReason::OutboundBacklog));
            let dropped = std::mem::take(&mut state.messages);
            drop(state);
            drop(dropped);
            log::warn(format!(
                "connection {} outbound backlog exceeded {} bytes; closing the connection",
                self.connection, self.limits.hard_limit
            ));
            self.ready.notify_one();
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
    /// 原因取关闭开关上第一个触发写下的（调用方先写开关）；队列已经关闭时保持原来的原因。
    pub(crate) fn close(&self) {
        let reason = self.close.reason().unwrap_or(CloseReason::InternalError);
        let dropped = {
            let mut state = self.lock();
            state.closed.get_or_insert(reason);
            std::mem::take(&mut state.messages)
        };
        drop(dropped);
        self.ready.notify_one();
    }

    pub(crate) fn backlog(&self) -> usize {
        self.lock().backlog
    }
}

/// transport 侧的出站队列：取出要发送的消息，发完一条报告一次。释放它表示 transport 已结束
/// （[`crate::Gateway::shutdown`] 等它）。
pub struct Outbound {
    pub(crate) queue: Arc<Queue>,
    _held: Held,
}

impl Outbound {
    pub(crate) fn new(queue: Arc<Queue>, held: Held) -> Self {
        Self { queue, _held: held }
    }

    /// 下一条要发送的消息；连接关闭后返回 None（还没发送的消息随连接一起丢弃）。只能有一个消费者。
    pub async fn next(&self) -> Option<Vec<u8>> {
        loop {
            {
                let mut state = self.queue.lock();
                if let Some(message) = state.messages.pop_front() {
                    return Some(message);
                }
                if state.closed.is_some() {
                    return None;
                }
            }
            // `notify_one` 在没有等待者时留下一个许可，检查与等待之间到达的消息不会被错过。
            self.queue.ready.notified().await;
        }
    }

    /// 一条 `bytes` 字节的消息已写出。
    pub fn written(&self, bytes: usize) {
        let mut state = self.queue.lock();
        state.backlog = state.backlog.saturating_sub(bytes);
        if state.saturated && state.backlog <= self.queue.limits.low_water {
            state.saturated = false;
            self.queue.edges.send_replace(FlowState::Drained);
        }
    }

    /// transport 无法再写（socket 写失败）：关闭这条连接。
    pub fn close(&self) {
        self.queue.close.close(CloseReason::TransportFailed);
        self.queue.close();
    }

    /// 出站队列关闭的原因（docs/specs/rust-u2-gateway.md §2.6）；[`Self::next`] 返回 None 之后一定是
    /// Some。transport 据此告诉对端为什么关闭。
    pub fn close_reason(&self) -> Option<CloseReason> {
        self.queue.lock().closed
    }
}
