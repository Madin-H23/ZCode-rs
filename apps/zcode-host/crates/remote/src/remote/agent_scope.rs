//! 远端 workspace 的 `zcode-agent` connection scope（`zcodeAgentConnectionScope.ts`，terminal-client
//! 角色），base 是远端 `zcode-server` 的同名 channel。规则见 rust-host-remote.md §2.5：
//!
//! ```text
//! hello / initialize ─► 本地完成（与本地 scope 相同的载荷与 clientHello 校验）
//! 命令、查询、附件 ─► 删除可伪造字段，写入 __zcodeTrustedV4Connection ─► 远端（trusted-host-relay）
//! subscribe ─► 暂存同 ownership 的帧 ─► 远端 ACK ─► 登记所有权，按到达顺序放行同 subscriptionId 的帧
//! 远端 frame ─► 每个 (kind, workspaceKey) 一条上游 listen ─► 已拥有：发给本连接；否则暂存
//! 远端 onAgentRuntimeLifecycle unavailable ─► 该 workspace 的所有权与暂存失效
//! 连接关闭 ─► 每条路由 setConnectionFlowStateV4{closed} ─► 反向退订本连接拥有的订阅
//! ```
//!
//! 订阅放在独立任务里：连接关闭会中止调用 future，但迟到的 ACK 仍要反向退订。
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use tokio::task::AbortHandle;
use zcode_host_control::ClientMode;
use zcode_host_kit::{
    CallContext, CallFuture, EventHub, EventStream, Reply, RpcError, RpcValue, ServiceChannel, log,
};
use zcode_host_rpc::RpcClient;

/// 解码一条已序列化的远端结果，只用于读取其中的字段。
fn decode(encoded: &[u8]) -> Value {
    zcode_host_rpc::Reader::new(encoded)
        .value()
        .map(RpcValue::into_json)
        .unwrap_or(Value::Null)
}
use crate::agent::service::{CHANNEL, parse_client_hello, serial_key, uuid_v4};

const TRUSTED_CONNECTION_FIELD: &str = "__zcodeTrustedV4Connection";
const TRUSTED_UNSUBSCRIBE_ROUTE_FIELD: &str = "__zcodeTrustedV4UnsubscribeRoute";
const STAGING_MAX_FRAMES: usize = 1024;
const STAGING_MAX_BYTES: usize = 32 * 1024 * 1024;
const WIRE_PROTOCOL_VERSION: u64 = 3;
const OVERFLOW: &str = "fault.subscription.initialFrameStagingOverflow";

fn fault(message: &str) -> RpcError {
    RpcError::new("Error", message)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Conversation,
    SessionsIndex,
    WorkspaceConfig,
}

impl Kind {
    fn subscribe(self) -> &'static str {
        match self {
            Kind::Conversation => "subscribeConversationV4",
            Kind::SessionsIndex => "subscribeSessionsIndexV4",
            Kind::WorkspaceConfig => "subscribeWorkspaceConfigV4",
        }
    }

    fn unsubscribe(self) -> &'static str {
        match self {
            Kind::Conversation => "unsubscribeConversationV4",
            Kind::SessionsIndex => "unsubscribeSessionsIndexV4",
            Kind::WorkspaceConfig => "unsubscribeWorkspaceConfigV4",
        }
    }

    fn resync(self) -> &'static str {
        match self {
            Kind::Conversation => "resyncConversationV4",
            Kind::SessionsIndex => "resyncSessionsIndexV4",
            Kind::WorkspaceConfig => "resyncWorkspaceConfigV4",
        }
    }

    fn event(self) -> &'static str {
        match self {
            Kind::Conversation => "onDynamicConversationFrame",
            Kind::SessionsIndex => "onDynamicSessionsIndexFrame",
            Kind::WorkspaceConfig => "onDynamicWorkspaceConfigFrame",
        }
    }

    fn topic(self, params: &Value, workspace_key: &str) -> Result<String, RpcError> {
        Ok(match self {
            Kind::Conversation => {
                let session = params["sessionId"]
                    .as_str()
                    .ok_or_else(|| fault("invalid argument sessionId: is required"))?;
                format!("conversation/{session}")
            }
            Kind::SessionsIndex => format!("sessions-index/{workspace_key}"),
            Kind::WorkspaceConfig => format!("workspace-config/{workspace_key}"),
        })
    }
}

/// `workspaceIdentity?.trim() || workspacePath`。
fn workspace_key(params: &Value) -> String {
    params["workspaceIdentity"]
        .as_str()
        .map(str::trim)
        .filter(|identity| !identity.is_empty())
        .or_else(|| params["workspacePath"].as_str())
        .unwrap_or_default()
        .to_owned()
}

fn workspace_target(params: &Value) -> Value {
    let mut target = Map::new();
    target.insert("workspacePath".into(), params["workspacePath"].clone());
    if let Some(identity) = params["workspaceIdentity"]
        .as_str()
        .filter(|identity| !identity.is_empty())
    {
        target.insert("workspaceIdentity".into(), json!(identity));
    }
    Value::Object(target)
}

/// `withTrustedConnection`：先删除所有可伪造字段，再写入 Host 真值。
pub(super) fn with_trusted_connection(params: &Value, carrier: &Value) -> Value {
    let mut forwarded = params.as_object().cloned().unwrap_or_default();
    for key in [
        TRUSTED_CONNECTION_FIELD,
        "connectionId",
        "clientMode",
        "deliveryProfile",
        "subscriberScope",
        "workflowRunDeltas",
    ] {
        forwarded.shift_remove(key);
    }
    forwarded.insert(TRUSTED_CONNECTION_FIELD.into(), carrier.clone());
    Value::Object(forwarded)
}

fn with_unsubscribe_route(params: &Value, topic: &str, connection_id: &str) -> Value {
    let mut forwarded = params.as_object().cloned().unwrap_or_default();
    forwarded.shift_remove(TRUSTED_UNSUBSCRIBE_ROUTE_FIELD);
    forwarded.insert(
        TRUSTED_UNSUBSCRIBE_ROUTE_FIELD.into(),
        json!({"topic": topic, "connectionId": connection_id}),
    );
    Value::Object(forwarded)
}

fn client_mode_str(mode: ClientMode) -> &'static str {
    match mode {
        ClientMode::DesktopContinuous => "desktop-continuous",
        ClientMode::WebRemoteReplayable => "web-remote-replayable",
    }
}

#[derive(Debug, Clone)]
struct Owned {
    kind: Kind,
    workspace_key: String,
    target: Value,
    topic: String,
    subscription_id: String,
}

struct Pending {
    id: u64,
    kind: Kind,
    workspace_key: String,
    topic: String,
    frames: Vec<Value>,
    bytes: usize,
    overflow: bool,
    generation: u64,
    invalidated: bool,
}

struct Route {
    hub: EventHub<Value>,
    task: AbortHandle,
}

#[derive(Default)]
struct ScopeState {
    closed: bool,
    hello_issued: bool,
    handshake_complete: bool,
    bound_client_id: Option<String>,
    workflow_run_deltas: bool,
    query_workspace: Option<String>,
    owned: Vec<Owned>,
    pending: Vec<Pending>,
    next_pending: u64,
    routes: HashMap<(Kind, String), Route>,
    generations: HashMap<String, u64>,
    /// attachmentBeginV4 进入 staging 的 workspace：关闭时也要发 `closed`。
    attachment_routes: HashMap<String, Value>,
    lifecycle: Option<AbortHandle>,
}

struct Scope {
    connection_id: String,
    client_mode: ClientMode,
    client: RpcClient,
    state: Mutex<ScopeState>,
}

impl Scope {
    fn lock(&self) -> std::sync::MutexGuard<'_, ScopeState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn assert_open(&self) -> Result<(), RpcError> {
        if self.lock().closed {
            return Err(fault("fault.connection.closed"));
        }
        Ok(())
    }

    fn assert_ready(&self) -> Result<(), RpcError> {
        let state = self.lock();
        if state.closed {
            return Err(fault("fault.connection.closed"));
        }
        if !state.handshake_complete {
            return Err(fault("fault.connection.handshakeRequired"));
        }
        Ok(())
    }

    fn carrier(&self) -> Value {
        let mut carrier = json!({
            "connectionId": self.connection_id,
            "clientMode": client_mode_str(self.client_mode),
        });
        if self.lock().workflow_run_deltas {
            carrier["workflowRunDeltas"] = json!(true);
        }
        carrier
    }

    fn hello(&self) -> Result<Value, RpcError> {
        let mut state = self.lock();
        if state.closed {
            return Err(fault("fault.connection.closed"));
        }
        state.hello_issued = true;
        let continuous = self.client_mode == ClientMode::DesktopContinuous;
        Ok(json!({
            "kind": "hello",
            "protocolVersion": WIRE_PROTOCOL_VERSION,
            "connectionId": self.connection_id,
            "clientMode": client_mode_str(self.client_mode),
            "deliveryProfile": if continuous { "continuous" } else { "replayable" },
            "serverTime": now_ms(),
            "capabilities": {
                "nativeDialogs": continuous,
                "localTerminal": continuous,
                "binaryFrames": false,
                "compression": "none",
                "workspaceHookReview": true,
                "independentPlanState": true,
                "workflowRunDeltas": true,
            },
            "auth": {},
        }))
    }

    fn initialize(&self, client_hello: &Value) -> Result<(), RpcError> {
        let mut state = self.lock();
        if state.closed {
            return Err(fault("fault.connection.closed"));
        }
        if !state.hello_issued {
            return Err(fault("fault.connection.helloRequired"));
        }
        let (client_id, workflow_run_deltas) = parse_client_hello(client_hello)?;
        if state
            .bound_client_id
            .as_ref()
            .is_some_and(|bound| *bound != client_id)
        {
            return Err(fault("fault.connection.clientChanged"));
        }
        state.bound_client_id = Some(client_id);
        state.workflow_run_deltas = workflow_run_deltas;
        state.handshake_complete = true;
        Ok(())
    }

    /// 写入可信 carrier 后转发；结果保持远端编码原样交给 Renderer。
    async fn forward(&self, method: &str, params: Value) -> Result<Vec<u8>, RpcError> {
        let carrier = self.carrier();
        self.client
            .call_encoded(
                CHANNEL,
                method,
                vec![RpcValue::Json(with_trusted_connection(&params, &carrier))],
            )
            .await
    }

    fn owns(state: &ScopeState, kind: Kind, workspace_key: &str, frame: &Value) -> bool {
        let (Some(subscription_id), Some(topic)) =
            (frame["subscriptionId"].as_str(), frame["topic"].as_str())
        else {
            return false;
        };
        state.owned.iter().any(|entry| {
            entry.kind == kind
                && entry.subscription_id == subscription_id
                && entry.topic == topic
                && entry.workspace_key == workspace_key
        })
    }

    /// 每个 (kind, workspaceKey) 一条上游 listen；帧按所有权分发或暂存。
    fn route(self: &Arc<Self>, kind: Kind, params: &Value) -> EventStream {
        let key = workspace_key(params);
        let mut state = self.lock();
        if let Some(route) = state.routes.get(&(kind, key.clone())) {
            return Box::pin(route.hub.subscribe().map(Reply::Value));
        }
        let hub = EventHub::<Value>::new();
        let events = hub.subscribe();
        let mut upstream =
            self.client
                .listen(CHANNEL, kind.event(), RpcValue::Json(params.clone()));
        let (scope, route_hub, route_key) = (Arc::downgrade(self), hub.clone(), key.clone());
        let task = tokio::spawn(async move {
            while let Some(frame) = upstream.next().await {
                let Some(scope) = scope.upgrade() else { return };
                let frame = frame.into_json();
                let mut state = scope.lock();
                if Self::owns(&state, kind, &route_key, &frame) {
                    drop(state);
                    route_hub.emit(frame);
                    continue;
                }
                let topic = frame["topic"].as_str().unwrap_or_default().to_owned();
                let bytes = frame.to_string().len();
                for pending in state.pending.iter_mut().filter(|pending| {
                    pending.kind == kind
                        && pending.workspace_key == route_key
                        && pending.topic == topic
                        && !pending.overflow
                        && !pending.invalidated
                }) {
                    // 超限时清空整批并显式失败，不能 shift 掉最旧帧造成永久缺片。
                    if pending.frames.len() + 1 > STAGING_MAX_FRAMES
                        || pending.bytes + bytes > STAGING_MAX_BYTES
                    {
                        pending.frames.clear();
                        pending.bytes = 0;
                        pending.overflow = true;
                        continue;
                    }
                    pending.frames.push(frame.clone());
                    pending.bytes += bytes;
                }
            }
        })
        .abort_handle();
        state.routes.insert((kind, key), Route { hub, task });
        Box::pin(events.map(Reply::Value))
    }

    fn take_pending(&self, id: u64) -> Option<Pending> {
        let mut state = self.lock();
        let index = state.pending.iter().position(|pending| pending.id == id)?;
        Some(state.pending.remove(index))
    }

    async fn unsubscribe_base(&self, entry: &Owned, runtime_policy: Option<&Value>) {
        let mut params = entry.target.as_object().cloned().unwrap_or_default();
        params.insert("subscriptionId".into(), json!(entry.subscription_id));
        if let Some(policy) = runtime_policy.filter(|policy| !policy.is_null()) {
            params.insert("runtimePolicy".into(), policy.clone());
        }
        let forwarded =
            with_unsubscribe_route(&Value::Object(params), &entry.topic, &self.connection_id);
        if let Err(error) = self
            .client
            .call(
                CHANNEL,
                entry.kind.unsubscribe(),
                vec![RpcValue::Json(forwarded)],
            )
            .await
        {
            log::debug(format!(
                "remote {} failed: {}",
                entry.kind.unsubscribe(),
                error.message
            ));
        }
    }

    async fn subscribe(self: Arc<Self>, kind: Kind, params: Value) -> Result<Reply, RpcError> {
        self.assert_ready()?;
        let key = workspace_key(&params);
        let topic = kind.topic(&params, &key)?;
        let target = workspace_target(&params);
        let pending_id = {
            let mut state = self.lock();
            let id = state.next_pending;
            state.next_pending += 1;
            let generation = state.generations.get(&key).copied().unwrap_or(0);
            state.pending.push(Pending {
                id,
                kind,
                workspace_key: key.clone(),
                topic: topic.clone(),
                frames: Vec::new(),
                bytes: 0,
                overflow: false,
                generation,
                invalidated: false,
            });
            id
        };
        let carrier = self.carrier();
        let result = self
            .client
            .call_encoded(
                CHANNEL,
                kind.subscribe(),
                vec![RpcValue::Json(with_trusted_connection(&params, &carrier))],
            )
            .await;
        let encoded = match result {
            Ok(encoded) => encoded,
            Err(error) => {
                self.take_pending(pending_id);
                return Err(error);
            }
        };
        let ack = decode(&encoded);
        let Some(subscription_id) = ack["ack"]["subscriptionId"].as_str().map(str::to_owned) else {
            self.take_pending(pending_id);
            return Ok(Reply::Encoded(encoded));
        };
        let entry = Owned {
            kind,
            workspace_key: key.clone(),
            target,
            topic: topic.clone(),
            subscription_id: subscription_id.clone(),
        };
        let pending = self.take_pending(pending_id);
        let (closed, current_generation) = {
            let state = self.lock();
            (
                state.closed,
                state.generations.get(&key).copied().unwrap_or(0),
            )
        };
        let Some(pending) = pending else {
            return Err(fault("fault.connection.closed"));
        };
        if pending.invalidated || pending.generation != current_generation {
            // 旧 runtime 的迟到 ACK 可能与新 runtime 复用同一 subId：只丢本地 owner，不反向退订。
            return Err(fault("fault.subscription.runtimeRestarted"));
        }
        if closed {
            // 连接关闭与 ACK 并发：迟到 ACK 不能在已关闭的 scope 重新登记，立即反向退订。
            self.unsubscribe_base(&entry, None).await;
            return Err(fault("fault.connection.closed"));
        }
        if pending.overflow {
            self.unsubscribe_base(&entry, None).await;
            return Err(fault(OVERFLOW));
        }
        let hub = {
            let mut state = self.lock();
            // 同一 topic 的新订阅取代旧订阅。
            state.owned.retain(|owned| {
                !(owned.kind == kind && owned.workspace_key == key && owned.topic == topic)
            });
            state.owned.push(entry);
            state
                .routes
                .get(&(kind, key.clone()))
                .map(|route| route.hub.clone())
        };
        if let Some(hub) = hub {
            // CLI 通知可能先于订阅响应到达：ACK 后只放行属于这个 subscriptionId 的暂存帧，保持到达顺序。
            for frame in pending.frames {
                if frame["subscriptionId"] == subscription_id.as_str() {
                    hub.emit(frame);
                }
            }
        }
        Ok(Reply::Encoded(encoded))
    }

    fn find_owned(&self, kind: Kind, params: &Value) -> Option<Owned> {
        let key = workspace_key(params);
        let subscription_id = params["subscriptionId"].as_str()?;
        let state = self.lock();
        let matches: Vec<&Owned> = state
            .owned
            .iter()
            .filter(|entry| {
                entry.kind == kind
                    && entry.subscription_id == subscription_id
                    && entry.workspace_key == key
            })
            .collect();
        // 同一 method/workspace 内 subId 碰撞时宁可拒绝猜测。
        (matches.len() == 1).then(|| matches[0].clone())
    }

    async fn unsubscribe(&self, kind: Kind, params: Value) -> Result<Reply, RpcError> {
        let Some(entry) = self.find_owned(kind, &params) else {
            return Ok(Reply::Undefined);
        };
        self.lock().owned.retain(|owned| {
            !(owned.kind == entry.kind
                && owned.subscription_id == entry.subscription_id
                && owned.topic == entry.topic)
        });
        self.close_unused_flow_route(&entry).await;
        self.unsubscribe_base(&entry, params.get("runtimePolicy"))
            .await;
        Ok(Reply::Undefined)
    }

    /// 最后一个订阅退订前，Node 只在已转发过流控状态的路由上补发 `closed`；Rust Host 没有
    /// WebSocket 的 saturated 边沿，从不转发过流控，因此这里没有需要关闭的路由。
    async fn close_unused_flow_route(&self, _entry: &Owned) {}

    async fn resync(&self, kind: Kind, params: Value) -> Result<Reply, RpcError> {
        self.assert_ready()?;
        let Some(entry) = self.find_owned(kind, &params) else {
            return Err(fault("fault.subscription.notOwned"));
        };
        let forwarded = with_unsubscribe_route(&params, &entry.topic, &self.connection_id);
        self.client
            .call_encoded(CHANNEL, kind.resync(), vec![RpcValue::Json(forwarded)])
            .await
            .map(Reply::Encoded)
    }

    fn invalidate(&self, workspace_key: &str, generation: u64) {
        let mut state = self.lock();
        state
            .generations
            .insert(workspace_key.to_owned(), generation);
        // runtime 内的订阅已全部消失：本地 owner 直接忘掉，不向新 runtime 清理。
        state
            .owned
            .retain(|entry| entry.workspace_key != workspace_key);
        for pending in state
            .pending
            .iter_mut()
            .filter(|pending| pending.workspace_key == workspace_key)
        {
            pending.invalidated = true;
            pending.frames.clear();
            pending.bytes = 0;
        }
    }

    fn ensure_lifecycle(self: &Arc<Self>) {
        let mut state = self.lock();
        if state.lifecycle.is_some() || state.closed {
            return;
        }
        let mut events =
            self.client
                .listen(CHANNEL, "onAgentRuntimeLifecycle", RpcValue::Undefined);
        let scope = Arc::downgrade(self);
        state.lifecycle = Some(
            tokio::spawn(async move {
                while let Some(event) = events.next().await {
                    let Some(scope) = scope.upgrade() else { return };
                    let event = event.into_json();
                    // available 不是旧 ownership 的失效边界；真正的换代一定先发 unavailable。
                    if event["state"] == "available" {
                        continue;
                    }
                    let (Some(key), Some(generation)) = (
                        event["workspaceKey"].as_str(),
                        event["runtimeIdentity"]["generation"].as_u64(),
                    ) else {
                        continue;
                    };
                    scope.invalidate(key, generation);
                }
            })
            .abort_handle(),
        );
    }

    async fn dispose(&self) {
        let (routes, owned, flow_targets, tasks) = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            state.handshake_complete = false;
            state.hello_issued = false;
            state.bound_client_id = None;
            state.pending.clear();
            let owned = std::mem::take(&mut state.owned);
            let mut flow_targets: HashMap<String, Value> =
                std::mem::take(&mut state.attachment_routes);
            for entry in &owned {
                flow_targets
                    .entry(entry.workspace_key.clone())
                    .or_insert_with(|| entry.target.clone());
            }
            let routes: Vec<Route> = state.routes.drain().map(|(_, route)| route).collect();
            let tasks: Vec<AbortHandle> = state.lifecycle.take().into_iter().collect();
            (routes, owned, flow_targets, tasks)
        };
        for task in tasks {
            task.abort();
        }
        for route in routes {
            route.task.abort();
            route.hub.close();
        }
        let carrier = json!({
            "connectionId": self.connection_id,
            "clientMode": client_mode_str(self.client_mode),
        });
        // closed 排在退订之前：反序会让上游先忘 owner，closed 被拒后留下暂停的连接。
        for target in flow_targets.into_values() {
            let mut params = target.as_object().cloned().unwrap_or_default();
            params.insert("state".into(), json!("closed"));
            let forwarded = with_trusted_connection(&Value::Object(params), &carrier);
            if let Err(error) = self
                .client
                .call(
                    CHANNEL,
                    "setConnectionFlowStateV4",
                    vec![RpcValue::Json(forwarded)],
                )
                .await
            {
                log::debug(format!(
                    "remote setConnectionFlowStateV4(closed) failed: {}",
                    error.message
                ));
            }
        }
        for entry in owned {
            self.unsubscribe_base(&entry, None).await;
        }
    }
}

/// `zcode-agent` channel：每条 attachment 连接一个 [`Scope`]。
pub struct RemoteAgentChannel {
    client: RpcClient,
    scopes: Mutex<HashMap<u64, Arc<Scope>>>,
}

impl RemoteAgentChannel {
    pub fn new(client: RpcClient) -> Arc<Self> {
        Arc::new(Self {
            client,
            scopes: Mutex::new(HashMap::new()),
        })
    }

    fn scope(&self, context: &CallContext) -> Arc<Scope> {
        let scope = self
            .scopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(context.connection)
            .or_insert_with(|| {
                Arc::new(Scope {
                    connection_id: format!("host-rpc-{}", uuid_v4()),
                    client_mode: context.client_mode,
                    client: self.client.clone(),
                    state: Mutex::new(ScopeState::default()),
                })
            })
            .clone();
        scope.ensure_lifecycle();
        scope
    }
}

fn first_arg(args: &[RpcValue]) -> Value {
    args.first()
        .cloned()
        .map(RpcValue::into_json)
        .unwrap_or(Value::Null)
}

fn kind_of(method: &str) -> Option<(Kind, &'static str)> {
    let kinds = [
        Kind::Conversation,
        Kind::SessionsIndex,
        Kind::WorkspaceConfig,
    ];
    for kind in kinds {
        if method == kind.subscribe() {
            return Some((kind, "subscribe"));
        }
        if method == kind.unsubscribe() {
            return Some((kind, "unsubscribe"));
        }
        if method == kind.resync() {
            return Some((kind, "resync"));
        }
    }
    None
}

/// 需要握手、并写入可信 carrier 的转发方法。
const READY_FORWARDED: &[&str] = &[
    "backgroundBashOutputV4",
    "conversationRowsRangeV4",
    "attachmentReadV4",
    "conversationAttachmentReadV4",
    "conversationAttachmentStatV4",
    "attachmentPreviewSourceV4",
    "attachmentChunkV4",
    "attachmentCommitV4",
    "attachmentAbortV4",
];

impl ServiceChannel for RemoteAgentChannel {
    /// 与本地 `zcode-agent` 同一张串行键表：同一会话的写操作按到达顺序逐个转发到远端。
    fn serial_key(
        &self,
        _context: &CallContext,
        method: &str,
        args: &[RpcValue],
    ) -> Option<String> {
        serial_key(method, args)
    }

    fn call(&self, context: &CallContext, method: &str, args: Vec<RpcValue>) -> CallFuture {
        let scope = self.scope(context);
        let params = first_arg(&args);
        if let Some((kind, action)) = kind_of(method) {
            return match action {
                "subscribe" => {
                    // 订阅在独立任务里完成：调用 future 被中止时，迟到的 ACK 仍会反向退订。
                    let task = tokio::spawn(scope.subscribe(kind, params));
                    Box::pin(async move {
                        task.await
                            .unwrap_or_else(|_| Err(fault("fault.connection.closed")))
                    })
                }
                "unsubscribe" => Box::pin(async move { scope.unsubscribe(kind, params).await }),
                _ => Box::pin(async move { scope.resync(kind, params).await }),
            };
        }
        match method {
            "helloConversationV4" => {
                let result = scope.hello().map(Reply::Value);
                Box::pin(async move { result })
            }
            "initializeConversationV4" => {
                let result = scope.initialize(&params).map(|()| Reply::Undefined);
                Box::pin(async move { result })
            }
            "setConnectionFlowStateV4" => {
                let result = scope
                    .assert_open()
                    .and(Err(fault("fault.connection.flowControlForbidden")));
                Box::pin(async move { result })
            }
            "sendConversationCommandV4" => Box::pin(async move {
                scope.assert_ready()?;
                let bound = scope.lock().bound_client_id.clone();
                if params["envelope"]["clientId"].as_str() != bound.as_deref() {
                    return Err(fault("fault.command.clientMismatch"));
                }
                scope
                    .forward("sendConversationCommandV4", params)
                    .await
                    .map(Reply::Encoded)
            }),
            "queryConversationCommandsV4" => Box::pin(async move {
                scope.assert_ready()?;
                if params["clock"].is_null() || params["clock"] == false {
                    let key = workspace_key(&params);
                    let mut state = scope.lock();
                    if state
                        .query_workspace
                        .as_ref()
                        .is_some_and(|bound| *bound != key)
                    {
                        return Err(fault("fault.command.queryForeignWorkspace"));
                    }
                    state.query_workspace = Some(key);
                }
                scope
                    .forward("queryConversationCommandsV4", params)
                    .await
                    .map(Reply::Encoded)
            }),
            "attachmentBeginV4" => Box::pin(async move {
                scope.assert_ready()?;
                let reply = scope.forward("attachmentBeginV4", params.clone()).await?;
                let staging = decode(&reply)["state"] == "staging";
                if staging {
                    let closed = scope.lock().closed;
                    if closed {
                        // begin ACK 晚于连接关闭：用同一可信身份 abort，再拒绝迟到结果。
                        let _ = scope.forward("attachmentAbortV4", params).await;
                        return Err(fault("fault.connection.closed"));
                    }
                    scope
                        .lock()
                        .attachment_routes
                        .insert(workspace_key(&params), workspace_target(&params));
                }
                Ok(Reply::Encoded(reply))
            }),
            ready if READY_FORWARDED.contains(&ready) => {
                let method = ready.to_owned();
                Box::pin(async move {
                    scope.assert_ready()?;
                    scope.forward(&method, params).await.map(Reply::Encoded)
                })
            }
            other => {
                // 其余成员原样转发（Proxy 透传）。
                let call = self.client.call_encoded(CHANNEL, other, args);
                Box::pin(async move { call.await.map(Reply::Encoded) })
            }
        }
    }

    fn listen(
        &self,
        context: &CallContext,
        event: &str,
        arg: RpcValue,
    ) -> Result<EventStream, RpcError> {
        let scope = self.scope(context);
        let params = arg.clone().into_json();
        let none =
            || -> Result<EventStream, RpcError> { Ok(Box::pin(futures_util::stream::pending())) };
        let forward = |arg: RpcValue| -> Result<EventStream, RpcError> {
            Ok(Box::pin(
                self.client
                    .listen_encoded(CHANNEL, event, arg)
                    .map(Reply::Encoded),
            ))
        };
        let continuous = context.client_mode == ClientMode::DesktopContinuous;
        match event {
            "onDynamicConversationFrame" => Ok(scope.route(Kind::Conversation, &params)),
            "onDynamicSessionsIndexFrame" => Ok(scope.route(Kind::SessionsIndex, &params)),
            "onDynamicWorkspaceConfigFrame" => Ok(scope.route(Kind::WorkspaceConfig, &params)),
            // 远端 identity 永远不是本地 desktop 的 TTFT 事实。
            "onDynamicLocalTtftFacts" => {
                scope.assert_open()?;
                none()
            }
            "onDynamicConversationTelemetryFact" => {
                scope.assert_open()?;
                if !continuous {
                    return none();
                }
                let carrier = scope.carrier();
                forward(RpcValue::Json(with_trusted_connection(
                    &workspace_target(&params),
                    &carrier,
                )))
            }
            "onDynamicCuaPermissionObservation" => {
                scope.assert_open()?;
                if continuous { forward(arg) } else { none() }
            }
            // 资源与 MCP 遥测只供可信 Host relay，不进入 renderer / 手机 attachment。
            "onDynamicProcessResourceSample"
            | "onDynamicToolExecResource"
            | "onDynamicMcpResourceSamples"
            | "onDynamicMcpTelemetry" => none(),
            _ => forward(arg),
        }
    }

    fn connection_closed(&self, context: &CallContext) {
        let scope = self
            .scopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&context.connection);
        if let Some(scope) = scope {
            tokio::spawn(async move { scope.dispose().await });
        }
    }
}
