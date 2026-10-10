//! Bot 远端运行时（rust-host-remote.md §2.9）：Node `createBotRemoteWorkspaceService` 与 Main 的 Bot
//! handler 在 Rust Host 里的对应。
//!
//! - [`RemoteBotRuntime`] 实现 `bots::RemoteBots`：连接判定读 Host 自己的注册表（App 级，
//!   rust-u4-app-host.md §2.7），`/reconnect` 仍请 Main 的远端会话管理器执行（它持有 SSH 连接器），
//!   任务能力在注册表里按 (path, identity) 找在线 session，用它的连接；
//! - [`RemotePort`] 是 Bot 的 `TaskPort` 在一条远端连接上的实现：zcode-task 走与 Renderer 相同的
//!   wrapper（附件物化、运行中任务），会话读取用远端 V4 帧；
//! - [`connection_bots`]：远端 attachment 的 `bots` channel 背后的 Bots 实例（每条连接一个）。
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use futures_util::{FutureExt, StreamExt};
use serde_json::{Map, Value, json};
use zcode_async::Spawner;
use zcode_host_control::{ClientMode, ControlRouter};
use zcode_host_kit::{CallContext, Reply, RpcError, RpcValue, ServiceChannel, log};
use zcode_host_rpc::RpcClient;

use super::RemoteHandle;
use super::RemoteWorkspaces;
use super::agent_scope::{RemoteAgentChannel, with_trusted_connection};
use super::registry::ConnState;
use super::tasks::RemoteTaskOps;
use super::transport::Connection;
use crate::account::credential::CredentialStore;
use crate::agent::WorkspaceTarget;
use crate::agent::service::uuid_v4;
use crate::agent::wire::{Assembled, Assembler};
use crate::automation::driver::{SendText, TerminalOutcome, uuid_v7};
use crate::bots::service::{Bots, BotsDeps};
use crate::bots::{FinalReply, NoRemote, RemoteBots, TaskPort, delivery};
use crate::main_bridge::MainBridge;
use crate::settings::SettingStore;

const RECONNECT_REQUEST: &str = "bot-remote-workspace-reconnect-request";
const RECONNECT_RESULT: &str = "bot-remote-workspace-reconnect-result";
const RECONNECT_TIMEOUT: Duration = Duration::from_secs(60);
const WIRE_PROTOCOL_VERSION: u64 = 3;

/// Bot 订阅用的独立 scope 的连接号：从高位分配，避开 host-core 的连接号（同一个
/// `RemoteAgentChannel` 按连接号区分 scope）。
static NEXT_SCOPE: AtomicU64 = AtomicU64::new(1 << 62);

fn message(error: RpcError) -> String {
    error.message
}

/// `{workspacePath, workspaceIdentity?}`。
fn workspace_params(target: &WorkspaceTarget) -> Map<String, Value> {
    let mut params = Map::new();
    params.insert("workspacePath".into(), json!(target.workspace_path));
    if let Some(identity) = target
        .workspace_identity
        .as_deref()
        .map(str::trim)
        .filter(|identity| !identity.is_empty())
    {
        params.insert("workspaceIdentity".into(), json!(identity));
    }
    params
}

fn reply_json(reply: Reply) -> Value {
    match reply {
        Reply::Value(value) => value,
        Reply::Encoded(body) => zcode_host_rpc::Reader::new(&body)
            .value()
            .map(RpcValue::into_json)
            .unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

/// App 运行时偏好的最新值与版本号（Node `latestAppRuntimePreferences` / revision）。
#[derive(Default)]
struct Preferences {
    latest: Option<Value>,
    revision: u64,
}

/// 远端运行时初始化需要的共享状态。
struct Shared {
    settings: SettingStore,
    preferences: Mutex<Preferences>,
}

impl Shared {
    fn preferences(&self) -> std::sync::MutexGuard<'_, Preferences> {
        self.preferences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// 一次就绪检查，并发的使用方共享它的结果。
type ReadyFlight = futures_util::future::Shared<BoxFuture<'static, Result<(), String>>>;

/// `getRuntimeServices` 的就绪闸：远端 Model Selection 与 App 运行时偏好都就绪后才算可用；失败不缓存，
/// 下次使用重试（同 Node 失败时不写缓存）。
struct Readiness {
    shared: Arc<Shared>,
    ready: Arc<AtomicBool>,
    /// 进行中的就绪检查（single-flight）；失败后清掉。原来是跨检查持有的 tokio 锁
    /// （docs/specs/rust-u2-task-scope.md §6.3）。
    flight: Mutex<Option<ReadyFlight>>,
}

/// 就绪检查本身：确认远端 Model Selection 就绪，再推 App 运行时偏好（推送期间版本变了就再推）。
async fn check_ready(
    client: RpcClient,
    shared: Arc<Shared>,
    ready: Arc<AtomicBool>,
) -> Result<(), String> {
    client
        .call("model-selection", "getView", Vec::new())
        .await
        .map_err(message)?;
    loop {
        let (revision, cached) = {
            let preferences = shared.preferences();
            (preferences.revision, preferences.latest.clone())
        };
        let preferences = match cached {
            Some(preferences) => preferences,
            None => {
                let settings = shared.settings.get().await;
                json!({
                    "askUserQuestionAutoResolutionEnabled":
                        settings["askUserQuestionAutoResolutionEnabled"] != false,
                    "modelIoFullRetentionEnabled": settings["modelIoFullRetentionEnabled"] == true,
                })
            }
        };
        client
            .call(
                "zcode-agent",
                "syncAppRuntimePreferences",
                vec![RpcValue::Json(preferences)],
            )
            .await
            .map_err(message)?;
        if revision == shared.preferences().revision {
            break;
        }
    }
    ready.store(true, Ordering::SeqCst);
    Ok(())
}

struct PortInner {
    /// 远端连接的作用域：Bot 的会话帧转发在这里。
    spawner: Spawner,
    client: RpcClient,
    tasks: Arc<RemoteTaskOps>,
    agent: Arc<RemoteAgentChannel>,
    /// Bot 自己的 V4 身份：命令信封的 clientId、直接调用的可信 carrier 与订阅 scope 的 attachmentId。
    bot_id: String,
    /// workspaceKey → 最近一次调用的 target（`list` / `deleted_ids` 只拿到 key）。
    targets: Mutex<HashMap<String, WorkspaceTarget>>,
    readiness: Option<Readiness>,
}

/// Bot 的 `TaskPort` 在一条远端连接上的实现（Node Bot 拿到的远端 `IZCodeTaskService`、
/// `IModelSelectionService` 与 `IZCodeAgentService`）。
pub struct RemotePort(Arc<PortInner>);

impl RemotePort {
    fn new(handle: &RemoteHandle, shared: Option<Arc<Shared>>) -> Self {
        Self(Arc::new(PortInner {
            spawner: handle.connection.spawner(),
            client: handle.connection.client.clone(),
            tasks: handle.tasks.clone(),
            agent: handle.agent.clone(),
            bot_id: format!("host-bots-{}", uuid_v4()),
            targets: Mutex::new(HashMap::new()),
            readiness: shared.map(|shared| Readiness {
                shared,
                ready: Arc::new(AtomicBool::new(false)),
                flight: Mutex::new(None),
            }),
        }))
    }

    fn is_ready(&self) -> bool {
        self.0
            .readiness
            .as_ref()
            .is_some_and(|readiness| readiness.ready.load(Ordering::SeqCst))
    }
}

impl PortInner {
    fn remember(&self, target: &WorkspaceTarget) {
        self.targets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(target.key(), target.clone());
    }

    /// key 对应的 target；没见过时把 key 当路径（远端按 `identity || path` 求同一个 key）。
    fn target_for_key(&self, key: &str) -> WorkspaceTarget {
        self.targets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .cloned()
            .unwrap_or_else(|| WorkspaceTarget::local(key.to_owned()))
    }

    /// 首次使用前：确认远端 Model Selection 就绪，再推 App 运行时偏好。并发的首次使用共享同一次检查；
    /// 失败后清掉这一次，下次使用重试。
    async fn ready(&self) -> Result<(), String> {
        let Some(readiness) = &self.readiness else {
            return Ok(());
        };
        if readiness.ready.load(Ordering::SeqCst) {
            return Ok(());
        }
        let flight = readiness
            .flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert_with(|| {
                check_ready(
                    self.client.clone(),
                    readiness.shared.clone(),
                    readiness.ready.clone(),
                )
                .boxed()
                .shared()
            })
            .clone();
        let result = flight.clone().await;
        if result.is_err() {
            let mut current = readiness
                .flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if current
                .as_ref()
                .is_some_and(|current| current.ptr_eq(&flight))
            {
                *current = None;
            }
        }
        result
    }

    fn carrier(&self) -> Value {
        json!({"connectionId": self.bot_id, "clientMode": "desktop-continuous"})
    }

    async fn task_call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.ready().await?;
        self.tasks.call(method, params).await.map_err(message)
    }

    async fn read_session(&self, target: &WorkspaceTarget, task_id: &str) -> Result<Value, String> {
        let mut params = workspace_params(target);
        params.insert("sessionId".into(), json!(task_id));
        self.client
            .call(
                "zcode-agent",
                "readSession",
                vec![RpcValue::Json(Value::Object(params))],
            )
            .await
            .map(RpcValue::into_json)
            .map_err(message)
    }

    async fn send(
        &self,
        target: WorkspaceTarget,
        task_id: String,
        input: SendText,
        attachments: Option<Vec<Value>>,
    ) -> Result<(), String> {
        self.ready().await?;
        self.remember(&target);
        // Node Bot 的 sendPrompt 参数：taskId、traceId、content、attachments?、botDeliveryTarget?、modelSelection?。
        let mut params = Map::new();
        params.insert("taskId".into(), json!(task_id));
        params.insert("traceId".into(), json!(input.input_id));
        params.insert("content".into(), json!(input.text));
        if let Some(attachments) = attachments.filter(|items| !items.is_empty()) {
            params.insert("attachments".into(), json!(attachments));
        }
        if let Some(delivery) = input.bot_delivery_target {
            params.insert("botDeliveryTarget".into(), delivery);
        }
        if let Some(selection) = input.model_selection {
            params.insert("modelSelection".into(), selection);
        }
        self.tasks
            .send_prompt("sendPrompt", Value::Object(params))
            .await
            .map(|_| ())
            .map_err(message)
    }
}

/// 一个订阅用的独立 scope：丢弃时按连接关闭清理（`closed` 流控、反向退订）。
struct ScopeGuard {
    agent: Arc<RemoteAgentChannel>,
    context: CallContext,
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        self.agent.connection_closed(&self.context);
    }
}

/// 订阅远端 `conversation/<taskId>`：在该连接的 `RemoteAgentChannel` 上开一个独立 scope（握手在 Host
/// 本地完成），重组成逻辑帧交给消费者；seq 缺口或重组失败时 forceSnapshot resync。消费者丢弃流、远端
/// runtime unavailable 或连接关闭时结束。
async fn subscribe(
    inner: Arc<PortInner>,
    target: WorkspaceTarget,
    task_id: String,
) -> Result<BoxStream<'static, Value>, String> {
    inner.ready().await?;
    inner.remember(&target);
    let scope = ScopeGuard {
        agent: inner.agent.clone(),
        context: CallContext {
            connection: NEXT_SCOPE.fetch_add(1, Ordering::SeqCst),
            attachment_id: inner.bot_id.clone(),
            client_mode: ClientMode::DesktopContinuous,
            window_id: None,
        },
    };
    let agent = scope.agent.clone();
    let context = scope.context.clone();
    agent
        .call(&context, "helloConversationV4", Vec::new())
        .await
        .map_err(message)?;
    agent
        .call(
            &context,
            "initializeConversationV4",
            vec![RpcValue::Json(json!({
                "kind": "clientHello",
                "protocolVersion": WIRE_PROTOCOL_VERSION,
                "clientId": inner.bot_id,
                "appVersion": "host-bots",
            }))],
        )
        .await
        .map_err(message)?;
    let mut params = workspace_params(&target);
    let mut frames = agent
        .listen(
            &context,
            "onDynamicConversationFrame",
            RpcValue::Json(Value::Object(params.clone())),
        )
        .map_err(message)?;
    params.insert("sessionId".into(), json!(task_id));
    let ack = reply_json(
        agent
            .call(
                &context,
                "subscribeConversationV4",
                vec![RpcValue::Json(Value::Object(params.clone()))],
            )
            .await
            .map_err(message)?,
    );
    let subscription_id = ack["ack"]["subscriptionId"]
        .as_str()
        .ok_or("subscribe ack without subscriptionId")?
        .to_owned();
    let key = target.key();
    let mut lifecycle = Box::pin(
        inner
            .client
            .listen(
                "zcode-agent",
                "onAgentRuntimeLifecycle",
                RpcValue::Undefined,
            )
            .filter(move |event| {
                let event = event.clone().into_json();
                std::future::ready(event["state"] == "unavailable" && event["workspaceKey"] == key)
            }),
    );
    let client = inner.client.clone();
    // 交给 Bot 消费者的逻辑帧队列有界，满了转发任务等待（与本地 Bot 相同，见
    // `crate::bots::subscribe_conversation`）；转发任务在远端连接的作用域里。
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<Value>(1024);
    let spawner = inner.spawner.clone();
    drop(spawner.spawn("remote-bot-conversation-frames", async move {
        let _scope = scope;
        let mut assembler = Assembler::new();
        let mut cursor: Option<u64> = None;
        loop {
            let frame = tokio::select! {
                frame = frames.next() => frame,
                _ = lifecycle.next() => None,
                () = client.closed() => None,
                () = sender.closed() => None,
            };
            let Some(frame) = frame else { break };
            let wire = reply_json(frame);
            if wire["subscriptionId"] != subscription_id.as_str() {
                continue;
            }
            let logical = match assembler.accept(&wire) {
                Assembled::Pending => continue,
                Assembled::Frame { frame, .. } => Some(frame),
                Assembled::Fault(code) => {
                    log::warn(format!(
                        "bots remote conversation frame fault task={task_id}: {code}"
                    ));
                    None
                }
            };
            let in_order = logical.as_ref().is_some_and(|logical| {
                logical["payload"]["kind"] == "snapshot"
                    || cursor.is_some_and(|seq| logical["fromSeq"].as_u64() == Some(seq))
            });
            if let (Some(logical), true) = (logical, in_order) {
                cursor = logical["toSeq"].as_u64();
                if sender.send(logical).await.is_err() {
                    break;
                }
                continue;
            }
            assembler.reset();
            let mut resync = workspace_params(&target);
            resync.insert("subscriptionId".into(), json!(subscription_id));
            resync.insert("base".into(), Value::Null);
            resync.insert("forceSnapshot".into(), json!(true));
            if agent
                .call(
                    &context,
                    "resyncConversationV4",
                    vec![RpcValue::Json(Value::Object(resync))],
                )
                .await
                .is_err()
            {
                break;
            }
        }
    }));
    Ok(futures_util::stream::poll_fn(move |context| receiver.poll_recv(context)).boxed())
}

impl TaskPort for RemotePort {
    fn create(
        &self,
        target: WorkspaceTarget,
        selection: Value,
        mode: Option<String>,
    ) -> BoxFuture<'static, Result<String, String>> {
        let inner = self.0.clone();
        async move {
            inner.remember(&target);
            // Node Bot：v4 原生建草稿（`v4Create`），模型与模式进 createSession 配置。
            let mut params = workspace_params(&target);
            params.insert("modelSelection".into(), selection);
            if let Some(mode) = mode {
                params.insert("mode".into(), json!(mode));
            }
            params.insert("v4Create".into(), json!(true));
            let meta = inner.task_call("createTask", Value::Object(params)).await?;
            meta["taskId"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| "createTask returned no taskId".to_owned())
        }
        .boxed()
    }

    fn resume(
        &self,
        target: WorkspaceTarget,
        task_id: String,
    ) -> BoxFuture<'static, Result<Option<Value>, String>> {
        let inner = self.0.clone();
        async move {
            inner.remember(&target);
            let mut params = workspace_params(&target);
            params.insert("taskId".into(), json!(task_id));
            inner.task_call("resumeTask", Value::Object(params)).await?;
            let selection = inner
                .task_call("getTaskModelSelection", json!({"taskId": task_id}))
                .await?;
            Ok((!selection.is_null()).then_some(selection))
        }
        .boxed()
    }

    fn send(
        &self,
        target: WorkspaceTarget,
        task_id: String,
        input: SendText,
    ) -> BoxFuture<'static, Result<(), String>> {
        let inner = self.0.clone();
        async move { inner.send(target, task_id, input, None).await }.boxed()
    }

    fn send_with_attachments(
        &self,
        target: WorkspaceTarget,
        task_id: String,
        input: SendText,
        attachments: Vec<Value>,
    ) -> BoxFuture<'static, Result<(), String>> {
        let inner = self.0.clone();
        async move { inner.send(target, task_id, input, Some(attachments)).await }.boxed()
    }

    fn stop(
        &self,
        target: WorkspaceTarget,
        task_id: String,
    ) -> BoxFuture<'static, Result<(), String>> {
        let inner = self.0.clone();
        async move {
            inner.remember(&target);
            inner
                .task_call("stopGeneration", json!({"taskId": task_id}))
                .await
                .map(|_| ())
        }
        .boxed()
    }

    fn list(&self, workspace_key: String) -> BoxFuture<'static, Vec<Value>> {
        let inner = self.0.clone();
        async move {
            let target = inner.target_for_key(&workspace_key);
            match inner
                .task_call("listTasks", Value::Object(workspace_params(&target)))
                .await
            {
                Ok(Value::Array(tasks)) => tasks,
                Ok(_) => Vec::new(),
                Err(error) => {
                    log::warn(format!(
                        "bots remote listTasks failed workspace={workspace_key}: {error}"
                    ));
                    Vec::new()
                }
            }
        }
        .boxed()
    }

    fn deleted_ids(&self, workspace_key: String) -> BoxFuture<'static, Vec<String>> {
        let inner = self.0.clone();
        async move {
            let target = inner.target_for_key(&workspace_key);
            inner
                .task_call(
                    "listDeletedTaskIds",
                    Value::Object(workspace_params(&target)),
                )
                .await
                .ok()
                .and_then(|ids| serde_json::from_value(ids).ok())
                .unwrap_or_default()
        }
        .boxed()
    }

    /// 订阅不可用时的兜底：远端 task ready（运行中任务结束）时按 `readSession` 的状态给终态。
    fn terminal_outcomes(&self) -> BoxStream<'static, TerminalOutcome> {
        let inner = self.0.clone();
        self.0
            .tasks
            .tracker()
            .ready_events()
            .subscribe()
            .then(move |(workspace_key, session_id)| {
                let inner = inner.clone();
                async move {
                    let target = inner.target_for_key(&workspace_key);
                    let failed =
                        inner
                            .read_session(&target, &session_id)
                            .await
                            .is_ok_and(|snapshot| {
                                crate::tasks::meta::meta_from_snapshot(&snapshot)["status"]
                                    == "error"
                            });
                    TerminalOutcome {
                        workspace_key,
                        session_id,
                        outcome: if failed { "failed" } else { "succeeded" },
                        input_id: None,
                    }
                }
            })
            .boxed()
    }

    fn final_reply(
        &self,
        target: WorkspaceTarget,
        task_id: String,
    ) -> BoxFuture<'static, FinalReply> {
        let inner = self.0.clone();
        async move {
            let mut params = workspace_params(&target);
            params.insert("sessionId".into(), json!(task_id));
            params.insert("limit".into(), json!(200));
            let forwarded = with_trusted_connection(&Value::Object(params), &inner.carrier());
            match inner
                .client
                .call(
                    "zcode-agent",
                    "conversationRowsRangeV4",
                    vec![RpcValue::Json(forwarded)],
                )
                .await
            {
                Ok(page) => {
                    let page = page.into_json();
                    delivery::final_reply_from_rows(
                        page["rows"]
                            .as_array()
                            .map(Vec::as_slice)
                            .unwrap_or_default(),
                    )
                }
                Err(error) => {
                    log::warn(format!(
                        "bots read remote final reply failed task={task_id}: {}",
                        error.message
                    ));
                    FinalReply::default()
                }
            }
        }
        .boxed()
    }

    /// 远端 `modelSelectionService.getView`；远端不可用或失败时为 null（Node `readModelSelectionView`）。
    fn model_view(&self, input: Option<Value>) -> BoxFuture<'static, Value> {
        let inner = self.0.clone();
        async move {
            if inner.ready().await.is_err() {
                return Value::Null;
            }
            let args = input.map(RpcValue::Json).into_iter().collect();
            match inner.client.call("model-selection", "getView", args).await {
                Ok(view) => view.into_json(),
                Err(error) => {
                    log::warn(format!(
                        "read remote model selection view for bot failed: {}",
                        error.message
                    ));
                    Value::Null
                }
            }
        }
        .boxed()
    }

    fn read_session(
        &self,
        target: WorkspaceTarget,
        task_id: String,
    ) -> BoxFuture<'static, Result<Value, String>> {
        let inner = self.0.clone();
        async move {
            inner.ready().await?;
            inner.remember(&target);
            inner.read_session(&target, &task_id).await
        }
        .boxed()
    }

    fn config_options(
        &self,
        target: WorkspaceTarget,
        task_id: String,
    ) -> BoxFuture<'static, Result<Value, String>> {
        let inner = self.0.clone();
        async move {
            inner.remember(&target);
            inner
                .task_call("getTaskConfigOptions", json!({"taskId": task_id}))
                .await
        }
        .boxed()
    }

    fn set_model(
        &self,
        target: WorkspaceTarget,
        task_id: String,
        selection: Value,
    ) -> BoxFuture<'static, Result<(), String>> {
        let inner = self.0.clone();
        async move {
            inner.remember(&target);
            inner
                .task_call(
                    "setModel",
                    json!({"taskId": task_id, "traceId": uuid_v4(), "modelSelection": selection}),
                )
                .await
                .map(|_| ())
        }
        .boxed()
    }

    fn set_thought_level(
        &self,
        target: WorkspaceTarget,
        task_id: String,
        level: String,
    ) -> BoxFuture<'static, Result<(), String>> {
        let inner = self.0.clone();
        async move {
            inner.remember(&target);
            inner
                .task_call(
                    "setConfigOption",
                    json!({"taskId": task_id, "traceId": uuid_v4(),
                           "configId": "thought_level", "value": level}),
                )
                .await
                .map(|_| ())
        }
        .boxed()
    }

    fn resolve_interaction(
        &self,
        target: WorkspaceTarget,
        task_id: String,
        interaction_id: String,
        answer: Value,
    ) -> BoxFuture<'static, Result<(), String>> {
        let inner = self.0.clone();
        async move {
            inner.ready().await?;
            inner.remember(&target);
            let mut envelope = Map::new();
            envelope.insert("commandId".into(), json!(uuid_v7()));
            envelope.insert("clientId".into(), json!(inner.bot_id));
            envelope.insert("sessionId".into(), json!(task_id));
            envelope.insert("type".into(), json!("resolveInteraction"));
            envelope.insert(
                "payload".into(),
                json!({"interactionId": interaction_id, "answer": answer}),
            );
            envelope.insert("issuedAt".into(), json!(crate::bots::now_ms()));
            let mut params = workspace_params(&target);
            params.insert("envelope".into(), Value::Object(envelope));
            let forwarded = with_trusted_connection(&Value::Object(params), &inner.carrier());
            let ack = inner
                .client
                .call(
                    "zcode-agent",
                    "sendConversationCommandV4",
                    vec![RpcValue::Json(forwarded)],
                )
                .await
                .map_err(message)?
                .into_json();
            match ack["status"].as_str() {
                Some("accepted" | "duplicate" | "noop") => Ok(()),
                status => Err(format!(
                    "v4 command resolveInteraction {}{}",
                    status.unwrap_or("failed"),
                    ack["message"]
                        .as_str()
                        .map(|message| format!(": {message}"))
                        .unwrap_or_default()
                )),
            }
        }
        .boxed()
    }

    fn subscribe(
        &self,
        target: WorkspaceTarget,
        task_id: String,
    ) -> BoxFuture<'static, Result<BoxStream<'static, Value>, String>> {
        subscribe(self.0.clone(), target, task_id).boxed()
    }
}

/// 远端 attachment 的 `bots` channel 背后的 Bots 实例（Node 远端集合的 `createBotsService`）：配置、
/// 凭据、设置与附件目录同本地实例，任务能力是这条连接的远端 zcode-task；不起通道运行时，也没有远端
/// 重连服务（Node 远端集合不注入 `remoteWorkspaceService`）。
pub(super) fn connection_bots(local: &Arc<Bots>, handle: &RemoteHandle) -> Arc<Bots> {
    Bots::new(BotsDeps {
        // 这条连接的 Bots 实例的任务随连接结束。
        spawner: handle.connection.spawner(),
        repo: local.repo.clone(),
        credentials: local.credentials.clone(),
        providers: local.providers.clone(),
        settings: local.settings.clone(),
        tasks: Arc::new(RemotePort::new(handle, None)),
        remote: Arc::new(NoRemote),
        attachments_dir: local.attachments_dir.clone(),
        background: false,
    })
}

struct Cached {
    connection: Weak<Connection>,
    port: Arc<RemotePort>,
}

/// `BotRemoteWorkspaceService` 的 Rust 实现（rust-host-remote.md §2.9）。
pub struct RemoteBotRuntime {
    me: Weak<RemoteBotRuntime>,
    remote: Weak<RemoteWorkspaces>,
    credentials: CredentialStore,
    main: MainBridge,
    shared: Arc<Shared>,
    /// workspaceKey → 该连接上的端口（连接换代后丢弃）。
    runtimes: Mutex<HashMap<String, Cached>>,
}

impl RemoteBotRuntime {
    pub(super) fn new(
        remote: Weak<RemoteWorkspaces>,
        settings: SettingStore,
        credentials: CredentialStore,
        main: MainBridge,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            remote,
            credentials,
            main,
            shared: Arc::new(Shared {
                settings,
                preferences: Mutex::new(Preferences::default()),
            }),
            runtimes: Mutex::new(HashMap::new()),
        })
    }

    /// Main → Host 的重连结果按 requestId 回到等待者。
    pub(super) fn register(&self, router: &mut ControlRouter) {
        let main = self.main.clone();
        router.on(RECONNECT_RESULT, move |message| {
            if let Some(request_id) = message["requestId"].as_str() {
                main.resolve(request_id, message.clone());
            }
        });
    }

    fn runtimes(&self) -> std::sync::MutexGuard<'_, HashMap<String, Cached>> {
        self.runtimes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `buildRemoteTargetForWorkspace`：按 identity（再按 path + identity）在 `lastWorkspaceSession` 里找
    /// 远端连接信息；SSH 的密码与私钥口令从凭据存储读出（只进 Main 消息，不进日志）。
    async fn remote_target(&self, target: &WorkspaceTarget) -> Result<Option<Value>, String> {
        let settings = self.shared.settings.get().await;
        let identity = target
            .workspace_identity
            .as_deref()
            .unwrap_or_default()
            .trim();
        let sessions: Vec<&Value> = settings["lastWorkspaceSession"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|entry| entry["kind"] == "remote")
            .collect();
        // UI 建连后可能把 workspacePath 规范化为 realpath，而 Bot 上下文仍是旧路径：先按 identity 命中。
        let entry = sessions
            .iter()
            .find(|entry| entry["workspaceIdentity"] == identity)
            .or_else(|| {
                sessions.iter().find(|entry| {
                    entry["workspacePath"] == target.workspace_path.as_str()
                        && entry["workspaceIdentity"] == identity
                })
            });
        let Some(entry) = entry else {
            return Ok(None);
        };
        let raw = &entry["target"];
        if raw["kind"] != "ssh" {
            return Ok(Some(raw.clone()));
        }
        let mut ssh = Map::new();
        ssh.insert("kind".into(), json!("ssh"));
        for key in ["host", "port", "username", "privateKeyPath"] {
            if !raw[key].is_null() {
                ssh.insert(key.into(), raw[key].clone());
            }
        }
        for (reference, field) in [
            ("passwordCredentialKey", "password"),
            ("privateKeyPassphraseCredentialKey", "privateKeyPassphrase"),
        ] {
            let Some(key) = raw[reference].as_str() else {
                continue;
            };
            if let Some(secret) = self
                .credentials
                .load(key)
                .await
                .map_err(|error| error.to_rpc().message)?
            {
                ssh.insert(field.into(), json!(secret));
            }
        }
        Ok(Some(Value::Object(ssh)))
    }

    fn request(
        &self,
        kind: &str,
        request_id: &str,
        target: &WorkspaceTarget,
        remote: Value,
    ) -> Value {
        json!({
            "type": kind,
            "requestId": request_id,
            "workspacePath": target.workspace_path,
            "workspaceIdentity": target.workspace_identity.clone().unwrap_or_default(),
            "target": remote,
        })
    }
}

impl RemoteBots for RemoteBotRuntime {
    fn available(&self) -> bool {
        true
    }

    /// `isConnected`：每次读 Host 自己的注册表（UI 手动重连不经过 Bot，注册表是 live session 的所有者），
    /// 不再问 Main（rust-u4-app-host.md §2.7）。
    fn is_connected(&self, target: WorkspaceTarget) -> BoxFuture<'static, bool> {
        let Some(runtime) = self.me.upgrade() else {
            return async { false }.boxed();
        };
        async move {
            let key = target.key();
            let remote = match runtime.remote_target(&target).await {
                Ok(Some(remote)) => remote,
                Ok(None) => return false,
                Err(error) => {
                    log::warn(format!(
                        "bots remote target unavailable workspace={key}: {error}"
                    ));
                    return false;
                }
            };
            runtime.remote.upgrade().is_some_and(|remote_workspaces| {
                remote_workspaces
                    .registry()
                    .has_connected_session(&key, &remote)
            })
        }
        .boxed()
    }

    /// `ensureConnected`：Main 负责复用已有 route、合并在途重连或新建连接。请求不带 `windowId`：发起方是
    /// Bot 而不是某个窗口，workspace route 属于哪个窗口由 Main 的 route 表决定（rust-u4-app-host.md §4.1）。
    fn ensure_connected(&self, target: WorkspaceTarget) -> BoxFuture<'static, Result<(), String>> {
        let Some(runtime) = self.me.upgrade() else {
            return async { Err("remote reconnect service unavailable".to_owned()) }.boxed();
        };
        async move {
            let Some(remote) = runtime.remote_target(&target).await? else {
                return Err("未找到该远端 workspace 的连接信息。".to_owned());
            };
            let request_id = format!("bot-reconnect-{}", uuid_v4());
            let message = runtime.request(RECONNECT_REQUEST, &request_id, &target, remote);
            let result = runtime
                .main
                .request(request_id, message, RECONNECT_TIMEOUT, |_| {})
                .await
                .ok_or_else(|| "远端 workspace 重连超时。".to_owned())?;
            if result["ok"] == true {
                return Ok(());
            }
            Err(result["error"].as_str().unwrap_or("unknown").to_owned())
        }
        .boxed()
    }

    /// `getZCodeTaskService`：注册表里 (path, identity) 精确匹配的在线 session 的连接；绝不按路径单独匹配。
    fn tasks(&self, target: &WorkspaceTarget) -> Option<Arc<dyn TaskPort>> {
        let remote = self.remote.upgrade()?;
        let identity = target
            .workspace_identity
            .as_deref()
            .filter(|identity| !identity.trim().is_empty())?;
        let session = remote
            .registry()
            .find_session_for_workspace(&target.workspace_path, Some(identity))
            .ok()??;
        if session.state != ConnState::Online || !session.online {
            return None;
        }
        let handle = remote.registry().resolve_scoped(&session.scope()?).ok()?;
        let key = target.key();
        let mut runtimes = self.runtimes();
        if let Some(cached) = runtimes.get(&key)
            && std::ptr::eq(cached.connection.as_ptr(), Arc::as_ptr(&handle.connection))
        {
            return Some(cached.port.clone());
        }
        // 修复 Node 缺陷：Node 按 workspaceKey 永久缓存远端运行时，重连后仍用已关闭的旧端口。这里
        // 记住连接，换代后重新建端口并重新初始化。
        let port = Arc::new(RemotePort::new(&handle, Some(self.shared.clone())));
        port.0.remember(target);
        runtimes.insert(
            key,
            Cached {
                connection: Arc::downgrade(&handle.connection),
                port: port.clone(),
            },
        );
        Some(port)
    }

    /// 只推给已经就绪的远端运行时，不为闲置 Bot 新建远端连接；初始化中的运行时看到版本变化会再推。
    fn sync_app_runtime_preferences(&self, preferences: Value) -> BoxFuture<'static, ()> {
        {
            let mut latest = self.shared.preferences();
            latest.latest = Some(preferences.clone());
            latest.revision += 1;
        }
        let clients: Vec<RpcClient> = self
            .runtimes()
            .values()
            .filter(|cached| cached.port.is_ready())
            .map(|cached| cached.port.0.client.clone())
            .collect();
        async move {
            let calls = clients.into_iter().map(|client| {
                let preferences = preferences.clone();
                async move {
                    client
                        .call(
                            "zcode-agent",
                            "syncAppRuntimePreferences",
                            vec![RpcValue::Json(preferences)],
                        )
                        .await
                }
            });
            for result in futures_util::future::join_all(calls).await {
                if let Err(error) = result {
                    log::warn(format!(
                        "sync app runtime preferences to remote bot runtime failed: {}",
                        error.message
                    ));
                }
            }
        }
        .boxed()
    }
}
