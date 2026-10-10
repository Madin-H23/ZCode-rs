//! `@zcode/rpc` 客户端（`packages/rpc/src/channelClient.ts`）：Rust Host 连接远端 `zcode-server`。
//!
//! - 收到 `[200]` Initialize 之前发出的请求按序排队，之后原样发出；请求 id 从 0 递增。
//! - 调用 future 被丢弃（调用方放弃等待）时发 `[101]` 取消；事件流被丢弃时发 `[103]` 退订。
//! - `[202]` 还原为 [`RpcError`]（message、name 与透传键）；`[203]` 的载荷放进 `data`。
//! - 传输的入站一端结束即视为连接关闭：挂起调用全部失败，事件流结束。
//! - 队列都有界（docs/specs/rust-u2-task-scope.md §6.2）：出站（含 `[200]` 之前排队的请求）至多
//!   [`OUTBOUND_CAPACITY`] 条，满了说明传输已经很久没有写出（对端不读），客户端关闭，挂起调用以
//!   [`CONNECTION_CLOSED`] 失败，由持有者按连接断开处理；每个事件流至多 [`LISTENER_CAPACITY`] 条，
//!   读不过来的那个事件流结束（并向对端退订），其他调用与事件流不受影响。读循环从不等待。
//!
//! 传输只交换完整的 rpc 消息（不含 `SocketProtocol` 帧头）；分帧由传输层负责。
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures_util::Stream;
use serde_json::Value;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot, watch};
use zcode_async::Spawner;

use crate::codec::{
    self, REQUEST_EVENT_DISPOSE, REQUEST_EVENT_LISTEN, REQUEST_PROMISE, REQUEST_PROMISE_CANCEL,
    RESPONSE_EVENT_FIRE, RESPONSE_INITIALIZE, RESPONSE_PROMISE_ERROR, RESPONSE_PROMISE_ERROR_OBJ,
    RESPONSE_PROMISE_SUCCESS, RpcError, RpcValue,
};

/// 连接关闭后所有挂起调用的错误。
pub const CONNECTION_CLOSED: &str = "Connection closed";

/// 出站队列的容量（条，含 `[200]` 之前排队的）。满了时关闭客户端：消息不能丢，同步的发送方又不能等。
pub const OUTBOUND_CAPACITY: usize = 4096;
/// 每个事件流的队列容量（条）。满了时结束这个事件流并退订：读循环不能为一个慢消费者停下。
pub const LISTENER_CAPACITY: usize = 4096;

/// 应答 body 保持序列化形态；`call` 解码，`call_encoded` 原样交给转发方。
type Pending = oneshot::Sender<Result<Vec<u8>, RpcError>>;

#[derive(Default)]
struct State {
    initialized: bool,
    closed: bool,
    next_id: i32,
    queued: Vec<Vec<u8>>,
    pending: HashMap<i32, Pending>,
    listeners: HashMap<i32, mpsc::Sender<Vec<u8>>>,
}

struct Inner {
    outbound: mpsc::Sender<Vec<u8>>,
    state: Mutex<State>,
    initialized: watch::Sender<bool>,
    closed: watch::Sender<bool>,
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Initialize 之前排队，之后直接发出；已关闭时丢弃。任一队列满了就关闭客户端（见模块文档）。
    fn send(&self, message: Vec<u8>) {
        let mut state = self.lock();
        if state.closed {
            return;
        }
        let overflow = if state.initialized {
            matches!(self.outbound.try_send(message), Err(TrySendError::Full(_)))
        } else if state.queued.len() < OUTBOUND_CAPACITY {
            state.queued.push(message);
            false
        } else {
            true
        };
        drop(state);
        if overflow {
            self.close();
        }
    }

    fn close(&self) {
        let (pending, listeners) = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            state.queued.clear();
            (
                std::mem::take(&mut state.pending),
                std::mem::take(&mut state.listeners),
            )
        };
        for (_, sender) in pending {
            let _ = sender.send(Err(closed_error()));
        }
        drop(listeners);
        self.closed.send_replace(true);
    }

    fn on_message(&self, message: &[u8]) {
        let (kind, id, body) = match codec::split_response(message) {
            Ok(response) => response,
            // 无法解析的响应与 TS 一样无从对应到请求，丢弃。
            Err(_) => return,
        };
        match kind {
            RESPONSE_INITIALIZE => {
                let mut state = self.lock();
                if state.initialized || state.closed {
                    return;
                }
                state.initialized = true;
                // 排队的条数不超过出站容量，而出站队列此时还是空的，全部放得下。
                for message in std::mem::take(&mut state.queued) {
                    let _ = self.outbound.try_send(message);
                }
                drop(state);
                self.initialized.send_replace(true);
            }
            RESPONSE_PROMISE_SUCCESS | RESPONSE_PROMISE_ERROR | RESPONSE_PROMISE_ERROR_OBJ => {
                let Some(id) = id else { return };
                let Some(sender) = self.lock().pending.remove(&id) else {
                    return;
                };
                let result = match kind {
                    RESPONSE_PROMISE_SUCCESS => Ok(body),
                    RESPONSE_PROMISE_ERROR => {
                        Err(RpcError::from_payload(&decode(&body).into_json()))
                    }
                    _ => Err(error_object(decode(&body).into_json())),
                };
                let _ = sender.send(result);
            }
            RESPONSE_EVENT_FIRE => {
                let Some(id) = id else { return };
                let mut state = self.lock();
                let Some(listener) = state.listeners.get(&id) else {
                    return;
                };
                match listener.try_send(body) {
                    Ok(()) => {}
                    Err(TrySendError::Closed(_)) => {
                        state.listeners.remove(&id);
                    }
                    // 消费方落后 LISTENER_CAPACITY 条：结束这个事件流（已入队的照常读完）并退订。
                    Err(TrySendError::Full(_)) => {
                        state.listeners.remove(&id);
                        drop(state);
                        self.send(codec::encode_cancel(REQUEST_EVENT_DISPOSE, id));
                    }
                }
            }
            _ => {}
        }
    }
}

/// 解码一条已序列化的值；损坏的载荷按 undefined 处理（与 TS 读到截断数据时同样无法使用）。
fn decode(body: &[u8]) -> RpcValue {
    codec::Reader::new(body)
        .value()
        .unwrap_or(RpcValue::Undefined)
}

fn closed_error() -> RpcError {
    RpcError::new("Error", CONNECTION_CLOSED)
}

/// `[203]`：非 Error 对象的拒绝。Rust 侧统一成 Error，原载荷放在 `data`。
fn error_object(payload: Value) -> RpcError {
    let message = payload
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| payload.to_string());
    RpcError::new("Error", message).with_data(payload)
}

#[derive(Clone)]
pub struct RpcClient {
    inner: Arc<Inner>,
}

impl RpcClient {
    /// `outbound` / `inbound` 承载完整的 rpc 消息；`outbound` 的容量应为 [`OUTBOUND_CAPACITY`]。
    /// 读循环是 `spawner` 作用域里的任务 `rpc-client-reader`：入站结束、或作用域关闭使它被中止时，
    /// 客户端关闭。
    pub fn start(
        spawner: &Spawner,
        outbound: mpsc::Sender<Vec<u8>>,
        mut inbound: mpsc::Receiver<Vec<u8>>,
    ) -> Self {
        let (initialized, _) = watch::channel(false);
        let (closed, _) = watch::channel(false);
        let inner = Arc::new(Inner {
            outbound,
            state: Mutex::new(State::default()),
            initialized,
            closed,
        });
        let reader = Arc::downgrade(&inner);
        drop(spawner.spawn("rpc-client-reader", async move {
            // 读循环以任何方式结束（入站结束，或作用域关闭时被中止）都关闭客户端：挂起的调用失败，
            // 不会一直等一个不再有人读的应答。
            let _close = CloseOnExit(reader.clone());
            while let Some(message) = inbound.recv().await {
                let Some(inner) = reader.upgrade() else {
                    return;
                };
                inner.on_message(&message);
            }
        }));
        Self { inner }
    }

    /// 等待 `[200]`；连接先关闭则返回 false。
    pub async fn wait_initialized(&self) -> bool {
        let mut initialized = self.inner.initialized.subscribe();
        let mut closed = self.inner.closed.subscribe();
        tokio::select! {
            result = initialized.wait_for(|value| *value) => result.is_ok(),
            _ = closed.wait_for(|value| *value) => false,
        }
    }

    pub fn is_closed(&self) -> bool {
        self.inner.lock().closed
    }

    /// 连接关闭时完成。
    pub async fn closed(&self) {
        let mut closed = self.inner.closed.subscribe();
        let _ = closed.wait_for(|value| *value).await;
    }

    /// 主动关闭：挂起调用失败、事件流结束。传输本身由持有者关闭。
    pub fn close(&self) {
        self.inner.close();
    }

    /// `[100, id, channel, method]` + 参数数组。
    pub fn call(
        &self,
        channel: &str,
        method: &str,
        args: Vec<RpcValue>,
    ) -> impl std::future::Future<Output = Result<RpcValue, RpcError>> + Send + 'static {
        let call = self.call_encoded(channel, method, args);
        async move { call.await.map(|body| decode(&body)) }
    }

    /// 同 [`Self::call`]，结果保持序列化形态（转发给 Renderer 时逐字节不变）。
    pub fn call_encoded(
        &self,
        channel: &str,
        method: &str,
        args: Vec<RpcValue>,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, RpcError>> + Send + 'static {
        let (sender, receiver) = oneshot::channel();
        let id = {
            let mut state = self.inner.lock();
            if state.closed {
                None
            } else {
                let id = state.next_id;
                state.next_id = state.next_id.wrapping_add(1);
                state.pending.insert(id, sender);
                Some(id)
            }
        };
        let guard = id.map(|id| {
            self.inner.send(codec::encode_request(
                REQUEST_PROMISE,
                id,
                channel,
                method,
                &RpcValue::Array(args),
            ));
            CancelGuard {
                inner: Some(self.inner.clone()),
                id,
            }
        });
        async move {
            let Some(guard) = guard else {
                return Err(closed_error());
            };
            let result = receiver.await.unwrap_or_else(|_| Err(closed_error()));
            guard.disarm();
            result
        }
    }

    /// `[102, id, channel, event]` + 单个参数。丢弃返回的流即退订。
    pub fn listen(&self, channel: &str, event: &str, arg: RpcValue) -> RpcEvents {
        RpcEvents(self.listen_encoded(channel, event, arg))
    }

    /// 同 [`Self::listen`]，事件保持序列化形态。
    pub fn listen_encoded(&self, channel: &str, event: &str, arg: RpcValue) -> RpcEncodedEvents {
        let (sender, receiver) = mpsc::channel(LISTENER_CAPACITY);
        let id = {
            let mut state = self.inner.lock();
            if state.closed {
                None
            } else {
                let id = state.next_id;
                state.next_id = state.next_id.wrapping_add(1);
                state.listeners.insert(id, sender);
                Some(id)
            }
        };
        if let Some(id) = id {
            self.inner.send(codec::encode_request(
                REQUEST_EVENT_LISTEN,
                id,
                channel,
                event,
                &arg,
            ));
        }
        RpcEncodedEvents {
            receiver,
            inner: Arc::downgrade(&self.inner),
            id,
        }
    }
}

struct CloseOnExit(std::sync::Weak<Inner>);

impl Drop for CloseOnExit {
    fn drop(&mut self) {
        if let Some(inner) = self.0.upgrade() {
            inner.close();
        }
    }
}

struct CancelGuard {
    inner: Option<Arc<Inner>>,
    id: i32,
}

impl CancelGuard {
    fn disarm(mut self) {
        self.inner = None;
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        // 只有仍在等待的调用才需要通知对端取消。
        let Some(inner) = self.inner.take() else {
            return;
        };
        if inner.lock().pending.remove(&self.id).is_some() {
            inner.send(codec::encode_cancel(REQUEST_PROMISE_CANCEL, self.id));
        }
    }
}

/// 远端事件流（已解码）；丢弃即发 `[103]`。
pub struct RpcEvents(RpcEncodedEvents);

impl Stream for RpcEvents {
    type Item = RpcValue;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<RpcValue>> {
        Pin::new(&mut self.0)
            .poll_next(cx)
            .map(|item| item.map(|body| decode(&body)))
    }
}

/// 远端事件流（序列化形态）；丢弃即发 `[103]`。
pub struct RpcEncodedEvents {
    receiver: mpsc::Receiver<Vec<u8>>,
    inner: std::sync::Weak<Inner>,
    id: Option<i32>,
}

impl Stream for RpcEncodedEvents {
    type Item = Vec<u8>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Vec<u8>>> {
        self.receiver.poll_recv(cx)
    }
}

impl Drop for RpcEncodedEvents {
    fn drop(&mut self) {
        let (Some(id), Some(inner)) = (self.id, self.inner.upgrade()) else {
            return;
        };
        if inner.lock().listeners.remove(&id).is_some() {
            inner.send(codec::encode_cancel(REQUEST_EVENT_DISPOSE, id));
        }
    }
}
