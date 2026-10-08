//! bots（docs/specs/rust-host-bots.md；R6 首版见 rust-host-automation.md 第 7 节）：Bot 配置与状态、
//! 凭据、飞书 / Lark 长连接、Telegram 与微信长轮询、Webhook、入站命令与选择、问答与权限、
//! 流式回复（飞书流式卡片）、automation 终态回推。
//!
//! Bot 读会话不经 legacy 任务快照：快照走 CLI `session/read`，实时走 V4 会话帧（[`stream`]）。
pub mod aes;
pub mod channel;
pub mod delivery;
pub mod feishu;
pub mod inbound;
pub mod interaction;
pub mod model;
pub mod providers;
pub mod reply;
pub mod runtime;
pub mod runtime_lock;
pub mod selection;
pub mod service;
pub mod status;
pub mod store;
pub mod stream;
pub mod telegram;
#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_channels;
#[cfg(test)]
mod tests_commands;
pub mod util;
pub mod watch;
pub mod webhook;
pub mod weixin;

use std::sync::Arc;

use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use futures_util::{FutureExt, StreamExt};
use serde_json::{Map, Value, json};
use zcode_host_control::ControlRouter;
use zcode_host_kit::{ChannelRegistry, log};

use crate::agent::extensions::UseReason;
use crate::agent::runtime::FrameKind;
use crate::agent::wire::{Assembled, Assembler};
use crate::agent::{AgentProcess, AgentRuntime, WorkspaceTarget};
use crate::automation::driver::{CreateSession, ResumeSession, SendText, TerminalOutcome};
use crate::automation::{Area, BotDelivery};
use crate::tasks::repo::RepoFilter;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

/// 一次运行结束时给 Bot 回推的内容：最后一段助手文本与失败原因（若有）。只在会话帧订阅不可用时
/// 作为兜底（见 [`delivery`]）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FinalReply {
    pub text: Option<String>,
    pub error: Option<String>,
}

/// Bot 用到的任务能力（`IZCodeTaskService` 的子集，按 Rust Host 的事实源实现）。生产实现走 automation
/// 区域的 TaskDriver 与 agent runtime，测试用内存实现，入站流程因此可以不启动 CLI 验证。
pub trait TaskPort: Send + Sync {
    /// `createTask`（v4 草稿，Bot 硬锁 yolo）→ 新会话 id。
    fn create(
        &self,
        target: WorkspaceTarget,
        selection: Value,
        mode: Option<String>,
    ) -> BoxFuture<'static, Result<String, String>>;
    /// `resumeTask` + `getTaskModelSelection`。
    fn resume(
        &self,
        target: WorkspaceTarget,
        task_id: String,
    ) -> BoxFuture<'static, Result<Option<Value>, String>>;
    /// V4 sendText。
    fn send(
        &self,
        target: WorkspaceTarget,
        task_id: String,
        input: SendText,
    ) -> BoxFuture<'static, Result<(), String>>;
    /// 带附件的输入（`session/send`，附件为 `ZCodePromptAttachment`）。
    fn send_with_attachments(
        &self,
        target: WorkspaceTarget,
        task_id: String,
        input: SendText,
        attachments: Vec<Value>,
    ) -> BoxFuture<'static, Result<(), String>>;
    fn stop(
        &self,
        target: WorkspaceTarget,
        task_id: String,
    ) -> BoxFuture<'static, Result<(), String>>;
    /// 该 workspace 的任务元数据（tasks-index）。
    fn list(&self, workspace_key: String) -> BoxFuture<'static, Vec<Value>>;
    fn deleted_ids(&self, workspace_key: String) -> BoxFuture<'static, Vec<String>>;
    /// 终态事件流（sessions-index 观察）；会话帧订阅不可用时的兜底。
    fn terminal_outcomes(&self) -> BoxStream<'static, TerminalOutcome>;
    /// 读取一次运行的最终回复（会话最后一轮的助手文本）。
    fn final_reply(
        &self,
        target: WorkspaceTarget,
        task_id: String,
    ) -> BoxFuture<'static, FinalReply>;
    /// `modelSelectionService.getView`（本 Host）。
    fn model_view(&self, input: Option<Value>) -> BoxFuture<'static, Value>;
    /// CLI `session/read`（会话快照：设置、消息、元数据）。
    fn read_session(
        &self,
        target: WorkspaceTarget,
        task_id: String,
    ) -> BoxFuture<'static, Result<Value, String>>;
    /// `getTaskConfigOptions`（经 `session/resume` 的设置投影成旧 configOptions）。
    fn config_options(
        &self,
        target: WorkspaceTarget,
        task_id: String,
    ) -> BoxFuture<'static, Result<Value, String>>;
    /// `setModel`（`session/setModel` 后以 resume 快照写 tasks-index）。
    fn set_model(
        &self,
        target: WorkspaceTarget,
        task_id: String,
        selection: Value,
    ) -> BoxFuture<'static, Result<(), String>>;
    /// `setConfigOption(thought_level)`（V4 `switchModelConfig`）。
    fn set_thought_level(
        &self,
        target: WorkspaceTarget,
        task_id: String,
        level: String,
    ) -> BoxFuture<'static, Result<(), String>>;
    /// `respondPermission` / `respondElicitation`（V4 `resolveInteraction`）。
    fn resolve_interaction(
        &self,
        target: WorkspaceTarget,
        task_id: String,
        interaction_id: String,
        answer: Value,
    ) -> BoxFuture<'static, Result<(), String>>;
    /// 订阅 `conversation/<taskId>`：返回重组后的逻辑帧（第一帧是初始快照）。流被丢弃时退订；
    /// CLI 进程换代时流结束。
    fn subscribe(
        &self,
        target: WorkspaceTarget,
        task_id: String,
    ) -> BoxFuture<'static, Result<BoxStream<'static, Value>, String>>;
}

/// 远端 workspace 的 Bot 运行时接缝（Node `BotRemoteWorkspaceService`，见 spec 第 7 节）。
pub trait RemoteBots: Send + Sync {
    /// 是否注入了远端重连服务（Node `deps.remoteWorkspaceService` 是否存在）。
    fn available(&self) -> bool;
    fn is_connected(&self, target: WorkspaceTarget) -> BoxFuture<'static, bool>;
    fn ensure_connected(&self, target: WorkspaceTarget) -> BoxFuture<'static, Result<(), String>>;
    /// 已连接的远端 workspace 的任务能力；None 时远端请求不能落到本地。
    fn tasks(&self, target: &WorkspaceTarget) -> Option<Arc<dyn TaskPort>>;
    /// `syncAppRuntimePreferences`：把 App 运行时偏好转给远端 Bot 运行时（本地无事可做）。
    fn sync_app_runtime_preferences(&self, _preferences: Value) -> BoxFuture<'static, ()> {
        async {}.boxed()
    }
}

/// 没有远端区域时的实现（单测、只装 automation 的组合）：远端上下文一律视为未连接。
pub struct NoRemote;

impl RemoteBots for NoRemote {
    fn available(&self) -> bool {
        false
    }

    fn is_connected(&self, _: WorkspaceTarget) -> BoxFuture<'static, bool> {
        async { false }.boxed()
    }

    fn ensure_connected(&self, _: WorkspaceTarget) -> BoxFuture<'static, Result<(), String>> {
        async { Err("remote reconnect service unavailable".to_owned()) }.boxed()
    }

    fn tasks(&self, _: &WorkspaceTarget) -> Option<Arc<dyn TaskPort>> {
        None
    }
}

/// 生产实现：automation 区域的 TaskDriver、agent runtime 与 provider 运行时。
struct AreaPort {
    area: Arc<Area>,
    /// Bot 自己的 V4 客户端 id（resolveInteraction 等命令的信封）。
    client_id: String,
}

fn reasoning_level(selection: &Value) -> Option<String> {
    selection["options"]["reasoningLevel"]
        .as_str()
        .filter(|level| !level.is_empty())
        .map(str::to_owned)
}

async fn ready_client(
    agent: &AgentRuntime,
    target: &WorkspaceTarget,
) -> Result<Arc<AgentProcess>, String> {
    agent
        .get_ready_client(target)
        .await
        .map_err(|error| error.message)
}

/// Bot 直接发起的 `session/resume`（`getTaskConfigOptions`：`/new` 继承配置、任务态 `/model`、
/// `/think` 读选项）的参数。Node 经 `resumeSnapshot` → `buildSessionResumeParams` 带上工具面参数
/// （Off-Peak、动态工作流）；缺了它恢复出来的会话会丢工具簇，所以与 automation driver 同源注入。
pub(crate) async fn resume_params(
    agent: &AgentRuntime,
    target: &WorkspaceTarget,
    task_id: &str,
) -> Value {
    let mut params = Map::new();
    params.insert("sessionId".into(), json!(task_id));
    params.insert("workspace".into(), target.workspace_ref());
    params.extend(agent.extensions().session_tool_flags(target).await);
    Value::Object(params)
}

impl AreaPort {
    fn command_envelope(&self, kind: &str, session_id: &str, payload: Value) -> Value {
        let mut envelope = Map::new();
        envelope.insert(
            "commandId".into(),
            json!(crate::automation::driver::uuid_v7()),
        );
        envelope.insert("clientId".into(), json!(self.client_id));
        envelope.insert("sessionId".into(), json!(session_id));
        envelope.insert("type".into(), json!(kind));
        envelope.insert("payload".into(), payload);
        envelope.insert("issuedAt".into(), json!(now_ms()));
        Value::Object(envelope)
    }
}

impl TaskPort for AreaPort {
    fn create(
        &self,
        target: WorkspaceTarget,
        selection: Value,
        mode: Option<String>,
    ) -> BoxFuture<'static, Result<String, String>> {
        let driver = self.area.driver.clone();
        async move {
            let meta = driver
                .create_session(
                    &target,
                    CreateSession {
                        thought_level: reasoning_level(&selection),
                        model: Some(selection),
                        mode,
                        defer_persistence: true,
                        ..CreateSession::default()
                    },
                )
                .await
                .map_err(|error| error.message)?;
            Ok(meta["taskId"].as_str().unwrap_or_default().to_owned())
        }
        .boxed()
    }

    fn resume(
        &self,
        target: WorkspaceTarget,
        task_id: String,
    ) -> BoxFuture<'static, Result<Option<Value>, String>> {
        let driver = self.area.driver.clone();
        let agent = self.area.agent.clone();
        async move {
            driver
                .resume_session(&target, &task_id, ResumeSession::default())
                .await
                .map_err(|error| error.message)?;
            let process = ready_client(&agent, &target).await?;
            let snapshot = process
                .request_value("session/read", &json!({"sessionId": task_id}))
                .await
                .map_err(|error| error.message)?;
            let current = &snapshot["settings"]["model"]["current"];
            Ok((!current.is_null()).then(|| current.clone()))
        }
        .boxed()
    }

    fn send(
        &self,
        target: WorkspaceTarget,
        task_id: String,
        input: SendText,
    ) -> BoxFuture<'static, Result<(), String>> {
        let driver = self.area.driver.clone();
        async move {
            driver
                .send_text(&target, &task_id, input)
                .await
                .map(|_| ())
                .map_err(|error| error.message)
        }
        .boxed()
    }

    fn send_with_attachments(
        &self,
        target: WorkspaceTarget,
        task_id: String,
        input: SendText,
        attachments: Vec<Value>,
    ) -> BoxFuture<'static, Result<(), String>> {
        let agent = self.area.agent.clone();
        async move {
            // buildSessionSendParams 的键序；附件轮次走 legacy session/send（与 zcode-task sendPrompt 相同）。
            let mut send = Map::new();
            send.insert("sessionId".into(), json!(task_id));
            if let Some(selection) = &input.model_selection {
                send.insert("modelSelection".into(), selection.clone());
            }
            send.insert("inputId".into(), json!(input.input_id));
            send.insert("content".into(), json!(input.text));
            send.insert("attachments".into(), json!(attachments));
            if let Some(target_value) = &input.bot_delivery_target {
                send.insert("botDeliveryTarget".into(), target_value.clone());
            }
            let process = ready_client(&agent, &target).await?;
            process
                .request_value("session/send", &Value::Object(send))
                .await
                .map(|_| ())
                .map_err(|error| error.message)
        }
        .boxed()
    }

    fn stop(
        &self,
        target: WorkspaceTarget,
        task_id: String,
    ) -> BoxFuture<'static, Result<(), String>> {
        let driver = self.area.driver.clone();
        async move {
            driver
                .stop(&target, &task_id)
                .await
                .map_err(|error| error.message)
        }
        .boxed()
    }

    fn list(&self, workspace_key: String) -> BoxFuture<'static, Vec<Value>> {
        let repo = self.area.tasks.repo().clone();
        async move {
            repo.list(RepoFilter {
                workspace_key: Some(workspace_key),
                ..RepoFilter::default()
            })
            .await
            .unwrap_or_default()
        }
        .boxed()
    }

    fn deleted_ids(&self, workspace_key: String) -> BoxFuture<'static, Vec<String>> {
        let repo = self.area.tasks.repo().clone();
        async move { repo.deleted_ids(workspace_key).await.unwrap_or_default() }.boxed()
    }

    fn terminal_outcomes(&self) -> BoxStream<'static, TerminalOutcome> {
        self.area.driver.terminal_outcomes().boxed()
    }

    fn final_reply(
        &self,
        target: WorkspaceTarget,
        task_id: String,
    ) -> BoxFuture<'static, FinalReply> {
        let agent = self.area.agent.clone();
        async move {
            let Ok(process) = agent.get_ready_client(&target).await else {
                return FinalReply::default();
            };
            let params =
                json!({"sessionId": task_id, "limit": 200, "clientMode": "desktop-continuous"});
            match process
                .request_value("v4/conversation/rowsRange", &params)
                .await
            {
                Ok(page) => delivery::final_reply_from_rows(
                    page["rows"]
                        .as_array()
                        .map(Vec::as_slice)
                        .unwrap_or_default(),
                ),
                Err(error) => {
                    log::warn(format!(
                        "bots read final reply failed task={task_id}: {}",
                        error.message
                    ));
                    FinalReply::default()
                }
            }
        }
        .boxed()
    }

    fn model_view(&self, input: Option<Value>) -> BoxFuture<'static, Value> {
        let providers = self.area.providers.clone();
        async move {
            let (revision, resolved) = providers.snapshot().await;
            resolved.model_selection_view(revision, input.as_ref())
        }
        .boxed()
    }

    fn read_session(
        &self,
        target: WorkspaceTarget,
        task_id: String,
    ) -> BoxFuture<'static, Result<Value, String>> {
        let agent = self.area.agent.clone();
        async move {
            let process = ready_client(&agent, &target).await?;
            agent
                .before_use(&process, UseReason::SessionRead)
                .await
                .map_err(|error| error.message)?;
            process
                .request_value("session/read", &json!({"sessionId": task_id}))
                .await
                .map_err(|error| error.message)
        }
        .boxed()
    }

    fn config_options(
        &self,
        target: WorkspaceTarget,
        task_id: String,
    ) -> BoxFuture<'static, Result<Value, String>> {
        let agent = self.area.agent.clone();
        async move {
            let process = ready_client(&agent, &target).await?;
            agent
                .before_use(&process, UseReason::SessionResume)
                .await
                .map_err(|error| error.message)?;
            let snapshot = process
                .request_value(
                    "session/resume",
                    &resume_params(&agent, &target, &task_id).await,
                )
                .await
                .map_err(|error| error.message)?;
            Ok(crate::tasks::config_options::settings_to_config_options(
                &snapshot["settings"],
            ))
        }
        .boxed()
    }

    fn set_model(
        &self,
        target: WorkspaceTarget,
        task_id: String,
        selection: Value,
    ) -> BoxFuture<'static, Result<(), String>> {
        let agent = self.area.agent.clone();
        let driver = self.area.driver.clone();
        async move {
            let process = ready_client(&agent, &target).await?;
            process
                .request_value(
                    "session/setModel",
                    &json!({"sessionId": task_id, "model": selection}),
                )
                .await
                .map_err(|error| error.message)?;
            // 以 resume 快照收口并写 tasks-index（同 zcode-task setModel 的 settleConfig）。
            driver
                .resume_session(&target, &task_id, ResumeSession::default())
                .await
                .map(|_| ())
                .map_err(|error| error.message)
        }
        .boxed()
    }

    fn set_thought_level(
        &self,
        target: WorkspaceTarget,
        task_id: String,
        level: String,
    ) -> BoxFuture<'static, Result<(), String>> {
        let driver = self.area.driver.clone();
        async move {
            driver
                .set_config_option(&target, &task_id, "thought_level", &level)
                .await
                .map_err(|error| error.message)
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
        let agent = self.area.agent.clone();
        let envelope = self.command_envelope(
            "resolveInteraction",
            &task_id,
            json!({"interactionId": interaction_id, "answer": answer}),
        );
        async move {
            let process = ready_client(&agent, &target).await?;
            let ack = process
                .request_value("v4/command", &envelope)
                .await
                .map_err(|error| error.message)?;
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
        let agent = self.area.agent.clone();
        async move { subscribe_conversation(agent, target, task_id).await }.boxed()
    }
}

/// 订阅 `conversation/<taskId>`（Bot 自己的连接，desktop-continuous）：后台任务从 runtime 的 V4 帧总线
/// 取本订阅的帧，重组成逻辑帧交给消费者；seq 缺口或重组失败时 forceSnapshot resync。消费者丢弃流、
/// 或 CLI 进程换代时结束并尽力退订。
pub async fn subscribe_conversation(
    agent: AgentRuntime,
    target: WorkspaceTarget,
    task_id: String,
) -> Result<BoxStream<'static, Value>, String> {
    let process = ready_client(&agent, &target).await?;
    agent
        .before_use(&process, UseReason::ConversationSubscribe)
        .await
        .map_err(|error| error.message)?;
    let key = target.key();
    let generation = process.generation;
    let frame_key = key.clone();
    // 先挂上帧流与生命周期，再发订阅：CLI 在响应行之后立即写初始快照。
    let mut frames = Box::pin(agent.frames().subscribe_filtered(move |frame| {
        frame.kind == FrameKind::Conversation
            && frame.workspace_key == frame_key
            && frame.generation == generation
    }));
    let lifecycle_key = key.clone();
    let mut lifecycle = Box::pin(agent.lifecycle().subscribe_filtered(move |event| {
        event["state"] == "unavailable"
            && event["runtimeIdentity"]["generation"].as_u64() == Some(generation)
            && WorkspaceTarget::from_value(event).is_ok_and(|target| target.key() == lifecycle_key)
    }));
    let connection_id = format!("host-bots-{}", util::uuid_v4());
    let topic = format!("conversation/{task_id}");
    let ack = process
        .request_value(
            "v4/conversation/subscribe",
            &json!({"topic": topic, "connectionId": connection_id,
                    "clientMode": "desktop-continuous", "workspace": target.workspace_ref()}),
        )
        .await
        .map_err(|error| error.message)?;
    let subscription_id = ack["ack"]["subscriptionId"]
        .as_str()
        .ok_or("subscribe ack without subscriptionId")?
        .to_owned();
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        let mut assembler = Assembler::new();
        let mut cursor: Option<u64> = None;
        loop {
            let frame = tokio::select! {
                frame = frames.next() => frame,
                _ = lifecycle.next() => None,
                () = sender.closed() => None,
            };
            let Some(frame) = frame else { break };
            if frame.subscription_id != subscription_id {
                continue;
            }
            let Ok(wire) = serde_json::from_str::<Value>(frame.raw.get()) else {
                continue;
            };
            let logical = match assembler.accept(&wire) {
                Assembled::Pending => continue,
                Assembled::Frame { frame, .. } => Some(frame),
                Assembled::Fault(code) => {
                    log::warn(format!(
                        "bots conversation frame fault task={task_id}: {code}"
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
                if sender.send(logical).is_err() {
                    break;
                }
                continue;
            }
            assembler.reset();
            let resync = process
                .request_value(
                    "v4/conversation/resync",
                    &json!({"topic": topic, "connectionId": connection_id,
                            "subscriptionId": subscription_id, "base": null, "forceSnapshot": true}),
                )
                .await;
            if resync.is_err() {
                break;
            }
        }
        if !process.client.is_closed() {
            let _ = process
                .request_value(
                    "v4/conversation/unsubscribe",
                    &json!({"topic": topic, "subscriptionId": subscription_id, "connectionId": connection_id}),
                )
                .await;
        }
    });
    Ok(futures_util::stream::poll_fn(move |context| receiver.poll_recv(context)).boxed())
}

/// automation 的 Bot 回推入口（`watchAutomationRun`）。
struct Delivery {
    bots: Arc<service::Bots>,
}

impl BotDelivery for Delivery {
    fn watch_automation_run(
        &self,
        target: Value,
        task_id: String,
        workspace: WorkspaceTarget,
    ) -> BoxFuture<'static, Result<(), String>> {
        let bots = self.bots.clone();
        async move {
            let params = json!({
                "target": target,
                "taskId": task_id,
                "workspacePath": workspace.workspace_path,
                "workspaceIdentity": workspace.workspace_identity,
            });
            bots.watch_automation_run(&params).await;
            Ok(())
        }
        .boxed()
    }
}

/// 安装 `bots` channel 并按配置启动通道运行时；返回的 [`service::Bots`] 由 automation 的
/// `AreaHandle` 持有，Host 退出时经它调用 [`service::Bots::shutdown`]。
pub fn install(
    area: &Arc<Area>,
    registry: &mut ChannelRegistry,
    _router: &mut ControlRouter,
) -> Option<Arc<service::Bots>> {
    let environment = &area.context.environment;
    let dir = environment.app_config_dir();
    let providers =
        match providers::Providers::new(providers::Endpoints::from_vars(&environment.vars)) {
            Ok(providers) => Arc::new(providers),
            Err(error) => {
                log::error(format!("bots http client unavailable: {error:#}"));
                return None;
            }
        };
    let bots = service::Bots::new(service::BotsDeps {
        repo: store::BotsRepo::new(dir.clone()),
        // 合并 account 区域后统一走它的凭据存储：同一文件只有一个写入方，写入也会广播凭据变更。
        credentials: area.credentials.clone(),
        providers,
        settings: area.settings.clone(),
        tasks: Arc::new(AreaPort {
            area: area.clone(),
            client_id: format!("host-bots-{}", crate::automation::driver::uuid_v7()),
        }),
        // 远端 workspace 的 Bot 运行时由远端区域实现（rust-host-remote.md §2.9），经 Area 注入。
        remote: area.remote_bots.clone(),
        attachments_dir: dir.join("bot-attachments"),
        // Rust Host 只作为桌面本地 Host 运行，总是负责 Bot 长连接与轮询；desktop-attached 远端 Host
        // 是 Node zcode-server，自己按 runStartupBackgroundTasks 决定（docs/specs/rust-host-bots.md §2.3）。
        background: true,
    });
    let _ = area
        .bot_delivery
        .set(Arc::new(Delivery { bots: bots.clone() }));
    registry.implement(service::channel(bots.clone()));
    bots.schedule_refresh();
    Some(bots)
}
