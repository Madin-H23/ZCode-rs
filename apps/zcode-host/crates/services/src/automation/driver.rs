//! Host 内部的 task 驱动：Host 自己（而不是 Renderer）发起会话与输入时用它，
//! 对应 Node 的 zcodeTaskServiceAdapter 中 automation、闲时任务用到的那部分（createTask、
//! resumeTask、setConfigOption、setAutomationSessionConfig、sendPrompt、终态事件）。
//!
//! ```text
//! create_session ──► CLI session/create（deferred 持久化）──► tasks-index 置顶 + task_created
//! resume_session ──► CLI session/resume ──► tasks-index 同步（盖 cron / 闲时标记）
//! switch_*       ──► CLI v4/command（CAS：stale 时按 revisionAtDecision 重试，最多 4 次）
//! send_text      ──► CLI v4/command sendText（commandId = inputId）
//! sessions-index 观察者：会话 非终态 → 终态 ──► terminal_outcomes()
//! ```
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::{Stream, StreamExt};
use serde_json::{Map, Value, json};
use zcode_host_kit::{EventHub, RpcError, log};

use crate::agent::extensions::UseReason;
use crate::agent::sessions_index::{IndexEvent, SessionsIndexObserver};
use crate::agent::{AgentProcess, AgentRuntime, WorkspaceTarget};
use crate::tasks::meta::{is_terminal_phase, meta_from_snapshot, searchable_text};
use crate::tasks::service::TaskService;

const CAS_MAX_ATTEMPTS: usize = 4;
pub const AUTOMATION_MUTATION_TOOLS: [&str; 3] = ["CronCreate", "CronUpdate", "CronDelete"];
pub const OFF_PEAK_MUTATION_TOOLS: [&str; 1] = ["OffPeakCreate"];

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

/// uuid v7（48 位毫秒时间戳 + 随机）。
pub fn uuid_v7() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        bytes = (now_ms() as u128 ^ 0x5a5a_5a5a_5a5a_5a5a).to_le_bytes();
    }
    let ts = now_ms() as u64;
    for (index, shift) in [40, 32, 24, 16, 8, 0].into_iter().enumerate() {
        bytes[index] = ((ts >> shift) & 0xff) as u8;
    }
    bytes[6] = 0x70 | (bytes[6] & 0x0f);
    bytes[8] = 0x80 | (bytes[8] & 0x3f);
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

/// `toZCodeMode`：任务模式 → 会话模式；未知值为 None。
pub fn to_session_mode(mode: &str) -> Option<&'static str> {
    match mode {
        "plan" => Some("plan"),
        "edit" => Some("edit"),
        "yolo" => Some("yolo"),
        "auto" => Some("auto"),
        "build" | "autoEdit" => Some("build"),
        _ => None,
    }
}

/// 会话一轮输入的终态（`onDynamicTaskTerminalOutcome` 的来源：sessions-index 非终态 → 终态）。
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalOutcome {
    pub workspace_key: String,
    pub session_id: String,
    /// `succeeded` / `failed`（phase 为 error 时）。
    pub outcome: &'static str,
    /// 该会话最近一次经本驱动发送的输入 id（`activePromptInputIds`）；用于精确匹配某次派发。
    pub input_id: Option<String>,
}

/// `createTask` 中 Host 派发用到的参数。
#[derive(Debug, Clone, Default)]
pub struct CreateSession {
    pub model: Option<Value>,
    pub thought_level: Option<String>,
    /// 任务模式（`ZCodeTaskMode`），发送前经 [`to_session_mode`] 转换。
    pub mode: Option<String>,
    pub automation_id: Option<String>,
    pub off_peak_task_id: Option<String>,
    pub defer_persistence: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ResumeSession {
    pub thought_level: Option<String>,
    pub automation_id: Option<String>,
    pub off_peak_task_id: Option<String>,
}

/// `sendPrompt`（V4 sendText）的参数。
#[derive(Debug, Clone, Default)]
pub struct SendText {
    /// 本轮输入 id，同时作为 commandId。
    pub input_id: String,
    pub text: String,
    pub model_selection: Option<Value>,
    pub model_execution: Option<Value>,
    pub automation_id: Option<String>,
    pub off_peak_task_id: Option<String>,
    pub off_peak_run_type: Option<String>,
    pub bot_delivery_target: Option<Value>,
    pub tool_denylist: Vec<String>,
}

pub struct TaskDriver {
    agent: AgentRuntime,
    tasks: Arc<TaskService>,
    terminal: EventHub<TerminalOutcome>,
    client_id: String,
    /// (workspaceKey, sessionId) → 最近一次输入 id；发送失败时删除（同 TS）。
    active_inputs: Mutex<HashMap<(String, String), String>>,
}

fn command_error(kind: &str, ack: &Value, context: &str) -> RpcError {
    let status = ack["status"].as_str().unwrap_or("failed");
    let reason = ack["reasonCode"]
        .as_str()
        .map(|code| format!(" ({code})"))
        .unwrap_or_default();
    let message = ack["message"]
        .as_str()
        .map(|message| format!(": {message}"))
        .unwrap_or_default();
    RpcError::new(
        "ZCodeV4CommandRejectedError",
        format!("v4 command {kind} {status}{reason}{message} — {context}"),
    )
    .with_code("ZCODE_V4_COMMAND_REJECTED")
}

impl TaskDriver {
    pub fn start(
        agent: AgentRuntime,
        tasks: Arc<TaskService>,
        observer: &SessionsIndexObserver,
    ) -> Arc<Self> {
        let driver = Arc::new(Self {
            agent,
            tasks,
            terminal: EventHub::new(),
            client_id: format!("host-services-{}", uuid_v7()),
            active_inputs: Mutex::new(HashMap::new()),
        });
        let mut events = Box::pin(observer.events().subscribe());
        let weak = Arc::downgrade(&driver);
        tokio::spawn(async move {
            while let Some(event) = events.next().await {
                let Some(driver) = weak.upgrade() else {
                    return;
                };
                driver.observe(&event);
            }
        });
        driver
    }

    fn observe(&self, event: &IndexEvent) {
        let emit = |target: &WorkspaceTarget, previous: Option<&Value>, next: &Value| {
            // 与 syncer 相同：只有基线里真实观察到非终态 → 终态才算一次收口。
            let became_terminal = previous.is_some_and(|previous| {
                !is_terminal_phase(&previous["phase"]) && is_terminal_phase(&next["phase"])
            });
            if !became_terminal {
                return;
            }
            let Some(session_id) = next["sessionId"].as_str() else {
                return;
            };
            let key = (target.key(), session_id.to_owned());
            let input_id = self.inputs().get(&key).cloned();
            self.terminal.emit(TerminalOutcome {
                workspace_key: key.0,
                session_id: key.1,
                input_id,
                outcome: if next["phase"] == "error" {
                    "failed"
                } else {
                    "succeeded"
                },
            });
        };
        match event {
            IndexEvent::Upserted {
                target,
                previous,
                session,
            } => emit(target, previous.as_ref(), session),
            IndexEvent::Snapshot {
                target,
                previous,
                sessions,
                initial: false,
            } => {
                for session in sessions {
                    let id = session["sessionId"].as_str().unwrap_or_default();
                    emit(target, previous.get(id), session);
                }
            }
            _ => {}
        }
    }

    fn inputs(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), String>> {
        self.active_inputs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 订阅终态；必须在发送输入之前订阅，避免快速完成的轮次漏掉。
    pub fn terminal_outcomes(&self) -> impl Stream<Item = TerminalOutcome> + Send + 'static {
        self.terminal.subscribe()
    }

    pub fn tasks(&self) -> &Arc<TaskService> {
        &self.tasks
    }

    async fn client(&self, target: &WorkspaceTarget) -> Result<Arc<AgentProcess>, RpcError> {
        self.agent.get_ready_client(target).await
    }

    /// `createTask`（非 v4Create 分支）：新建会话并写入 tasks-index 顶部，广播 task_created。
    pub async fn create_session(
        &self,
        target: &WorkspaceTarget,
        options: CreateSession,
    ) -> Result<Value, RpcError> {
        let process = self.client(target).await?;
        self.agent
            .before_use(&process, UseReason::CreateSession)
            .await?;
        let mut params = Map::new();
        params.insert("workspace".into(), target.workspace_ref());
        if let Some(mode) = options.mode.as_deref().and_then(to_session_mode) {
            params.insert("mode".into(), json!(mode));
        }
        if let Some(model) = &options.model {
            params.insert("model".into(), model.clone());
        }
        if options.automation_id.is_some() || options.defer_persistence {
            // session_input 有 session 外键：首条 prompt 之前必须由 V4 admission 统一持久化主记录。
            params.insert("persistence".into(), json!("deferred"));
        }
        if let Some(level) = &options.thought_level {
            params.insert("thoughtLevel".into(), json!(level));
        }
        if options.automation_id.is_some() {
            params.insert("titleGenerationEnabled".into(), json!(false));
        }
        // 本地 workspace 下发 Off-Peak 工具面（isOffPeakToolSupported）；灰度与准入在 offPeak/create 校验。
        if crate::off_peak::dispatch::tool_supported(target) {
            params.insert("offPeakToolEnabled".into(), json!(true));
        }
        let snapshot = process
            .request_value("session/create", &Value::Object(params))
            .await?;
        let mut meta = meta_from_snapshot(&snapshot);
        if let Some(object) = meta.as_object_mut() {
            if let Some(id) = &options.automation_id {
                object.insert("cronAutomationId".into(), json!(id));
            }
            if let Some(id) = &options.off_peak_task_id {
                object.insert("offPeakTaskId".into(), json!(id));
            }
        }
        let outcome = self
            .tasks
            .repo()
            .sync_at_grouped_top(meta, None)
            .await
            .map_err(|error| RpcError::new("Error", format!("{error:#}")))?;
        let task_id = outcome.meta["taskId"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        self.tasks
            .emit_list_changed(target, &task_id, "task_created", Some(&outcome.meta), None);
        Ok(outcome.meta)
    }

    /// `resumeTask`：恢复会话（冷恢复会重新水合），同步 tasks-index 并补盖归属标记。
    pub async fn resume_session(
        &self,
        target: &WorkspaceTarget,
        session_id: &str,
        options: ResumeSession,
    ) -> Result<Value, RpcError> {
        let snapshot = self
            .resume_snapshot(target, session_id, options.thought_level.as_deref())
            .await?;
        if snapshot["session"]["sessionKind"] == "subagent_child" {
            return Ok(meta_from_snapshot(&snapshot));
        }
        let mut meta = meta_from_snapshot(&snapshot);
        if let Some(object) = meta.as_object_mut() {
            if let Some(id) = &options.automation_id {
                object.insert("cronAutomationId".into(), json!(id));
            }
            if let Some(id) = &options.off_peak_task_id {
                object.insert("offPeakTaskId".into(), json!(id));
            }
        }
        self.tasks
            .repo()
            .sync(meta, Some(searchable_text(&snapshot)))
            .await
            .map_err(|error| RpcError::new("Error", format!("{error:#}")))
    }

    async fn resume_snapshot(
        &self,
        target: &WorkspaceTarget,
        session_id: &str,
        thought_level: Option<&str>,
    ) -> Result<Value, RpcError> {
        let process = self.client(target).await?;
        self.agent
            .before_use(&process, UseReason::ConversationSubscribe)
            .await?;
        let mut params = Map::new();
        params.insert("sessionId".into(), json!(session_id));
        params.insert("workspace".into(), target.workspace_ref());
        if let Some(level) = thought_level
            .map(str::trim)
            .filter(|level| !level.is_empty())
        {
            params.insert("thoughtLevel".into(), json!(level));
        }
        // 冷恢复同样下发 Off-Peak 工具面，否则恢复出来的会话会丢掉工具簇。
        if crate::off_peak::dispatch::tool_supported(target) {
            params.insert("offPeakToolEnabled".into(), json!(true));
        }
        process
            .request_value("session/resume", &Value::Object(params))
            .await
    }

    async fn sync_snapshot(&self, snapshot: &Value) -> Result<(), RpcError> {
        if snapshot["session"]["sessionKind"] == "subagent_child" {
            return Ok(());
        }
        self.tasks
            .repo()
            .sync(
                meta_from_snapshot(snapshot),
                Some(searchable_text(snapshot)),
            )
            .await
            .map(|_| ())
            .map_err(|error| RpcError::new("Error", format!("{error:#}")))
    }

    fn envelope(
        &self,
        kind: &str,
        session_id: &str,
        payload: Value,
        command_id: Option<&str>,
        base_revision: Option<i64>,
    ) -> Value {
        let mut envelope = Map::new();
        envelope.insert(
            "commandId".into(),
            json!(command_id.map_or_else(uuid_v7, str::to_owned)),
        );
        envelope.insert("clientId".into(), json!(self.client_id));
        envelope.insert("sessionId".into(), json!(session_id));
        if let Some(revision) = base_revision {
            envelope.insert("baseRevision".into(), json!(revision));
        }
        envelope.insert("type".into(), json!(kind));
        envelope.insert("payload".into(), payload);
        envelope.insert("issuedAt".into(), json!(now_ms()));
        Value::Object(envelope)
    }

    /// `sendHostCasCommandV4`：首发 baseRevision=0 探测，stale 时用回报的 revision 重试。
    async fn cas_command(
        &self,
        target: &WorkspaceTarget,
        session_id: &str,
        kind: &str,
        payload: Value,
        context: &str,
    ) -> Result<Value, RpcError> {
        let process = self.client(target).await?;
        let mut base_revision = 0;
        let mut last = Value::Null;
        for _ in 0..CAS_MAX_ATTEMPTS {
            let envelope =
                self.envelope(kind, session_id, payload.clone(), None, Some(base_revision));
            let ack = process.request_value("v4/command", &envelope).await?;
            if ack["status"] == "stale" {
                base_revision = ack["revisionAtDecision"].as_i64().unwrap_or(base_revision);
                last = ack;
                continue;
            }
            return match ack["status"].as_str() {
                Some("accepted" | "duplicate" | "noop") => Ok(ack),
                _ => Err(command_error(kind, &ack, context)),
            };
        }
        Err(command_error(
            kind,
            &last,
            &format!("{context}（连续 stale，放弃重试）"),
        ))
    }

    /// `switchCollaborationModeViaProtocol`：auto 走旧 `session/setMode`，其余 V4。
    pub async fn switch_mode(
        &self,
        target: &WorkspaceTarget,
        session_id: &str,
        mode: &str,
    ) -> Result<(), RpcError> {
        if mode == "auto" {
            let process = self.client(target).await?;
            process
                .request_value(
                    "session/setMode",
                    &json!({"sessionId": session_id, "mode": mode}),
                )
                .await?;
            return Ok(());
        }
        self.cas_command(
            target,
            session_id,
            "switchCollaborationMode",
            json!({"mode": mode}),
            &format!("session={session_id} mode={mode}"),
        )
        .await
        .map(|_| ())
    }

    /// `setConfigOption`（mode / thought_level）：写配置后 resume 刷新快照并同步 tasks-index。
    pub async fn set_config_option(
        &self,
        target: &WorkspaceTarget,
        session_id: &str,
        config_id: &str,
        value: &str,
    ) -> Result<(), RpcError> {
        match config_id {
            "thought_level" => {
                let process = self.client(target).await?;
                let current = process
                    .request_value("session/read", &json!({"sessionId": session_id}))
                    .await?;
                let model = &current["settings"]["model"]["current"];
                if !model.is_object() {
                    return Err(RpcError::new("Error", "请先选择模型，再设置思考档位"));
                }
                self.cas_command(
                    target,
                    session_id,
                    "switchModelConfig",
                    json!({"provider": model["providerId"], "model": model["modelId"], "thought": value}),
                    &format!("session={session_id} thought={value}"),
                )
                .await?;
            }
            "mode" => {
                let mode = to_session_mode(value).unwrap_or("build");
                self.switch_mode(target, session_id, mode).await?;
            }
            _ => {}
        }
        let snapshot = self.resume_snapshot(target, session_id, None).await?;
        self.sync_snapshot(&snapshot).await
    }

    /// `setAutomationSessionConfig`：一条 V4 命令原子切换模型与档位，再显式收敛档位与模式。
    pub async fn set_automation_session_config(
        &self,
        target: &WorkspaceTarget,
        session_id: &str,
        selection: &Value,
        thought_level: Option<&str>,
        mode: Option<&str>,
    ) -> Result<(), RpcError> {
        let thought = thought_level.map(str::trim).unwrap_or_default();
        let payload = json!({
            "provider": selection["providerId"],
            "model": selection["modelId"],
            "thought": thought,
        });
        let model_label = format!(
            "{}/{}",
            selection["providerId"].as_str().unwrap_or_default(),
            selection["modelId"].as_str().unwrap_or_default()
        );
        self.cas_command(
            target,
            session_id,
            "switchModelConfig",
            payload.clone(),
            &format!("automation session={session_id} model={model_label} thought={thought}"),
        )
        .await?;
        if !thought.is_empty() {
            self.cas_command(
                target,
                session_id,
                "switchModelConfig",
                payload,
                &format!("automation session={session_id} thought={thought}"),
            )
            .await?;
        }
        if let Some(mode) = mode.map(str::trim).filter(|mode| !mode.is_empty()) {
            self.switch_mode(target, session_id, to_session_mode(mode).unwrap_or("build"))
                .await?;
        }
        let snapshot = self.resume_snapshot(target, session_id, None).await?;
        self.sync_snapshot(&snapshot).await
    }

    /// `sendPrompt` → V4 sendText（`heldQueueDisposition: keepQueueAndSend`）。本轮 automation /
    /// 闲时归属会在 denylist 里合并对应的变更工具（`sendConversationCommandV4` 的信封处理）。
    pub async fn send_text(
        &self,
        target: &WorkspaceTarget,
        session_id: &str,
        input: SendText,
    ) -> Result<Value, RpcError> {
        let mut denylist: Vec<String> = Vec::new();
        let mut push = |name: &str| {
            if !denylist.iter().any(|existing| existing == name) {
                denylist.push(name.to_owned());
            }
        };
        for name in &input.tool_denylist {
            push(name);
        }
        if input.automation_id.is_some() {
            AUTOMATION_MUTATION_TOOLS.iter().for_each(|name| push(name));
        } else if input.off_peak_task_id.is_some() {
            OFF_PEAK_MUTATION_TOOLS.iter().for_each(|name| push(name));
        }
        let mut payload = Map::new();
        payload.insert("text".into(), json!(input.text));
        payload.insert("heldQueueDisposition".into(), json!("keepQueueAndSend"));
        if let Some(selection) = &input.model_selection {
            payload.insert("modelSelection".into(), selection.clone());
        }
        if let Some(execution) = &input.model_execution {
            payload.insert("modelExecution".into(), execution.clone());
        }
        if let Some(id) = &input.automation_id {
            payload.insert("automationId".into(), json!(id));
        } else if let Some(id) = &input.off_peak_task_id {
            payload.insert("offPeakTaskId".into(), json!(id));
            if let Some(kind) = &input.off_peak_run_type {
                payload.insert("offPeakRunType".into(), json!(kind));
            }
        }
        if let Some(target_value) = &input.bot_delivery_target {
            payload.insert("botDeliveryTarget".into(), target_value.clone());
        }
        if !denylist.is_empty() {
            payload.insert("toolDisallowlist".into(), json!(denylist));
        }
        let envelope = self.envelope(
            "sendText",
            session_id,
            Value::Object(payload),
            Some(&input.input_id),
            None,
        );
        let key = (target.key(), session_id.to_owned());
        self.inputs().insert(key.clone(), input.input_id.clone());
        let result = async {
            let process = self.client(target).await?;
            let ack = process.request_value("v4/command", &envelope).await?;
            match ack["status"].as_str() {
                Some("accepted" | "duplicate" | "noop") => Ok(ack),
                _ => Err(command_error(
                    "sendText",
                    &ack,
                    &format!("session={session_id}"),
                )),
            }
        }
        .await;
        if result.is_err() {
            self.inputs().remove(&key);
        }
        result
    }

    /// `stopGeneration` → V4 stop。
    pub async fn stop(&self, target: &WorkspaceTarget, session_id: &str) -> Result<(), RpcError> {
        let envelope = self.envelope("stop", session_id, json!({}), None, None);
        let process = self.client(target).await?;
        let ack = process.request_value("v4/command", &envelope).await?;
        match ack["status"].as_str() {
            Some("accepted" | "duplicate" | "noop") => Ok(()),
            _ => Err(command_error(
                "stop",
                &ack,
                &format!("session={session_id}"),
            )),
        }
    }

    /// 后台完成统一置未读（失败只记日志，与 Node 的 `void setTaskUnread` 相同）。
    pub async fn mark_unread(&self, target: &WorkspaceTarget, session_id: &str) {
        if let Err(error) = self.tasks.set_unread(target, session_id, true, None).await {
            log::warn(format!(
                "mark background task {session_id} unread failed: {}",
                error.message
            ));
        }
    }
}
