//! `broadcast` channel（packages/services/src/broadcast/broadcastService.ts 与 Main 的
//! `packages/desktop/src/main/broadcastHub.ts`）。
//!
//! App 级 Host 是广播扇出与 claim 表的唯一所有者（docs/specs/rust-u4-app-host.md §2.3），不经 Main 中继：
//!
//! - **扇出**按收听连接的窗口身份（`CallContext.window_id`）决定每条连接收到的消息：
//!
//!   | 发送方                     | 同一窗口的连接 | 其他连接                            |
//!   | -------------------------- | -------------- | ----------------------------------- |
//!   | 某个窗口的连接             | 原样           | 带 `sourceWindowId` = 发送方窗口 id |
//!   | 没有窗口身份的连接         | 自己：原样     | 带 `sourceWindowId` = 0             |
//!   | Host 内部（`Broadcaster`） | 原样           | 原样                                |
//!
//!   UI 用“消息没有 `sourceWindowId`”判断是自己窗口发的（codingPlanQuotaResetState、IntlProvider）。
//! - **claim** 与 BroadcastHub 同语义：先到先得；reservation 5 s 过期；只有持有 token 的申请方（连接）可以
//!   commit / release；committed 永久保留；上限 1024 条，超出淘汰最早的；连接关闭时回收它未 commit 的
//!   reservation。结果在 Host 内同步得出，不再有 2 s 等待上限。
//!
//! ```text
//! Renderer(窗口 1) ─send─► Broadcast ─► 窗口 1 的连接：原样
//!                                    └► 窗口 2 / 无窗口的连接：{...message, sourceWindowId: 1}
//! account（Host 内部）─Broadcaster.send─► 所有连接：原样
//! ```
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use zcode_host_control::WindowId;
use zcode_host_kit::{CallContext, EventHub, Overflow, Reply, RpcError, ServiceBuilder};

use crate::main_bridge::random_id;

/// reservation 的有效期（BroadcastHub `BROADCAST_CLAIM_RESERVATION_TTL_MS`）。
pub const CLAIM_RESERVATION_TTL_MS: u64 = 5_000;
/// `busy` 的重试提示上限（`BROADCAST_CLAIM_RETRY_MS`）。
pub const CLAIM_RETRY_MS: u64 = 250;
/// claim 表上限（`MAX_BROADCAST_CLAIMS`），超出时淘汰最早登记的。
pub const MAX_CLAIMS: usize = 1_024;

/// 毫秒时钟；claim 的过期只比较差值，用单调时钟，测试注入手动时钟。
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

fn monotonic_clock() -> Clock {
    let started = Instant::now();
    Arc::new(move || started.elapsed().as_millis() as u64)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Lease {
    key: String,
    token: String,
}

fn validate_message(message: Value) -> Result<Value, RpcError> {
    let channel_ok = message
        .get("channel")
        .and_then(Value::as_str)
        .is_some_and(|channel| !channel.trim().is_empty());
    let source_ok = match message.get("sourceWindowId") {
        None | Some(Value::Null) => true,
        Some(value) => value.as_i64().is_some(),
    };
    if !message.is_object() || !channel_ok || !source_ok {
        return Err(RpcError::new("ZodError", "invalid broadcast message"));
    }
    Ok(message)
}

/// 广播的发送方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sender {
    /// Host 内部（例如账号失效通知）。
    Host,
    Window(WindowId),
    /// 没有窗口身份的连接（U7 的手机远控）：按“另一个窗口”对待。
    Connection(u64),
}

impl Sender {
    fn of(context: &CallContext) -> Self {
        match context.window_id {
            Some(window) => Sender::Window(window),
            None => Sender::Connection(context.connection),
        }
    }
}

struct Envelope {
    message: Value,
    sender: Sender,
}

impl Envelope {
    /// 这条连接收到的消息：同窗口（或发送方连接自己）原样，其他连接带 `sourceWindowId`。
    fn deliver_to(&self, listener: &CallContext) -> Value {
        let source = match self.sender {
            Sender::Host => None,
            Sender::Window(window) if listener.window_id == Some(window) => None,
            Sender::Window(window) => Some(window.get()),
            Sender::Connection(connection) if listener.connection == connection => None,
            // 窗口 id 从 1 开始，0 表示来源不是窗口（rust-u4-app-host.md §2.3）。
            Sender::Connection(_) => Some(0),
        };
        let mut message = self.message.clone();
        if let (Some(source), Some(object)) = (source, message.as_object_mut()) {
            object.insert("sourceWindowId".to_owned(), json!(source));
        }
        message
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaimStatus {
    Reserved { expires_at: u64 },
    Committed,
}

struct Claim {
    token: String,
    /// 申请方连接（`CallContext.connection`）。
    owner: u64,
    status: ClaimStatus,
    /// 登记顺序：超出上限时淘汰最小的（同 JS `Map` 的插入顺序；commit 不改变位置）。
    order: u64,
}

#[derive(Default)]
struct ClaimState {
    claims: HashMap<String, Claim>,
    next_order: u64,
}

impl ClaimState {
    fn prune(&mut self, now: u64) {
        self.claims.retain(|_, claim| match claim.status {
            ClaimStatus::Reserved { expires_at } => expires_at > now,
            ClaimStatus::Committed => true,
        });
        while self.claims.len() > MAX_CLAIMS {
            let Some(oldest) = self
                .claims
                .iter()
                .min_by_key(|(_, claim)| claim.order)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.claims.remove(&oldest);
        }
    }

    /// 只有持有 token 的申请方能结算自己的 reservation。
    fn held_reservation(&mut self, owner: u64, lease: &Lease) -> Option<&mut Claim> {
        self.claims.get_mut(&lease.key).filter(|claim| {
            matches!(claim.status, ClaimStatus::Reserved { .. })
                && claim.owner == owner
                && claim.token == lease.token
        })
    }
}

/// claim 表（BroadcastHub 的 claims），申请方是一条连接。
pub struct Claims {
    clock: Clock,
    state: Mutex<ClaimState>,
}

impl Claims {
    pub fn new(clock: Clock) -> Self {
        Self {
            clock,
            state: Mutex::new(ClaimState::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ClaimState> {
        // 表只做整条插入、修改与删除，持锁时 panic 不会留下半更新状态。
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `acquireClaim` 的结果（与 TS `BroadcastClaimAcquireResult` 同形）。
    pub fn acquire(&self, owner: u64, key: &str) -> Value {
        let key = key.trim();
        if key.is_empty() {
            return json!({"status": "unavailable"});
        }
        let now = (self.clock)();
        let mut state = self.lock();
        state.prune(now);
        match state.claims.get(key).map(|claim| claim.status) {
            Some(ClaimStatus::Committed) => return json!({"status": "committed"}),
            Some(ClaimStatus::Reserved { expires_at }) => {
                return json!({
                    "status": "busy",
                    "retryAfterMs": CLAIM_RETRY_MS.min(expires_at.saturating_sub(now)),
                });
            }
            None => {}
        }
        let order = state.next_order;
        state.next_order += 1;
        let token = format!("{owner}:{}:{order}", random_id());
        state.claims.insert(
            key.to_owned(),
            Claim {
                token: token.clone(),
                owner,
                status: ClaimStatus::Reserved {
                    expires_at: now + CLAIM_RESERVATION_TTL_MS,
                },
                order,
            },
        );
        state.prune(now);
        json!({"status": "acquired", "lease": {"key": key, "token": token}})
    }

    fn commit(&self, owner: u64, lease: &Lease) {
        let now = (self.clock)();
        let mut state = self.lock();
        state.prune(now);
        if let Some(claim) = state.held_reservation(owner, lease) {
            claim.status = ClaimStatus::Committed;
        }
    }

    fn release(&self, owner: u64, lease: &Lease) {
        let now = (self.clock)();
        let mut state = self.lock();
        state.prune(now);
        if state.held_reservation(owner, lease).is_some() {
            state.claims.remove(&lease.key);
        }
    }

    /// 连接关闭：回收它尚未 commit 的 reservation；已 commit 的保留，避免后来的窗口重播同一次提示。
    pub fn release_connection(&self, owner: u64) {
        self.lock()
            .claims
            .retain(|_, claim| claim.owner != owner || claim.status == ClaimStatus::Committed);
    }

    pub fn len(&self) -> usize {
        self.lock().claims.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Host 内部发广播（例如账号的 `auth:zcode-jwt-invalid`）：所有连接都收到原样的消息。
#[derive(Clone)]
pub struct Broadcaster {
    messages: EventHub<Arc<Envelope>>,
}

impl Broadcaster {
    pub fn send(&self, message: Value) {
        self.messages.emit(Arc::new(Envelope {
            message,
            sender: Sender::Host,
        }));
    }
}

/// `broadcast` channel 与 Host 内部的发送端（单调时钟）。
pub fn service_with_sender() -> (ServiceBuilder, Broadcaster) {
    service_with_clock(monotonic_clock())
}

/// 同 [`service_with_sender`]，claim 的过期用给定时钟（测试注入）。
pub fn service_with_clock(clock: Clock) -> (ServiceBuilder, Broadcaster) {
    // 订阅者是各连接的 `onDidReceive` 转发（Gateway 立即搬进出站队列）；落后 1024 条说明转发卡住，
    // 结束它的订阅（docs/specs/rust-u2-task-scope.md §6.2）。
    let messages = EventHub::<Arc<Envelope>>::new("broadcast", 1024, Overflow::CloseSubscriber);
    let claims = Arc::new(Claims::new(clock));
    let broadcaster = Broadcaster {
        messages: messages.clone(),
    };
    let send_messages = messages.clone();
    let (acquire_claims, commit_claims, release_claims, try_claims, closed_claims) = (
        claims.clone(),
        claims.clone(),
        claims.clone(),
        claims.clone(),
        claims,
    );
    let builder = ServiceBuilder::new("broadcast")
        .method("send", move |context, args| {
            let result = args
                .get::<Value>(0, "message")
                .and_then(validate_message)
                .map(|message| {
                    send_messages.emit(Arc::new(Envelope {
                        message,
                        sender: Sender::of(&context),
                    }));
                });
            async move { result.map(|()| Reply::Undefined) }
        })
        .method("acquireClaim", move |context, args| {
            let result = args
                .get::<String>(0, "key")
                .map(|key| acquire_claims.acquire(context.connection, &key));
            async move { result.map(Reply::from) }
        })
        .method("commitClaim", move |context, args| {
            let result = args
                .get::<Lease>(0, "lease")
                .map(|lease| commit_claims.commit(context.connection, &lease));
            async move { result.map(|()| Reply::Undefined) }
        })
        .method("releaseClaim", move |context, args| {
            let result = args
                .get::<Lease>(0, "lease")
                .map(|lease| release_claims.release(context.connection, &lease));
            async move { result.map(|()| Reply::Undefined) }
        })
        .method("tryClaim", move |context, args| {
            let result = args.get::<String>(0, "key").map(|key| {
                let acquired = try_claims.acquire(context.connection, &key);
                if acquired["status"] != "acquired" {
                    return false;
                }
                let lease = Lease {
                    key: acquired["lease"]["key"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    token: acquired["lease"]["token"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                };
                try_claims.commit(context.connection, &lease);
                true
            });
            async move { result.map(|claimed| Reply::from(Value::Bool(claimed))) }
        })
        .event("onMessage", move |context, _| {
            let listener = context.clone();
            Ok(Box::pin(messages.subscribe().map(move |envelope| {
                Reply::Value(envelope.deliver_to(&listener))
            })))
        })
        .on_connection_closed(move |context| closed_claims.release_connection(context.connection));
    (builder, broadcaster)
}

#[cfg(test)]
#[path = "broadcast_tests.rs"]
mod tests;
