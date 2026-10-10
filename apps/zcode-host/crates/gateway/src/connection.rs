//! 一条连接上的 `@zcode/rpc` 服务端循环（`packages/rpc/src/channelServer.ts`；
//! docs/specs/rust-u2-gateway.md §2.2、§2.4）。
//!
//! 与 Node 版的有意差异：未知 channel 立即报错，不做 1000 ms 等待；订阅失败只记日志
//! （Node 版此时异常逃出消息处理器，同样不会应答）。
//!
//! 调用与订阅转发是连接作用域 `connection:<id>` 里的任务；循环本身在 Gateway 的作用域里，按
//! §2.4 的顺序收口：
//!
//! ```text
//! 停止读取（释放 inbound 接收端）
//!   → 排队中的调用离开串行通道（不会开始）
//!   → 出站队列关闭（transport 的写循环结束），中止连接作用域里的调用与订阅转发，等它们结束
//!   → 各 channel 的 connection_closed（恰好一次）
//! ```
//!
//! 每个关闭触发先在关闭开关上写下原因（第一个生效），出站队列关闭时带上它（§2.6）。
use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use futures_util::{FutureExt, StreamExt};
use tokio::sync::{mpsc, watch};
use zcode_async::{Spawner, Task, TaskScope};
use zcode_host_kit::{
    CallContext, CallFuture, ChannelRegistry, FlowState, Reply, ServiceChannel, log,
};
use zcode_host_rpc::codec::{self, RESPONSE_EVENT_FIRE, RESPONSE_PROMISE_SUCCESS};
use zcode_host_rpc::{Request, RequestKind, RpcError, RpcValue, read_request};

use crate::lanes::Ticket;
use crate::outbound::{CloseReason, CloseSwitch, Held, Outbound, Queue};
use crate::{AttachmentMeta, ChannelSource, Channels, Entry, Shared};

/// 入站队列的容量（条）。满了以后 transport 停止读取，背压经 TCP 到达对端；连接循环处理一条请求
/// 不等待任何东西（调用在任务里执行），所以队列只会在循环还没拿到 channel 表时积压。
const INBOUND_CAPACITY: usize = 64;

/// [`crate::Gateway::connect`] 的结果：transport 侧的一条连接。
pub struct Connection {
    /// 连接 id，即 `CallContext.connection`。
    pub id: u64,
    pub inbound: Inbound,
    pub outbound: Outbound,
}

/// transport 把收到的完整 rpc 消息交给 Gateway。释放它即表示 transport 不再读取（对端断开）。
pub struct Inbound(mpsc::Sender<Vec<u8>>);

/// 连接已被 Gateway 关闭。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionClosed;

impl Inbound {
    /// 交出一条消息。入站队列满时等待；连接已关闭时返回 [`ConnectionClosed`]。
    pub async fn send(&self, message: Vec<u8>) -> Result<(), ConnectionClosed> {
        self.0.send(message).await.map_err(|_| ConnectionClosed)
    }

    /// Gateway 关闭这条连接（停止读取）时完成。
    pub async fn closed(&self) {
        self.0.closed().await;
    }
}

pub(crate) fn connect(
    shared: &Arc<Shared>,
    meta: AttachmentMeta,
    channels: ChannelSource,
) -> Connection {
    let id = shared
        .next_connection
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let close = CloseSwitch::new();
    let (queue, edges) = Queue::new(id, shared.limits, close.clone());
    let (inbound, inbound_rx) = mpsc::channel(INBOUND_CAPACITY);
    let context = CallContext {
        connection: id,
        attachment_id: meta.attachment_id.clone(),
        client_mode: meta.client_mode,
        window_id: meta.window_id,
    };
    {
        let mut connections = shared.connections();
        // 与 `Gateway::shutdown` 在同一把锁里：Host 退出之后接入的连接直接关闭（不发 `[200]`）。
        if shared
            .shutting_down
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            close.close(CloseReason::HostShutdown);
        }
        connections.insert(
            id,
            Entry {
                meta,
                close: close.clone(),
                queue: queue.clone(),
            },
        );
    }
    let registration = Registration {
        shared: shared.clone(),
        id,
        queue: queue.clone(),
        close: close.clone(),
        scope: shared.root.child(format!("connection:{id}")),
    };
    let closed = close.subscribe();
    // 作用域已关闭（Host 正在退出）时任务不会运行：future 被丢弃，Registration 随之清理。
    drop(shared.root.spawn(
        "connection",
        run(Loop {
            registration,
            context,
            channels,
            inbound: inbound_rx,
            queue: queue.clone(),
            edges,
            close,
            closed,
            _held: shared.in_use.hold(),
        }),
    ));
    Connection {
        id,
        inbound: Inbound(inbound),
        outbound: Outbound::new(queue, shared.in_use.hold()),
    }
}

/// 连接在注册表里的登记与它的作用域。正常收口由 [`Live::close`] 按顺序完成；循环没有走到那里就
/// 结束时（拿不到 channel 表、被中止或 panic）由这里兜底：排队中的调用离队，出站队列关闭（transport
/// 随之结束；还没有原因时记为 `InternalError`），之后字段 `scope` 被释放，作用域里的任务被中止。
struct Registration {
    shared: Arc<Shared>,
    id: u64,
    queue: Arc<Queue>,
    close: CloseSwitch,
    scope: TaskScope,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.shared.lanes.withdraw_connection(self.id);
        self.close.close(CloseReason::InternalError);
        self.queue.close();
        self.shared.connections().remove(&self.id);
    }
}

struct Loop {
    registration: Registration,
    context: CallContext,
    channels: ChannelSource,
    inbound: mpsc::Receiver<Vec<u8>>,
    queue: Arc<Queue>,
    edges: watch::Receiver<FlowState>,
    close: CloseSwitch,
    closed: watch::Receiver<Option<CloseReason>>,
    /// 循环结束（收口完，含 `connection_closed`）或被丢弃时释放：`Gateway::shutdown` 等它。
    _held: Held,
}

async fn run(state: Loop) {
    let Loop {
        registration,
        context,
        channels,
        mut inbound,
        queue,
        mut edges,
        close,
        mut closed,
        _held,
    } = state;
    let resolved = tokio::select! {
        biased;
        _ = closed.wait_for(Option::is_some) => None,
        channels = channels => channels,
    };
    // 拿不到 channel 表（Host 未就绪就退出、远端 scope 接入失败）：不发 `[200]`，直接关闭。
    let Some(Channels {
        registry,
        closed: mut ended,
    }) = resolved
    else {
        close.close(CloseReason::ScopeUnavailable);
        return;
    };
    queue.push(codec::initialize());
    log::debug(format!("attachment {} initialized", context.attachment_id));

    let live = Live {
        shared: registration.shared.clone(),
        context,
        registry,
        queue,
        spawner: registration.scope.spawner(),
        running: Arc::default(),
    };
    // 积压边沿与请求在同一循环里处理；关闭信号优先。
    let mut edges_open = true;
    loop {
        tokio::select! {
            biased;
            _ = closed.wait_for(Option::is_some) => break,
            () = wait_ended(ended.as_mut()) => {
                close.close(CloseReason::ScopeClosed);
                break;
            }
            changed = edges.changed(), if edges_open => match changed {
                Ok(()) => {
                    let state = *edges.borrow_and_update();
                    live.registry.connection_flow(&live.context, state);
                }
                Err(_) => edges_open = false,
            },
            message = inbound.recv() => match message {
                Some(message) => live.handle(&message),
                None => {
                    close.close(CloseReason::PeerClosed);
                    break;
                }
            },
        }
    }
    // 1. 停止读取：transport 的 `Inbound::send` 随之失败、`Inbound::closed` 完成。
    drop(inbound);
    live.close(registration).await;
}

/// 远端 scope 的关闭信号；没有信号或发送端已释放（不会再关闭）时永不完成。
async fn wait_ended(ended: Option<&mut watch::Receiver<bool>>) {
    if let Some(ended) = ended
        && ended.wait_for(|ended| *ended).await.is_ok()
    {
        return;
    }
    std::future::pending().await
}

/// 连接作用域里的一个任务（调用或订阅转发）。
struct Running {
    task: Task<()>,
    /// 有串行键的调用：键与排队位置，客户端取消时用来离队。
    lane: Option<(String, u64)>,
}

type RunningTasks = Arc<Mutex<HashMap<i32, Running>>>;

fn lock(running: &RunningTasks) -> MutexGuard<'_, HashMap<i32, Running>> {
    running.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 调用怎样开始：没有键的调用在到达时就调用 `channel.call`（与今天相同，转发型 channel 因此按到达
/// 顺序发出请求）；有键的调用轮到它时才调用。
enum Start {
    Now(CallFuture),
    Queued {
        ticket: Ticket,
        channel: Arc<dyn ServiceChannel>,
        context: CallContext,
        args: Vec<RpcValue>,
    },
}

struct Live {
    shared: Arc<Shared>,
    context: CallContext,
    registry: Arc<ChannelRegistry>,
    queue: Arc<Queue>,
    spawner: Spawner,
    running: RunningTasks,
}

impl Live {
    fn handle(&self, message: &[u8]) {
        let request = match read_request(message) {
            Ok(request) => request,
            Err(error) => {
                log::warn(format!("dropping malformed rpc message: {error}"));
                return;
            }
        };
        match request.kind {
            RequestKind::Promise => self.call(request),
            RequestKind::EventListen => self.listen(request),
            RequestKind::PromiseCancel | RequestKind::EventDispose => self.cancel(request.id),
        }
    }

    fn call(&self, request: Request) {
        let Request {
            id,
            channel: channel_name,
            name: method,
            body,
            ..
        } = request;
        let args = match body {
            RpcValue::Array(items) => items,
            RpcValue::Undefined => Vec::new(),
            other => vec![other],
        };
        let Some(channel) = self.registry.get(&channel_name) else {
            self.queue
                .push(codec::promise_error(id, &unknown_channel(&channel_name)));
            return;
        };
        // RPC 网络遥测（NetworkTelemetryChannelServer）：只包已登记的 channel，计时含排队与整个调用。
        let started = Instant::now();
        let key = match std::panic::catch_unwind(AssertUnwindSafe(|| {
            channel.serial_key(&self.context, &method, &args)
        })) {
            Ok(key) => key,
            Err(payload) => return self.panicked_in_loop(id, &channel_name, &method, &*payload),
        };
        let (start, lane) = match key {
            Some(key) => {
                // 修复 G1（rust-u2-gateway.md §1）：之前每个调用一到达就各自成为一个任务，多线程运行时上
                // 谁先执行不确定，同一连接连续 2000 个调用里有六百多对相邻调用的执行顺序与到达顺序相反，
                // 连续发出的两条会话命令可能以相反的顺序到达 runtime。现在有键的调用在这里同步登记排队
                // 位置（连接循环按到达顺序处理请求），轮到它时才开始，不依赖任务调度或异步锁的公平性。
                let ticket = self
                    .shared
                    .lanes
                    .enter(key.clone(), self.context.connection);
                let lane = Some((key, ticket.id()));
                (
                    Start::Queued {
                        ticket,
                        channel,
                        context: self.context.clone(),
                        args,
                    },
                    lane,
                )
            }
            None => match std::panic::catch_unwind(AssertUnwindSafe(|| {
                channel.call(&self.context, &method, args)
            })) {
                Ok(call) => (Start::Now(call), None),
                Err(payload) => {
                    return self.panicked_in_loop(id, &channel_name, &method, &*payload);
                }
            },
        };
        let call = Call {
            id,
            channel: channel_name,
            method,
            started,
            queue: self.queue.clone(),
            observer: self.shared.observer.clone(),
            running: self.running.clone(),
        };
        self.track(id, lane, "call", call.run(start));
    }

    fn listen(&self, request: Request) {
        let Some(channel) = self.registry.get(&request.channel) else {
            log::warn(format!("listen on unknown channel {}", request.channel));
            return;
        };
        let listened = std::panic::catch_unwind(AssertUnwindSafe(|| {
            channel.listen(&self.context, &request.name, request.body)
        }));
        let mut events = match listened {
            Ok(Ok(events)) => events,
            Ok(Err(error)) => {
                log::warn(format!(
                    "listen {}.{} rejected: {}",
                    request.channel, request.name, error.message
                ));
                return;
            }
            Err(payload) => {
                log::error(format!(
                    "listen {}.{} panicked on connection {}: {}",
                    request.channel,
                    request.name,
                    self.context.connection,
                    panic_message(&*payload)
                ));
                return;
            }
        };
        let (id, queue, running) = (request.id, self.queue.clone(), self.running.clone());
        self.track(id, None, "listen", async move {
            while let Some(event) = events.next().await {
                if !queue.push(encode_reply(RESPONSE_EVENT_FIRE, id, event)) {
                    break;
                }
            }
            lock(&running).remove(&id);
        });
    }

    /// 启动任务并登记。持锁完成“启动并登记”：任务即使立刻结束，也要等登记完成才能把自己移除。
    fn track(
        &self,
        id: i32,
        lane: Option<(String, u64)>,
        name: &'static str,
        future: impl Future<Output = ()> + Send + 'static,
    ) {
        let mut running = lock(&self.running);
        let task = self.spawner.spawn(name, future);
        running.insert(id, Running { task, lane });
    }

    /// 客户端取消调用或订阅。排队中的调用先离队，保证不会开始；被中止的任务留在表里，直到它真正
    /// 结束后被清掉（或在连接关闭时被等待）。
    fn cancel(&self, id: i32) {
        let mut running = lock(&self.running);
        if let Some(entry) = running.get(&id) {
            if let Some((key, ticket)) = &entry.lane {
                self.shared.lanes.withdraw(key, *ticket);
            }
            entry.task.abort();
        }
        running.retain(|_, entry| !entry.task.is_finished());
    }

    /// 同步部分（`serial_key`、`call` 返回 future 之前）panic：回错误，不让连接循环随之结束。
    fn panicked_in_loop(&self, id: i32, channel: &str, method: &str, payload: &(dyn Any + Send)) {
        let message = panic_message(payload);
        log::error(format!(
            "{channel}.{method} panicked on connection {}: {message}",
            self.context.connection
        ));
        let error = panic_error(channel, method);
        self.shared.observer.call_finished(
            channel,
            method,
            std::time::Duration::ZERO,
            Some(&error),
        );
        self.queue.push(codec::promise_error(id, &error));
    }

    /// 连接关闭（§2.4）。调用方已停止读取。
    async fn close(self, registration: Registration) {
        // 2. 排队中的调用离队：之后释放通道的调用不会把通道交给这条连接的调用。
        self.shared
            .lanes
            .withdraw_connection(self.context.connection);
        // 3. 出站队列关闭（transport 的写循环随之结束），中止调用与订阅转发并等它们真正结束：
        //    任务的 future（含 channel 的调用 future）都已释放之后才通知 channel。
        self.queue.close();
        let tasks: Vec<Running> = lock(&self.running)
            .drain()
            .map(|(_, entry)| entry)
            .collect();
        for entry in &tasks {
            entry.task.abort();
        }
        for entry in tasks {
            let _ = entry.task.await;
        }
        let reason = registration.close.reason();
        drop(registration);
        // 4. 按连接持有状态的 channel 清理；每条连接只走到这里一次。
        self.registry.connection_closed(&self.context);
        log::debug(format!(
            "attachment {} closed ({})",
            self.context.attachment_id,
            reason.map_or("unknown", CloseReason::as_str)
        ));
    }
}

/// 一次调用的上下文，移进调用任务。
struct Call {
    id: i32,
    channel: String,
    method: String,
    started: Instant,
    queue: Arc<Queue>,
    observer: Arc<dyn crate::CallObserver>,
    running: RunningTasks,
}

impl Call {
    async fn run(self, start: Start) {
        let (call, ticket) = match start {
            Start::Now(call) => (call, None),
            Start::Queued {
                mut ticket,
                channel,
                context,
                args,
            } => {
                if !ticket.turn().await {
                    // 排队中被撤出（取消、连接关闭）：不开始，也不应答。
                    drop(ticket);
                    lock(&self.running).remove(&self.id);
                    return;
                }
                let method = &self.method;
                match std::panic::catch_unwind(AssertUnwindSafe(|| {
                    channel.call(&context, method, args)
                })) {
                    Ok(call) => (call, Some(ticket)),
                    Err(payload) => return self.finish(Err(payload), Some(ticket)),
                }
            }
        };
        let outcome = AssertUnwindSafe(call).catch_unwind().await;
        self.finish(outcome, ticket);
    }

    /// 应答 → 释放串行通道 → 从连接的任务表移除；handler panic 时回错误后把 panic 交还作用域上报。
    fn finish(
        self,
        outcome: Result<Result<Reply, RpcError>, Box<dyn Any + Send>>,
        ticket: Option<Ticket>,
    ) {
        let elapsed = self.started.elapsed();
        let (response, panic) = match outcome {
            Ok(result) => {
                self.observer.call_finished(
                    &self.channel,
                    &self.method,
                    elapsed,
                    result.as_ref().err(),
                );
                let response = match result {
                    Ok(reply) => encode_reply(RESPONSE_PROMISE_SUCCESS, self.id, reply),
                    Err(error) => codec::promise_error(self.id, &error),
                };
                (response, None)
            }
            Err(payload) => {
                let error = panic_error(&self.channel, &self.method);
                self.observer
                    .call_finished(&self.channel, &self.method, elapsed, Some(&error));
                (codec::promise_error(self.id, &error), Some(payload))
            }
        };
        // 应答先于释放通道入队：同一键的下一个调用的应答排在它后面。
        self.queue.push(response);
        drop(ticket);
        lock(&self.running).remove(&self.id);
        if let Some(payload) = panic {
            // 交给作用域的上报（作用域路径与任务名进 Host 日志）；应答已经发出，等待方不会挂起。
            std::panic::resume_unwind(payload);
        }
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_owned())
}

fn panic_error(channel: &str, method: &str) -> RpcError {
    RpcError::new(
        "Error",
        format!("{channel}.{method} failed: internal error in the Rust Host"),
    )
}

fn unknown_channel(channel: &str) -> RpcError {
    RpcError::new(
        "Unknown channel",
        format!("Channel name '{channel}' is not registered in the Rust Host"),
    )
}

fn encode_reply(kind: i32, id: i32, reply: Reply) -> Vec<u8> {
    match reply {
        Reply::Undefined => codec::response_undefined(kind, id),
        Reply::Value(value) => codec::response_value(kind, id, &value),
        Reply::Bytes(bytes) => codec::response_bytes(kind, id, &bytes),
        Reply::Encoded(body) => codec::response_encoded(kind, id, &body),
        Reply::RawJson(raw) => codec::raw_response(kind, id, &raw).unwrap_or_else(|error| {
            codec::promise_error(id, &RpcError::new("Error", error.to_string()))
        }),
    }
}
