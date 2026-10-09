//! Gateway：Host 的连接层（docs/specs/rust-u2-gateway.md）。
//!
//! transport（今天是 WebSocket）完成握手与鉴权后，把一条已鉴权的连接交给 [`Gateway::connect`]：
//! 它写入收到的完整 rpc 消息（[`Inbound`]），取出要发送的消息（[`Outbound`]）。Gateway 负责
//!
//! - 连接注册表：每条连接的 id、attachment 元数据、任务作用域与关闭开关；
//! - 每条连接的 `@zcode/rpc` 服务端循环（`connection.rs`）；
//! - 每条连接一个按字节计数的出站队列与积压水位（`outbound.rs`）；
//! - Host 级的串行键通道（`lanes.rs`）。
//!
//! 它只认识 [`zcode_host_kit::ServiceChannel`]，不认识任何业务 service；channel 表由组装层经 [`ChannelSource`] 给出。
#![deny(clippy::unwrap_used, clippy::expect_used)]
mod connection;
mod lanes;
mod outbound;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::watch;
use zcode_async::Spawner;
use zcode_host_control::{AttachmentScope, ClientMode, WindowId};
use zcode_host_kit::ChannelRegistry;
use zcode_host_rpc::RpcError;

pub use connection::{Connection, ConnectionClosed, Inbound};
pub use outbound::{Outbound, OutboundLimits};

use lanes::Lanes;
use outbound::{CloseSwitch, Queue};

/// 签发 capability 时登记的 attachment 元数据；连接建立后成为它的可信上下文（`CallContext`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentMeta {
    pub attachment_id: String,
    pub client_mode: ClientMode,
    pub scope: AttachmentScope,
    /// 签发时 Main 给的主窗口 id；连接建立后进入 `CallContext.window_id`（rust-u4-app-host.md §2.2）。
    pub window_id: Option<WindowId>,
}

/// 一条连接使用的 channel 表。
pub struct Channels {
    pub registry: Arc<ChannelRegistry>,
    /// 表的提供方要求关闭连接的信号（远端 scope 的会话断开）；置 true 时 Gateway 关闭这条连接。
    pub closed: Option<watch::Receiver<bool>>,
}

/// 连接的 channel 表来源：本地 scope 等 Host 就绪，远端 scope 等对应 workspace 接入。
/// 返回 None 时连接不发 `[200]` 就关闭。
pub type ChannelSource = Pin<Box<dyn Future<Output = Option<Channels>> + Send>>;

/// 调用结束时的观测（RPC 网络遥测）。
///
/// 实现方在调用任务所在的线程上被同步调用，不能阻塞。`elapsed` 从请求到达 Gateway 起算，含排队时间。
pub trait CallObserver: Send + Sync {
    fn call_finished(
        &self,
        channel: &str,
        method: &str,
        elapsed: Duration,
        error: Option<&RpcError>,
    );
}

/// 注册表里一条连接的诊断信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionInfo {
    pub id: u64,
    pub meta: AttachmentMeta,
    /// 出站队列的积压字节数。
    pub backlog: usize,
}

#[derive(Clone)]
pub struct Gateway {
    shared: Arc<Shared>,
}

pub(crate) struct Shared {
    /// 连接循环所在的作用域；每条连接的作用域是它的子作用域 `connection:<id>`。
    root: Spawner,
    lanes: Arc<Lanes>,
    observer: Arc<dyn CallObserver>,
    limits: OutboundLimits,
    next_connection: AtomicU64,
    connections: Mutex<HashMap<u64, Entry>>,
}

struct Entry {
    meta: AttachmentMeta,
    close: CloseSwitch,
    queue: Arc<Queue>,
}

impl Shared {
    /// 注册表只做插入、删除与读取，持锁期间不 await。
    fn connections(&self) -> MutexGuard<'_, HashMap<u64, Entry>> {
        self.connections
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl Gateway {
    /// `root`：连接循环所在的作用域，通常是 Host 的根作用域；连接的作用域 `<root>/connection:<id>`
    /// 由它派生，随它关闭。
    pub fn new(root: Spawner, observer: Arc<dyn CallObserver>, limits: OutboundLimits) -> Self {
        Self {
            shared: Arc::new(Shared {
                root,
                lanes: Arc::default(),
                observer,
                limits,
                next_connection: AtomicU64::new(1),
                connections: Mutex::default(),
            }),
        }
    }

    /// 接入一条已鉴权的连接。transport 把收到的完整 rpc 消息写进 [`Connection::inbound`]，从
    /// [`Connection::outbound`] 取出要发送的消息，每发完一条调用 [`Outbound::written`]；
    /// 结束读取时释放 `inbound`。
    pub fn connect(&self, meta: AttachmentMeta, channels: ChannelSource) -> Connection {
        connection::connect(&self.shared, meta, channels)
    }

    /// 关闭某个 attachment 的全部连接（`detach-service-port`）。返回被关闭的连接数。
    pub fn close_attachment(&self, attachment_id: &str) -> usize {
        let closers: Vec<CloseSwitch> = self
            .shared
            .connections()
            .values()
            .filter(|entry| entry.meta.attachment_id == attachment_id)
            .map(|entry| entry.close.clone())
            .collect();
        for closer in &closers {
            closer.close();
        }
        closers.len()
    }

    /// 当前的连接（按 id 排序），用于诊断。
    pub fn connections(&self) -> Vec<ConnectionInfo> {
        let mut connections: Vec<ConnectionInfo> = self
            .shared
            .connections()
            .iter()
            .map(|(id, entry)| ConnectionInfo {
                id: *id,
                meta: entry.meta.clone(),
                backlog: entry.queue.backlog(),
            })
            .collect();
        connections.sort_by_key(|info| info.id);
        connections
    }
}

#[cfg(test)]
#[path = "gateway_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "outbound_tests.rs"]
mod outbound_tests;

#[cfg(test)]
#[path = "lanes_tests.rs"]
mod lanes_tests;
