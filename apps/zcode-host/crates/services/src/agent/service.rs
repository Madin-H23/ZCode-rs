//! `zcode-agent` channel：每条 RPC 连接一个 connection scope（zcodeAgentConnectionScope.ts），
//! 其下是共享的 [`AgentRuntime`]（zcodeAgentService.ts 的 V4 转发部分）。
//!
//! scope 负责：签发 hello、强制 hello → clientHello 握手、注入可信的 connectionId / clientMode
//! （从不读取 UI 自报的值）、持有订阅并按 `(kind, workspaceKey, topic, subscriptionId, 进程代)`
//! 过滤帧。订阅 ACK 之前到达的帧按 pending ownership 暂存，ACK 后按到达顺序放行。
//!
//! ```text
//! subscribe ─► begin_pending ─► CLI v4/conversation/subscribe ─► ACK ─► remember（放行暂存帧）
//!                                       │ 响应行先于初始快照帧写出
//! CLI frame ─► AgentRuntime.frames ─► route 任务（每个 (kind, workspaceKey) 一个）
//!                ├─ 已拥有：发给该连接的 listen
//!                └─ 同 topic 有 pending：暂存（≤1024 帧 / 32 MiB，超出则订阅失败）
//! ```
//!
//! 订阅实际工作放在独立任务里：连接关闭会中止调用 future，但 ACK 迟到时仍要反向退订。
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use tokio::task::AbortHandle;
use zcode_host_control::ClientMode;
use zcode_host_kit::{
    Args, CallContext, CallFuture, EventHub, EventStream, Reply, RpcError, RpcValue,
    ServiceChannel, log, replies,
};

use super::api::AgentApi;
use super::extensions::UseReason;
use super::runtime::{
    AgentProcess, AgentRuntime, Frame, FrameKind, RUNTIME_UNAVAILABLE_CODE, RuntimePolicy,
    WorkspaceTarget,
};

pub const CHANNEL: &str = "zcode-agent";
const WIRE_PROTOCOL_VERSION: u64 = 3;
const STAGING_MAX_FRAMES: usize = 1024;
const STAGING_MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_FRAME_BYTES: usize = 1024 * 1024;

fn fault(message: &str) -> RpcError {
    RpcError::new("Error", message)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

/// RFC 4122 v4 UUID 文本。
pub fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    // 熵源不可用时无法生成连接身份，与签发 capability 一样直接终止。
    getrandom::fill(&mut bytes).expect("operating system random source unavailable");
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn client_mode_str(mode: ClientMode) -> &'static str {
    match mode {
        ClientMode::DesktopContinuous => "desktop-continuous",
        ClientMode::WebRemoteReplayable => "web-remote-replayable",
    }
}

/// `clientHelloSchema`（strict）；返回 `(clientId, workflowRunDeltas)`。
fn parse_client_hello(value: &Value) -> Result<(String, bool), RpcError> {
    let invalid = |detail: &str| fault(&format!("invalid clientHello: {detail}"));
    let object = value
        .as_object()
        .ok_or_else(|| invalid("expected object"))?;
    const KEYS: &[&str] = &[
        "kind",
        "protocolVersion",
        "clientId",
        "clientKind",
        "appVersion",
        "capabilities",
    ];
    if let Some(key) = object.keys().find(|key| !KEYS.contains(&key.as_str())) {
        return Err(invalid(&format!("unknown key {key}")));
    }
    if object.get("kind") != Some(&json!("clientHello")) {
        return Err(invalid("kind"));
    }
    if object.get("protocolVersion").and_then(Value::as_u64) != Some(WIRE_PROTOCOL_VERSION) {
        return Err(invalid("protocolVersion"));
    }
    let client_id = object
        .get("clientId")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("clientId"))?;
    if !object.get("appVersion").is_some_and(Value::is_string) {
        return Err(invalid("appVersion"));
    }
    if let Some(kind) = object.get("clientKind")
        && !matches!(
            kind.as_str(),
            Some("desktop" | "web" | "mobileRemote" | "mobileApp")
        )
    {
        return Err(invalid("clientKind"));
    }
    let mut workflow_run_deltas = false;
    if let Some(capabilities) = object.get("capabilities") {
        let capabilities = capabilities
            .as_object()
            .ok_or_else(|| invalid("capabilities"))?;
        for (key, flag) in capabilities {
            if !matches!(key.as_str(), "workspaceHookReviewUi" | "workflowRunDeltas") {
                return Err(invalid(&format!("unknown capability {key}")));
            }
            if !flag.is_boolean() {
                return Err(invalid(key));
            }
        }
        workflow_run_deltas = capabilities.get("workflowRunDeltas") == Some(&json!(true));
    }
    Ok((client_id.to_owned(), workflow_run_deltas))
}

fn required_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params[key]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            RpcError::new("Error", format!("invalid argument {key}: is required"))
                .with_code("fault.invalidArgument")
        })
}

/// 把 `params` 里存在的键原样拷到 CLI 参数里（undefined / null 视为未提供）。
fn copy_present(target: &mut Map<String, Value>, params: &Value, keys: &[&str]) {
    for key in keys {
        if let Some(value) = params.get(*key).filter(|value| !value.is_null()) {
            target.insert((*key).to_owned(), value.clone());
        }
    }
}

fn truthy_str(value: &Value) -> bool {
    value.as_str().is_some_and(|text| !text.is_empty())
}

/// mergeAutomationMutationToolDenylist / mergeOffPeakMutationToolDenylist：去重并保持首次出现的顺序。
fn merge_denylist(current: &Value, denied: &[&str]) -> Value {
    let mut merged: Vec<Value> = current.as_array().cloned().unwrap_or_default();
    for name in denied {
        if !merged.iter().any(|existing| existing == name) {
            merged.push(json!(name));
        }
    }
    let mut seen = Vec::new();
    merged.retain(|item| {
        if seen.contains(item) {
            false
        } else {
            seen.push(item.clone());
            true
        }
    });
    Value::Array(merged)
}

fn raw_reply(raw: impl AsRef<serde_json::value::RawValue>) -> Reply {
    Reply::RawJson(raw.as_ref().get().to_owned())
}

#[derive(Debug, Clone)]
struct Owned {
    kind: FrameKind,
    workspace_key: String,
    topic: String,
    subscription_id: String,
    generation: u64,
}

impl Owned {
    fn matches(&self, frame: &Frame) -> bool {
        self.kind == frame.kind
            && self.workspace_key == frame.workspace_key
            && self.topic == frame.topic
            && self.subscription_id == frame.subscription_id
            && self.generation == frame.generation
    }
}

struct Pending {
    id: u64,
    kind: FrameKind,
    workspace_key: String,
    topic: String,
    staged: Vec<Arc<Frame>>,
    bytes: usize,
    overflow: bool,
}

struct Route {
    hub: EventHub<Arc<Frame>>,
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
    routes: HashMap<(FrameKind, String), Route>,
    /// 发过请求的 workspace：连接关闭时向它们发 `v4/connection/flow closed`。
    touched: HashMap<String, WorkspaceTarget>,
}

enum RememberError {
    Closed,
    Overflow,
}

struct Scope {
    connection_id: String,
    client_mode: ClientMode,
    runtime: AgentRuntime,
    api: AgentApi,
    state: Mutex<ScopeState>,
}

impl Scope {
    fn lock(&self) -> std::sync::MutexGuard<'_, ScopeState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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

    fn touch(&self, target: &WorkspaceTarget) {
        self.lock()
            .touched
            .entry(target.key())
            .or_insert_with(|| target.clone());
    }

    fn begin_pending(&self, kind: FrameKind, workspace_key: &str, topic: &str) -> u64 {
        let mut state = self.lock();
        state.next_pending += 1;
        let id = state.next_pending;
        state.pending.push(Pending {
            id,
            kind,
            workspace_key: workspace_key.to_owned(),
            topic: topic.to_owned(),
            staged: Vec::new(),
            bytes: 0,
            overflow: false,
        });
        id
    }

    fn discard_pending(&self, id: u64) {
        self.lock().pending.retain(|pending| pending.id != id);
    }

    /// ACK 到达：登记所有权并按顺序放行同一 subscriptionId 的暂存帧。
    fn remember(&self, pending_id: u64, owned: Owned) -> Result<(), RememberError> {
        let mut state = self.lock();
        let position = state
            .pending
            .iter()
            .position(|pending| pending.id == pending_id);
        let pending = position.map(|index| state.pending.remove(index));
        if state.closed {
            return Err(RememberError::Closed);
        }
        if pending.as_ref().is_some_and(|pending| pending.overflow) {
            return Err(RememberError::Overflow);
        }
        // 同一 ownership 的新订阅取代旧订阅（CLI 按 (connectionId, topic) 替换）。
        state.owned.retain(|entry| {
            !(entry.kind == owned.kind
                && entry.workspace_key == owned.workspace_key
                && entry.topic == owned.topic)
        });
        if let Some(pending) = pending
            && let Some(route) = state.routes.get(&(owned.kind, owned.workspace_key.clone()))
        {
            for frame in pending.staged {
                if owned.matches(&frame) {
                    route.hub.emit(frame);
                }
            }
        }
        state.owned.push(owned);
        Ok(())
    }

    fn route_frame(&self, frame: Arc<Frame>) {
        let mut state = self.lock();
        if state.owned.iter().any(|owned| owned.matches(&frame)) {
            if let Some(route) = state.routes.get(&(frame.kind, frame.workspace_key.clone())) {
                route.hub.emit(frame);
            }
            return;
        }
        let size = frame.raw.get().len();
        for pending in state.pending.iter_mut().filter(|pending| {
            pending.kind == frame.kind
                && pending.workspace_key == frame.workspace_key
                && pending.topic == frame.topic
        }) {
            if pending.overflow {
                continue;
            }
            pending.bytes += size;
            pending.staged.push(frame.clone());
            if pending.staged.len() > STAGING_MAX_FRAMES || pending.bytes > STAGING_MAX_BYTES {
                pending.overflow = true;
                pending.staged.clear();
            }
        }
    }

    /// 一个 `(kind, workspaceKey)` 只向 runtime 订阅一次，各 listen 共享。
    fn frame_events(
        self: &Arc<Self>,
        kind: FrameKind,
        target: &Value,
    ) -> Result<EventStream, RpcError> {
        self.assert_open()?;
        let workspace_key = WorkspaceTarget::from_value(target)?.key();
        let mut state = self.lock();
        let route = state
            .routes
            .entry((kind, workspace_key.clone()))
            .or_insert_with(|| {
                let key = workspace_key.clone();
                let mut upstream =
                    Box::pin(self.runtime.frames().subscribe_filtered(move |frame| {
                        frame.kind == kind && frame.workspace_key == key
                    }));
                let scope = Arc::downgrade(self);
                let task = tokio::spawn(async move {
                    while let Some(frame) = upstream.next().await {
                        let Some(scope) = scope.upgrade() else { return };
                        scope.route_frame(frame);
                    }
                });
                Route {
                    hub: EventHub::new(),
                    task: task.abort_handle(),
                }
            });
        let events = route
            .hub
            .subscribe()
            .map(|frame| Reply::RawJson(frame.raw.get().to_owned()));
        Ok(Box::pin(events))
    }

    fn find_owned(
        &self,
        kind: FrameKind,
        workspace_key: &str,
        subscription_id: &str,
    ) -> Option<Owned> {
        let state = self.lock();
        let mut matches = state.owned.iter().filter(|owned| {
            owned.kind == kind
                && owned.workspace_key == workspace_key
                && owned.subscription_id == subscription_id
        });
        let first = matches.next()?.clone();
        matches.next().is_none().then_some(first)
    }

    fn forget(&self, owned: &Owned) {
        self.lock().owned.retain(|entry| {
            !(entry.kind == owned.kind
                && entry.workspace_key == owned.workspace_key
                && entry.subscription_id == owned.subscription_id
                && entry.generation == owned.generation)
        });
    }

    fn connection_fields(&self, workflow_run_deltas: bool) -> Map<String, Value> {
        let mut fields = Map::new();
        fields.insert("connectionId".into(), json!(self.connection_id));
        fields.insert(
            "clientMode".into(),
            json!(client_mode_str(self.client_mode)),
        );
        if workflow_run_deltas && self.lock().workflow_run_deltas {
            fields.insert("workflowRunDeltas".into(), json!(true));
        }
        fields
    }

    async fn subscribe(self: Arc<Self>, kind: FrameKind, params: Value) -> Result<Reply, RpcError> {
        self.assert_ready()?;
        let target = WorkspaceTarget::from_value(&params)?;
        let workspace_key = target.key();
        let (topic, policy) = match kind {
            FrameKind::Conversation => (
                format!("conversation/{}", required_str(&params, "sessionId")?),
                RuntimePolicy::StartIfNeeded,
            ),
            FrameKind::SessionsIndex => (
                format!("sessions-index/{workspace_key}"),
                RuntimePolicy::from_value(params.get("runtimePolicy")),
            ),
            FrameKind::WorkspaceConfig => (
                format!("workspace-config/{workspace_key}"),
                RuntimePolicy::from_value(params.get("runtimePolicy")),
            ),
        };
        let pending_id = self.begin_pending(kind, &workspace_key, &topic);
        let scope = self.clone();
        let work = tokio::spawn(async move {
            let result = scope
                .subscribe_on_cli(kind, &target, &topic, policy, pending_id, &params)
                .await;
            if result.is_err() {
                scope.discard_pending(pending_id);
            }
            result
        });
        work.await
            .unwrap_or_else(|error| Err(fault(&format!("subscribe task failed: {error}"))))
    }

    async fn subscribe_on_cli(
        &self,
        kind: FrameKind,
        target: &WorkspaceTarget,
        topic: &str,
        policy: RuntimePolicy,
        pending_id: u64,
        params: &Value,
    ) -> Result<Reply, RpcError> {
        let process = self.runtime.get_client(target, policy).await?;
        self.touch(target);
        if kind == FrameKind::Conversation {
            self.runtime
                .before_use(&process, UseReason::ConversationSubscribe)
                .await?;
        }
        let mut cli = Map::new();
        cli.insert("topic".into(), json!(topic));
        cli.extend(self.connection_fields(kind == FrameKind::Conversation));
        if kind == FrameKind::Conversation {
            cli.insert("workspace".into(), target.workspace_ref());
            // 冷订阅绕过 task adapter：旧 session 没有 durable selection 时从 tasks-index 带回思考强度。
            if let Some(level) = self.resume_thought_level(target, topic).await {
                cli.insert("resumeThoughtLevel".into(), json!(level));
            }
        }
        copy_present(&mut cli, params, &["base", "visibility"]);
        let raw = process
            .request("v4/conversation/subscribe", &Value::Object(cli))
            .await?;
        let result: Value = serde_json::from_str(raw.get())
            .map_err(|error| fault(&format!("invalid subscribe result: {error}")))?;
        let subscription_id = result["ack"]["subscriptionId"]
            .as_str()
            .ok_or_else(|| fault("invalid subscribe result: missing ack.subscriptionId"))?
            .to_owned();
        if process.client.is_closed() {
            return Err(fault("fault.subscription.runtimeRestarted"));
        }
        let owned = Owned {
            kind,
            workspace_key: target.key(),
            topic: topic.to_owned(),
            subscription_id,
            generation: process.generation,
        };
        match self.remember(pending_id, owned.clone()) {
            Ok(()) => Ok(raw_reply(raw)),
            Err(RememberError::Closed) => {
                self.unsubscribe_on_cli(&process, &owned).await;
                Err(fault("fault.connection.closed"))
            }
            Err(RememberError::Overflow) => {
                self.unsubscribe_on_cli(&process, &owned).await;
                Err(fault("fault.subscription.initialFrameStagingOverflow"))
            }
        }
    }

    async fn resume_thought_level(&self, target: &WorkspaceTarget, topic: &str) -> Option<String> {
        let tasks = self.api.tasks_index.as_ref()?;
        let session_id = topic.strip_prefix("conversation/")?;
        match tasks.get_task_meta(&target.key(), session_id).await {
            Ok(meta) => meta?["thoughtLevel"]
                .as_str()
                .map(str::trim)
                .filter(|level| !level.is_empty())
                .map(str::to_owned),
            Err(error) => {
                // 索引读取失败仍允许只读恢复。
                log::warn(format!(
                    "read cold resume thought hint failed for {session_id}: {error:#}"
                ));
                None
            }
        }
    }

    async fn unsubscribe_on_cli(&self, process: &AgentProcess, owned: &Owned) {
        let params = json!({
            "topic": owned.topic,
            "subscriptionId": owned.subscription_id,
            "connectionId": self.connection_id,
        });
        if let Err(error) = process
            .request("v4/conversation/unsubscribe", &params)
            .await
        {
            log::debug(format!(
                "unsubscribe {} failed: {}",
                owned.topic, error.message
            ));
        }
    }

    /// 订阅属于哪一代进程：只发给同一代，进程换代后旧订阅随之失效。
    async fn live_process(
        &self,
        target: &WorkspaceTarget,
        owned: &Owned,
    ) -> Option<std::sync::Arc<AgentProcess>> {
        self.runtime
            .existing(target)
            .await
            .filter(|process| process.generation == owned.generation)
    }

    async fn unsubscribe(&self, kind: FrameKind, params: Value) -> Result<Reply, RpcError> {
        let target = WorkspaceTarget::from_value(&params)?;
        let subscription_id = required_str(&params, "subscriptionId")?;
        let Some(owned) = self.find_owned(kind, &target.key(), subscription_id) else {
            return Ok(Reply::Undefined);
        };
        self.forget(&owned);
        if let Some(process) = self.live_process(&target, &owned).await {
            self.unsubscribe_on_cli(&process, &owned).await;
        }
        Ok(Reply::Undefined)
    }

    async fn resync(&self, kind: FrameKind, params: Value) -> Result<Reply, RpcError> {
        self.assert_ready()?;
        let target = WorkspaceTarget::from_value(&params)?;
        let subscription_id = required_str(&params, "subscriptionId")?;
        let owned = self
            .find_owned(kind, &target.key(), subscription_id)
            .ok_or_else(|| fault("fault.subscription.notOwned"))?;
        let Some(process) = self.live_process(&target, &owned).await else {
            self.forget(&owned);
            if RuntimePolicy::from_value(params.get("runtimePolicy")) == RuntimePolicy::ExistingOnly
                && self.runtime.existing(&target).await.is_none()
            {
                return Err(fault("ZCode Agent runtime is not running.")
                    .with_code(RUNTIME_UNAVAILABLE_CODE));
            }
            return Err(fault("fault.subscription.notOwned"));
        };
        let mut cli = Map::new();
        cli.insert("topic".into(), json!(owned.topic));
        cli.insert("connectionId".into(), json!(self.connection_id));
        cli.insert("subscriptionId".into(), json!(owned.subscription_id));
        cli.insert(
            "base".into(),
            params.get("base").cloned().unwrap_or(Value::Null),
        );
        copy_present(&mut cli, &params, &["forceSnapshot"]);
        let raw = process
            .request("v4/conversation/resync", &Value::Object(cli))
            .await?;
        // resync ACK 只对发起时的订阅有效；等待期间被取代或进程换代都按未拥有处理。
        if self
            .find_owned(kind, &owned.workspace_key, &owned.subscription_id)
            .is_none()
            || process.client.is_closed()
        {
            return Err(fault("fault.subscription.notOwned"));
        }
        Ok(raw_reply(raw))
    }

    async fn command(&self, params: Value) -> Result<Reply, RpcError> {
        self.assert_ready()?;
        let target = WorkspaceTarget::from_value(&params)?;
        let envelope = params
            .get("envelope")
            .filter(|envelope| envelope.is_object())
            .ok_or_else(|| fault("invalid argument envelope: is required"))?;
        let bound = self.lock().bound_client_id.clone();
        if envelope["clientId"].as_str() != bound.as_deref() {
            return Err(fault("fault.command.clientMismatch"));
        }
        let process = self.runtime.get_ready_client(&target).await?;
        self.touch(&target);
        if envelope["type"] == "createSession" {
            self.runtime
                .before_use(&process, UseReason::CreateSession)
                .await?;
        }
        let payload = &envelope["payload"];
        let plan_enabled = payload["planEnabled"] == json!(true)
            || payload["config"]["planEnabled"] == json!(true)
            || payload["firstInput"]["planEnabled"] == json!(true);
        if plan_enabled
            && process
                .capabilities()
                .await
                .is_none_or(|capabilities| capabilities["independentPlanState"] != json!(true))
        {
            return Err(fault("proto.independentPlanUnsupported"));
        }
        let envelope = self.command_envelope(&target, envelope.clone()).await;
        Ok(raw_reply(process.request("v4/command", &envelope).await?))
    }

    /// buildConversationCommandEnvelope + ttft 剥离 + 浏览器环境（docs/specs/rust-host-agent.md §2.3）。
    async fn command_envelope(&self, target: &WorkspaceTarget, mut envelope: Value) -> Value {
        let extensions = self.runtime.extensions();
        match envelope["type"].as_str() {
            Some("createSession") => {
                let off_peak = extensions.off_peak_tools_for(target);
                let dynamic_workflow = extensions.dynamic_workflow_enabled().await;
                if let Some(payload) = envelope["payload"].as_object_mut() {
                    if off_peak {
                        payload.insert("offPeakToolEnabled".into(), json!(true));
                    }
                    if dynamic_workflow {
                        payload.insert("dynamicWorkflowEnabled".into(), json!(true));
                    }
                }
            }
            Some("sendText") => {
                let payload = &envelope["payload"];
                // 只认本轮 payload：automation 派发轮禁用 Cron 变更工具，闲时派发轮禁用 OffPeakCreate。
                let denied: &[&str] = if truthy_str(&payload["automationId"]) {
                    &["CronCreate", "CronUpdate", "CronDelete"]
                } else if truthy_str(&payload["offPeakTaskId"]) {
                    &["OffPeakCreate"]
                } else {
                    &[]
                };
                if !denied.is_empty() {
                    let merged = merge_denylist(&payload["toolDisallowlist"], denied);
                    envelope["payload"]["toolDisallowlist"] = merged;
                }
            }
            _ => {}
        }
        // TTFT 只允许可信桌面本地 continuous；远端 workspace 不能开启本地观测。
        let remote_or_replayable = self.client_mode != ClientMode::DesktopContinuous
            || target
                .workspace_identity
                .as_deref()
                .is_some_and(|identity| !identity.trim().is_empty())
            || target.remote_session_id.is_some();
        if remote_or_replayable && let Some(object) = envelope.as_object_mut() {
            object.remove("ttft");
        }
        if envelope["type"] == "sendText"
            && let Some(session_id) = envelope["sessionId"].as_str().filter(|id| !id.is_empty())
            && let Some(browser) = &self.api.browser
            && let Some(ambient) = browser
                .ambient_context(session_id, target, client_mode_str(self.client_mode))
                .await
            && envelope["payload"].is_object()
        {
            envelope["payload"]["browserAmbientContext"] = ambient;
        }
        envelope
    }

    /// `backgroundBashOutputV4`：只观察现有进程；旧 CLI / 无进程时回稳定的 kind。
    async fn background_bash_output(&self, params: Value) -> Result<Reply, RpcError> {
        self.assert_ready()?;
        let target = WorkspaceTarget::from_value(&params)?;
        let session_id = required_str(&params, "sessionId")?;
        let work_id = required_str(&params, "workId")?;
        let request = json!({"sessionId": session_id, "workId": work_id});
        let process = match self
            .runtime
            .get_client(&target, RuntimePolicy::ExistingOnly)
            .await
        {
            Ok(process) => process,
            Err(error) if error.code_str() == Some(RUNTIME_UNAVAILABLE_CODE) => {
                return Ok(Reply::Value(
                    json!({"kind": "unavailable", "workId": work_id}),
                ));
            }
            Err(error) => return Err(error),
        };
        match process
            .request("v4/conversation/backgroundBashOutput", &request)
            .await
        {
            Ok(raw) => Ok(raw_reply(raw)),
            Err(error) if error.code_i64() == Some(-32601) => Ok(Reply::Value(
                json!({"kind": "unsupported", "workId": work_id}),
            )),
            Err(error) => Err(error),
        }
    }

    async fn query_commands(&self, params: Value) -> Result<Reply, RpcError> {
        self.assert_ready()?;
        let target = WorkspaceTarget::from_value(&params)?;
        let clock = params.get("clock") == Some(&json!(true));
        if !clock {
            let mut state = self.lock();
            let key = target.key();
            if state
                .query_workspace
                .as_ref()
                .is_some_and(|bound| *bound != key)
            {
                return Err(fault("fault.command.queryForeignWorkspace"));
            }
            state.query_workspace = Some(key);
        }
        let mut query = Map::new();
        query.insert(
            "commands".into(),
            params.get("commands").cloned().unwrap_or_else(|| json!([])),
        );
        if clock {
            query.insert("clock".into(), json!(true));
        }
        let process = self
            .runtime
            .get_client(&target, RuntimePolicy::StartIfNeeded)
            .await?;
        Ok(raw_reply(
            process
                .request("v4/commands/query", &Value::Object(query))
                .await?,
        ))
    }

    /// 只读转发：`fields` 从调用参数原样拷贝，`extra` 是 Host 注入的可信字段。
    async fn read_only(
        &self,
        method: &str,
        params: Value,
        fields: &[&str],
        extra: Map<String, Value>,
    ) -> Result<Reply, RpcError> {
        let target = WorkspaceTarget::from_value(&params)?;
        let mut cli = Map::new();
        copy_present(&mut cli, &params, fields);
        cli.extend(extra);
        let process = self
            .runtime
            .get_client(&target, RuntimePolicy::StartIfNeeded)
            .await?;
        Ok(raw_reply(
            process.request(method, &Value::Object(cli)).await?,
        ))
    }

    /// 附件上传事务：走写门禁，并证明 NDJSON 行不超过 CLI 的 1 MiB 上限。
    async fn attachment(
        &self,
        method: &str,
        params: Value,
        fields: &[&str],
    ) -> Result<Reply, RpcError> {
        self.assert_ready()?;
        let target = WorkspaceTarget::from_value(&params)?;
        let mut cli = Map::new();
        cli.insert("connectionId".into(), json!(self.connection_id));
        copy_present(&mut cli, &params, fields);
        let cli = Value::Object(cli);
        let line = json!({"id": "9".repeat(32), "method": method, "params": cli});
        if line.to_string().len() + 1 > MAX_FRAME_BYTES {
            return Err(fault("proto.frameTooLarge"));
        }
        let process = self.runtime.get_ready_client(&target).await?;
        self.touch(&target);
        Ok(raw_reply(process.request(method, &cli).await?))
    }

    /// 连接关闭：通知 CLI 清理该连接的上传状态，再尽力退订全部已拥有的订阅。
    async fn dispose(&self) {
        let (owned, touched, routes) = {
            let mut state = self.lock();
            state.closed = true;
            state.handshake_complete = false;
            state.pending.clear();
            (
                std::mem::take(&mut state.owned),
                std::mem::take(&mut state.touched),
                std::mem::take(&mut state.routes),
            )
        };
        for route in routes.values() {
            route.task.abort();
        }
        for target in touched.values() {
            if let Some(process) = self.runtime.existing(target).await {
                let params = json!({"connectionId": self.connection_id, "state": "closed"});
                if let Err(error) = process.request("v4/connection/flow", &params).await {
                    log::debug(format!("connection flow closed failed: {}", error.message));
                }
            }
        }
        for entry in &owned {
            let Some(target) = touched.get(&entry.workspace_key) else {
                continue;
            };
            if let Some(process) = self.live_process(target, entry).await {
                self.unsubscribe_on_cli(&process, entry).await;
            }
        }
    }
}

pub struct AgentChannel {
    runtime: AgentRuntime,
    api: AgentApi,
    scopes: Mutex<HashMap<u64, Arc<Scope>>>,
    fallback: Option<Arc<dyn ServiceChannel>>,
}

impl AgentChannel {
    pub fn new(api: AgentApi, fallback: Option<Arc<dyn ServiceChannel>>) -> Arc<Self> {
        Arc::new(Self {
            runtime: api.chat.clone(),
            api,
            scopes: Mutex::new(HashMap::new()),
            fallback,
        })
    }

    fn scope(&self, context: &CallContext) -> Arc<Scope> {
        self.scopes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(context.connection)
            .or_insert_with(|| {
                Arc::new(Scope {
                    connection_id: format!("host-rpc-{}", uuid_v4()),
                    client_mode: context.client_mode,
                    runtime: self.runtime.clone(),
                    api: self.api.clone(),
                    state: Mutex::new(ScopeState::default()),
                })
            })
            .clone()
    }

    fn dispatch(&self, scope: Arc<Scope>, method: &str, args: Args) -> Option<CallFuture> {
        let params = args.raw(0).cloned().unwrap_or(Value::Null);
        let runtime = self.runtime.clone();
        let future: CallFuture = match method {
            "helloConversationV4" => Box::pin(async move { scope.hello().map(Reply::Value) }),
            "initializeConversationV4" => {
                Box::pin(async move { scope.initialize(&params).map(|()| Reply::Undefined) })
            }
            "setConnectionFlowStateV4" => Box::pin(async move {
                scope.assert_open()?;
                Err(fault("fault.connection.flowControlForbidden"))
            }),
            "syncAppRuntimePreferences" => Box::pin(async move {
                runtime.sync_app_preferences(params).await?;
                Ok(Reply::Undefined)
            }),
            "initialize" => Box::pin(async move {
                let target = WorkspaceTarget::from_value(&params)?;
                Ok(Reply::Value(runtime.initialize(&target).await))
            }),
            "subscribeConversationV4" => Box::pin(scope.subscribe(FrameKind::Conversation, params)),
            "subscribeSessionsIndexV4" => {
                Box::pin(scope.subscribe(FrameKind::SessionsIndex, params))
            }
            "subscribeWorkspaceConfigV4" => {
                Box::pin(scope.subscribe(FrameKind::WorkspaceConfig, params))
            }
            "unsubscribeWorkspaceConfigV4" => {
                Box::pin(async move { scope.unsubscribe(FrameKind::WorkspaceConfig, params).await })
            }
            "resyncWorkspaceConfigV4" => {
                Box::pin(async move { scope.resync(FrameKind::WorkspaceConfig, params).await })
            }
            "backgroundBashOutputV4" => {
                Box::pin(async move { scope.background_bash_output(params).await })
            }
            "conversationAttachmentReadV4" | "conversationAttachmentStatV4" => {
                let (cli_method, fields): (&str, &'static [&'static str]) =
                    if method == "conversationAttachmentReadV4" {
                        (
                            "v4/conversation/attachmentRead",
                            &[
                                "sessionId",
                                "ref",
                                "target",
                                "attachmentIndex",
                                "offset",
                                "limit",
                            ],
                        )
                    } else {
                        (
                            "v4/conversation/attachmentStat",
                            &["sessionId", "ref", "target", "attachmentIndex"],
                        )
                    };
                Box::pin(async move {
                    scope.assert_ready()?;
                    scope
                        .read_only(cli_method, params, fields, Map::new())
                        .await
                })
            }
            "unsubscribeConversationV4" => {
                Box::pin(async move { scope.unsubscribe(FrameKind::Conversation, params).await })
            }
            "unsubscribeSessionsIndexV4" => {
                Box::pin(async move { scope.unsubscribe(FrameKind::SessionsIndex, params).await })
            }
            "resyncConversationV4" => {
                Box::pin(async move { scope.resync(FrameKind::Conversation, params).await })
            }
            "resyncSessionsIndexV4" => {
                Box::pin(async move { scope.resync(FrameKind::SessionsIndex, params).await })
            }
            "sendConversationCommandV4" => Box::pin(async move { scope.command(params).await }),
            "queryConversationCommandsV4" => {
                Box::pin(async move { scope.query_commands(params).await })
            }
            "conversationRowsRangeV4" => Box::pin(async move {
                scope.assert_ready()?;
                let extra = Map::from_iter([(
                    "clientMode".to_owned(),
                    json!(client_mode_str(scope.client_mode)),
                )]);
                scope
                    .read_only(
                        "v4/conversation/rowsRange",
                        params,
                        &["sessionId", "beforeRowId", "limit"],
                        extra,
                    )
                    .await
            }),
            "conversationPlansV4" => Box::pin(async move {
                scope
                    .read_only("v4/conversation/plans", params, &["sessionId"], Map::new())
                    .await
            }),
            "conversationFileChangesV4" | "conversationFileRewindPreviewV4" => {
                let cli_method = if method == "conversationFileChangesV4" {
                    "v4/conversation/fileChanges"
                } else {
                    "v4/conversation/fileRewindPreview"
                };
                Box::pin(async move {
                    scope
                        .read_only(
                            cli_method,
                            params,
                            &["sessionId", "target", "baseRevision", "baseLogEpoch"],
                            Map::new(),
                        )
                        .await
                })
            }
            "conversationWorkflowRunsV4" => Box::pin(async move {
                scope
                    .read_only(
                        "v4/conversation/workflowRuns",
                        params,
                        &["sessionId", "limit"],
                        Map::new(),
                    )
                    .await
            }),
            "attachmentBeginV4" => Box::pin(async move {
                scope
                    .attachment(
                        "v4/attachment/begin",
                        params,
                        &[
                            "uploadId",
                            "sessionId",
                            "fileName",
                            "mime",
                            "totalBytes",
                            "totalChunks",
                            "checksum",
                        ],
                    )
                    .await
            }),
            "attachmentChunkV4" => Box::pin(async move {
                scope
                    .attachment(
                        "v4/attachment/chunk",
                        params,
                        &["uploadId", "sessionId", "chunkIndex", "dataBase64"],
                    )
                    .await
            }),
            "attachmentCommitV4" => Box::pin(async move {
                scope
                    .attachment("v4/attachment/commit", params, &["uploadId", "sessionId"])
                    .await
            }),
            "attachmentAbortV4" => Box::pin(async move {
                scope
                    .attachment("v4/attachment/abort", params, &["uploadId", "sessionId"])
                    .await
                    .map(|_| Reply::Undefined)
            }),
            "attachmentReadV4" => Box::pin(async move {
                scope.assert_ready()?;
                scope
                    .read_only(
                        "v4/attachment/read",
                        params,
                        &[
                            "sessionId",
                            "ref",
                            "target",
                            "attachmentIndex",
                            "offset",
                            "limit",
                        ],
                        Map::new(),
                    )
                    .await
            }),
            // 本地播放源需要媒体路径授权，Rust Host 暂不提供；UI 回退到分块读取。
            "attachmentPreviewSourceV4" => Box::pin(async move {
                scope.assert_ready()?;
                Ok(Reply::Value(json!({"kind": "chunked"})))
            }),
            _ => return None,
        };
        Some(future)
    }
}

impl ServiceChannel for AgentChannel {
    fn call(&self, context: &CallContext, method: &str, args: Vec<RpcValue>) -> CallFuture {
        let scope = self.scope(context);
        if let Some(future) = self.dispatch(scope, method, Args::from_rpc(args.clone())) {
            return future;
        }
        match &self.fallback {
            Some(fallback) => fallback.call(context, method, args),
            None => {
                let error = fault(&format!("Method not found: {method}"));
                Box::pin(async move { Err(error) })
            }
        }
    }

    fn listen(
        &self,
        context: &CallContext,
        event: &str,
        arg: RpcValue,
    ) -> Result<EventStream, RpcError> {
        match event {
            "onDynamicConversationFrame" => self
                .scope(context)
                .frame_events(FrameKind::Conversation, &arg.into_json()),
            "onDynamicSessionsIndexFrame" => self
                .scope(context)
                .frame_events(FrameKind::SessionsIndex, &arg.into_json()),
            "onDynamicWorkspaceConfigFrame" => self
                .scope(context)
                .frame_events(FrameKind::WorkspaceConfig, &arg.into_json()),
            "onAgentRuntimeLifecycle" => Ok(replies(self.runtime.lifecycle().subscribe())),
            "onAgentRuntimeRestarted" => Ok(replies(self.runtime.restarted().subscribe())),
            _ => match &self.fallback {
                Some(fallback) => fallback.listen(context, event, arg),
                None => Err(fault(&format!("Event not found: {event}"))),
            },
        }
    }

    fn connection_closed(&self, context: &CallContext) {
        let scope = self
            .scopes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&context.connection);
        if let Some(scope) = scope {
            tokio::spawn(async move { scope.dispose().await });
        }
        if let Some(fallback) = &self.fallback {
            fallback.connection_closed(context);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_hello_is_strict() {
        let hello = json!({
            "kind": "clientHello", "protocolVersion": 3, "clientId": "client-1",
            "clientKind": "desktop", "appVersion": "unknown",
            "capabilities": {"workspaceHookReviewUi": true, "workflowRunDeltas": true},
        });
        assert_eq!(
            parse_client_hello(&hello).unwrap(),
            ("client-1".to_owned(), true)
        );
        let mut extra = hello.clone();
        extra["x"] = json!(1);
        assert!(parse_client_hello(&extra).is_err());
        let mut capability = hello.clone();
        capability["capabilities"]["other"] = json!(true);
        assert!(parse_client_hello(&capability).is_err());
        let mut version = hello;
        version["protocolVersion"] = json!(2);
        assert!(parse_client_hello(&version).is_err());
    }

    #[test]
    fn denylists_merge_without_duplicates() {
        assert_eq!(
            merge_denylist(
                &json!(["Bash", "CronDelete", "Bash"]),
                &["CronCreate", "CronUpdate", "CronDelete"]
            ),
            json!(["Bash", "CronDelete", "CronCreate", "CronUpdate"])
        );
        assert_eq!(
            merge_denylist(&Value::Null, &["OffPeakCreate"]),
            json!(["OffPeakCreate"])
        );
    }

    #[test]
    fn uuid_is_v4_shaped() {
        let id = uuid_v4();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
    }
}
