//! `zcode-agent` 的非连接作用域成员（docs/specs/rust-host-agent.md §2.2）：legacy 会话、插件与 MCP、
//! 技能与插件引用目录、已保存工作流、用量、生成文本等。`zcode-session` 与 `plugin-management`
//! 复用同一份实现。
//!
//! lane 选择：会话类成员走 chat lane（只读 client 不过 provider 门禁，写 client 过门禁）；
//! 插件管理走 plugin-management lane；MCP 状态走 mcp-status lane（docs §2.1）。
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};
use zcode_host_kit::{RpcError, log};

use super::browser::BrowserBridge;
use super::extensions::UseReason;
use super::notifications::AgentEvents;
use super::runtime::{
    AGENT_PROVIDER, AgentProcess, AgentRuntime, Lane, MCP_STATUS_IDLE_TIMEOUT,
    RUNTIME_UNAVAILABLE_CODE, RuntimePolicy, WorkspaceTarget,
};
use super::service::uuid_v4;
use super::session_events::SessionEvents;

pub const PLUGIN_WORKSPACE_DIR: &str = "plugin-workspace";
pub const PLUGIN_MANAGEMENT_TIMEOUT: Duration = Duration::from_secs(5 * 60);
pub const PLUGIN_CANCEL_TIMEOUT: Duration = Duration::from_secs(5);
pub const SESSION_COMPACT_TIMEOUT: Duration = Duration::from_secs(5 * 60);
pub const CHILD_PROCESSES_TIMEOUT: Duration = Duration::from_millis(800);
pub const CANCEL_GENERATE_TEXT_TIMEOUT: Duration = Duration::from_secs(5);
pub const MCP_STATUS_MODE_UNSUPPORTED_CODE: &str = "ZCODE_AGENT_MCP_STATUS_MODE_UNSUPPORTED";

const SESSION_CREATE_COMPAT: &[&str] = &[
    "persistence",
    "thoughtLevel",
    "mcpServers",
    "toolAllowlist",
    "toolDenylist",
    "offPeakToolEnabled",
    "dynamicWorkflowEnabled",
];
const SESSION_RESUME_COMPAT: &[&str] = &[
    "thoughtLevel",
    "mcpServers",
    "toolAllowlist",
    "toolDenylist",
    "offPeakToolEnabled",
    "dynamicWorkflowEnabled",
];
const SESSION_SEND_COMPAT: &[&str] = &[
    "browserAmbientContext",
    "automationId",
    "offPeakTaskId",
    "offPeakRunType",
    "botDeliveryTarget",
    "toolDenylist",
];

/// 共享的 agent 实现：chat lane、两个控制面 lane 与事件分发。
#[derive(Clone)]
pub struct AgentApi {
    pub chat: AgentRuntime,
    pub plugin: AgentRuntime,
    pub mcp: AgentRuntime,
    pub events: Arc<AgentEvents>,
    pub session_events: Arc<SessionEvents>,
    pub browser: Option<BrowserBridge>,
    /// `<dataBaseDir>/.zcode/plugin-workspace`：控制面 lane 的固定工作目录。
    pub plugin_workspace: PathBuf,
    /// V4 冷订阅的 `resumeThoughtLevel` 提示来源。
    pub tasks_index: Option<crate::tasks_index::TasksIndex>,
}

pub fn fault(message: impl Into<String>) -> RpcError {
    RpcError::new("Error", message)
}

pub fn invalid(name: &str) -> RpcError {
    zcode_host_kit::args::invalid_argument(name, "is required")
}

pub fn required_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params[key]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid(key))
}

/// `...(params.x !== undefined ? {x} : {})`：键存在且不是 null。
pub fn copy_defined(out: &mut Map<String, Value>, params: &Value, keys: &[&str]) {
    for key in keys {
        if let Some(value) = params.get(*key).filter(|value| !value.is_null()) {
            out.insert((*key).to_owned(), value.clone());
        }
    }
}

/// JS 真值（`...(params.x ? {x} : {})`）：非空字符串、true、非零数、对象 / 数组。
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

pub fn copy_truthy(out: &mut Map<String, Value>, params: &Value, keys: &[&str]) {
    for key in keys {
        if let Some(value) = params.get(*key).filter(|value| truthy(value)) {
            out.insert((*key).to_owned(), value.clone());
        }
    }
}

/// getUnrecognizedTopLevelKeys：只信任 `-32602` 的结构化 issues，不从可变文案猜字段名。
pub fn unrecognized_top_level_keys(error: &RpcError) -> Vec<String> {
    let message = error.message.as_str();
    if error.code_i64() != Some(-32602)
        || !(message == "Invalid params" || message.starts_with("Invalid params — "))
    {
        return Vec::new();
    }
    let Some(serialized) = error
        .data
        .as_deref()
        .and_then(|data| data["message"].as_str())
    else {
        return Vec::new();
    };
    let Ok(Value::Array(issues)) = serde_json::from_str::<Value>(serialized) else {
        return Vec::new();
    };
    let mut keys: Vec<String> = Vec::new();
    for issue in issues {
        let top_level = issue["path"].as_array().is_none_or(Vec::is_empty);
        if issue["code"] != "unrecognized_keys" || !top_level {
            continue;
        }
        for key in issue["keys"].as_array().into_iter().flatten() {
            if let Some(key) = key.as_str()
                && !keys.iter().any(|known| known == key)
            {
                keys.push(key.to_owned());
            }
        }
    }
    keys
}

/// 兼容重试的字段：全部属于允许省略的可选键时才返回，否则为空（不重试）。
fn compat_fields(error: &RpcError, allowed: &[&str]) -> Vec<String> {
    let keys = unrecognized_top_level_keys(error);
    if !keys.is_empty() && keys.iter().all(|key| allowed.contains(&key.as_str())) {
        keys
    } else {
        Vec::new()
    }
}

fn without(mut params: Map<String, Value>, omitted: &[String]) -> Value {
    for key in omitted {
        params.remove(key);
    }
    Value::Object(params)
}

/// formatModelPickerValue：`provider/model`，有推理档位时追加 `$level`。
pub fn format_model(selection: &Value) -> String {
    let (Some(provider), Some(model)) = (
        selection["providerId"].as_str(),
        selection["modelId"].as_str(),
    ) else {
        return String::new();
    };
    match selection["options"]["reasoningLevel"].as_str() {
        Some(level) if !level.is_empty() => format!("{provider}/{model}${level}"),
        _ => format!("{provider}/{model}"),
    }
}

/// 进程取消保护：调用 future 被丢弃（RPC 取消或连接关闭）时通知 CLI 取消生成。
struct CancelOnDrop {
    process: Option<Arc<AgentProcess>>,
    operation_id: String,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let Some(process) = self.process.take() else {
            return;
        };
        let operation_id = std::mem::take(&mut self.operation_id);
        tokio::spawn(async move {
            // 取消是尽力而为的控制面操作：超时不回收进程，失败只记 debug。
            if let Err(error) = process
                .observe_with_timeout(
                    "workspace/cancelGenerateText",
                    &json!({"operationId": operation_id}),
                    CANCEL_GENERATE_TEXT_TIMEOUT,
                )
                .await
            {
                log::debug(format!(
                    "workspace/cancelGenerateText failed: {}",
                    error.message
                ));
            }
        });
    }
}

impl AgentApi {
    pub fn new(
        chat: AgentRuntime,
        data_base_dir: &std::path::Path,
        events: Arc<AgentEvents>,
        browser: Option<BrowserBridge>,
    ) -> Self {
        let plugin = chat.lane(Lane::PluginManagement, None);
        let mcp = chat.lane(Lane::McpStatus, Some(MCP_STATUS_IDLE_TIMEOUT));
        let session_events = SessionEvents::start(chat.clone());
        Self {
            chat,
            plugin,
            mcp,
            events,
            session_events,
            browser,
            plugin_workspace: data_base_dir.join(".zcode").join(PLUGIN_WORKSPACE_DIR),
            tasks_index: None,
        }
    }

    pub fn with_tasks_index(mut self, tasks_index: crate::tasks_index::TasksIndex) -> Self {
        self.tasks_index = Some(tasks_index);
        self
    }

    /// ensurePluginManagementWorkspacePath：控制面 lane 的固定 cwd，按需创建。
    pub async fn plugin_target(&self) -> Result<WorkspaceTarget, RpcError> {
        tokio::fs::create_dir_all(&self.plugin_workspace)
            .await
            .map_err(|error| fault(format!("failed to prepare plugin workspace: {error}")))?;
        Ok(WorkspaceTarget::local(
            self.plugin_workspace.display().to_string(),
        ))
    }

    pub async fn plugin_client(&self) -> Result<Arc<AgentProcess>, RpcError> {
        let target = self.plugin_target().await?;
        self.plugin
            .get_client(&target, RuntimePolicy::StartIfNeeded)
            .await
    }

    pub async fn mcp_client(&self) -> Result<Arc<AgentProcess>, RpcError> {
        let target = self.plugin_target().await?;
        self.mcp
            .get_client(&target, RuntimePolicy::StartIfNeeded)
            .await
    }

    pub async fn read_client(
        &self,
        target: &WorkspaceTarget,
    ) -> Result<Arc<AgentProcess>, RpcError> {
        self.chat
            .get_client(target, RuntimePolicy::StartIfNeeded)
            .await
    }

    /// 按调用参数的 `runtimePolicy` 取只读 client。
    async fn policy_client(
        &self,
        target: &WorkspaceTarget,
        params: &Value,
    ) -> Result<Arc<AgentProcess>, RpcError> {
        self.chat
            .get_client(
                target,
                RuntimePolicy::from_value(params.get("runtimePolicy")),
            )
            .await
    }

    pub async fn write_client(
        &self,
        target: &WorkspaceTarget,
    ) -> Result<Arc<AgentProcess>, RpcError> {
        self.chat.get_ready_client(target).await
    }

    // ── 存储启动 ──

    pub async fn prepare_storage(&self, target: &WorkspaceTarget) -> Result<(), RpcError> {
        let process = self
            .chat
            .acquire(target, RuntimePolicy::StartIfNeeded)
            .await?;
        process
            .client
            .wait_storage_ready()
            .await
            .map_err(super::runtime::client_error)
    }

    pub async fn runtime_identity(&self, target: &WorkspaceTarget) -> Result<Value, RpcError> {
        // 查询 runtime identity 只能观察现有进程，不能把 dormant workspace 变成活动进程。
        match self.chat.existing(target).await {
            Some(process) => Ok(process.runtime_identity()),
            None => {
                Err(fault("ZCode Agent runtime is not running.")
                    .with_code(RUNTIME_UNAVAILABLE_CODE))
            }
        }
    }

    // ── legacy 会话 ──

    fn session_create_params(
        &self,
        target: &WorkspaceTarget,
        params: &Value,
        gates: (bool, bool),
    ) -> Map<String, Value> {
        let mut out = Map::new();
        copy_defined(&mut out, params, &["sessionId"]);
        out.insert("workspace".into(), target.workspace_ref());
        copy_defined(
            &mut out,
            params,
            &[
                "parentSessionId",
                "mode",
                "model",
                "persistence",
                "thoughtLevel",
                "titleGenerationEnabled",
                "mcpServers",
                "toolAllowlist",
                "toolDenylist",
                "importedHistory",
            ],
        );
        if gates.0 {
            out.insert("offPeakToolEnabled".into(), json!(true));
        }
        if gates.1 {
            out.insert("dynamicWorkflowEnabled".into(), json!(true));
        }
        out
    }

    async fn gates(&self, target: &WorkspaceTarget) -> (bool, bool) {
        let extensions = self.chat.extensions();
        (
            extensions.off_peak_tools_for(target),
            extensions.dynamic_workflow_enabled().await,
        )
    }

    pub async fn create_session(&self, params: &Value) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        let process = self.write_client(&target).await?;
        self.chat
            .before_use(&process, UseReason::CreateSession)
            .await?;
        let gates = self.gates(&target).await;
        let full = self.session_create_params(&target, params, gates);
        let error = match process
            .request_value("session/create", &Value::Object(full.clone()))
            .await
        {
            Ok(snapshot) => return Ok(snapshot),
            Err(error) => error,
        };
        let omitted = compat_fields(&error, SESSION_CREATE_COMPAT);
        if omitted.is_empty() {
            log::warn(format!(
                "session/create failed for {}: {}",
                target.key(),
                error.message
            ));
            return Err(error);
        }
        // 旧 app-server 的 strict schema 不认新增可选字段：只省略这些键重试一次。
        log::warn(format!("session/create compat retry without {omitted:?}"));
        let snapshot = process
            .request_value("session/create", &without(full, &omitted))
            .await?;
        let thought_level = params["thoughtLevel"]
            .as_str()
            .filter(|level| !level.is_empty());
        match thought_level {
            // 旧 create 不认 thoughtLevel 时补发 setThoughtLevel，首轮仍用用户选择的推理强度。
            Some(level) if omitted.iter().any(|key| key == "thoughtLevel") => {
                process
                    .request_value(
                        "session/setThoughtLevel",
                        &json!({
                            "sessionId": snapshot["session"]["sessionId"],
                            "thoughtLevel": level,
                            "persistAsWorkspaceLastUsed": true,
                        }),
                    )
                    .await
            }
            _ => Ok(snapshot),
        }
    }

    pub async fn resume_session(&self, params: &Value) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        required_str(params, "sessionId")?;
        let process = self.write_client(&target).await?;
        self.chat
            .before_use(&process, UseReason::SessionResume)
            .await?;
        let (off_peak, dynamic_workflow) = self.gates(&target).await;
        let mut full = Map::new();
        copy_defined(&mut full, params, &["sessionId"]);
        full.insert("workspace".into(), target.workspace_ref());
        copy_defined(
            &mut full,
            params,
            &[
                "thoughtLevel",
                "mcpServers",
                "toolAllowlist",
                "toolDenylist",
            ],
        );
        if off_peak {
            full.insert("offPeakToolEnabled".into(), json!(true));
        }
        if dynamic_workflow {
            full.insert("dynamicWorkflowEnabled".into(), json!(true));
        }
        let error = match process
            .request_value("session/resume", &Value::Object(full.clone()))
            .await
        {
            Ok(snapshot) => return Ok(snapshot),
            Err(error) => error,
        };
        let omitted = compat_fields(&error, SESSION_RESUME_COMPAT);
        if omitted.is_empty() {
            return Err(error);
        }
        log::warn(format!("session/resume compat retry without {omitted:?}"));
        process
            .request_value("session/resume", &without(full, &omitted))
            .await
    }

    pub async fn list_sessions(&self, params: &Value) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        let process = self.policy_client(&target, params).await?;
        let mut request = Map::new();
        request.insert("workspace".into(), target.workspace_ref());
        request.insert(
            "includeArchived".into(),
            json!(params["includeArchived"].as_bool().unwrap_or(false)),
        );
        copy_defined(&mut request, params, &["limit"]);
        copy_truthy(&mut request, params, &["sessionIds"]);
        let result = process
            .request_value("session/list", &Value::Object(request))
            .await?;
        Ok(result["sessions"].clone())
    }

    pub async fn list_session_subagents(&self, params: &Value) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        let session_id = required_str(params, "sessionId")?;
        let process = self.read_client(&target).await?;
        let mut request = Map::new();
        request.insert("sessionId".into(), json!(session_id));
        copy_defined(&mut request, params, &["endedCursor"]);
        request.insert(
            "endedLimit".into(),
            params
                .get("endedLimit")
                .filter(|limit| !limit.is_null())
                .cloned()
                .unwrap_or(json!(20)),
        );
        process
            .request_value("session/subagents", &Value::Object(request))
            .await
    }

    pub async fn read_session(&self, params: &Value) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        let session_id = required_str(params, "sessionId")?;
        let process = self.policy_client(&target, params).await?;
        // existing-only 是被动观察，不同步 Account Config（不改变 session 生命周期）。
        if RuntimePolicy::from_value(params.get("runtimePolicy")) != RuntimePolicy::ExistingOnly {
            self.chat
                .before_use(&process, UseReason::SessionRead)
                .await?;
        }
        let mut request = Map::new();
        request.insert("sessionId".into(), json!(session_id));
        copy_defined(
            &mut request,
            params,
            &["deliveryKind", "messageLimit", "afterSeq"],
        );
        process
            .request_value("session/read", &Value::Object(request))
            .await
    }

    async fn session_query(
        &self,
        params: &Value,
        method: &str,
        fields: &[&str],
        policy: RuntimePolicy,
    ) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        let session_id = required_str(params, "sessionId")?;
        let process = self.chat.get_client(&target, policy).await?;
        let mut request = Map::new();
        request.insert("sessionId".into(), json!(session_id));
        copy_defined(&mut request, params, fields);
        process.request_value(method, &Value::Object(request)).await
    }

    pub async fn read_session_messages(&self, params: &Value) -> Result<Value, RpcError> {
        let result = self
            .session_query(
                params,
                "session/messages",
                &["afterMessageId", "limit"],
                RuntimePolicy::StartIfNeeded,
            )
            .await?;
        Ok(result["messages"].clone())
    }

    pub async fn read_session_events(&self, params: &Value) -> Result<Value, RpcError> {
        let result = self
            .session_query(
                params,
                "session/events",
                &["afterSeq", "limit"],
                RuntimePolicy::StartIfNeeded,
            )
            .await?;
        Ok(result["events"].clone())
    }

    pub async fn read_session_debug(&self, params: &Value) -> Result<Value, RpcError> {
        self.session_query(params, "session/debug", &[], RuntimePolicy::ExistingOnly)
            .await
    }

    pub async fn task_token_usage(&self, params: &Value) -> Result<Value, RpcError> {
        self.session_query(
            params,
            "v4/conversation/usage",
            &[],
            RuntimePolicy::StartIfNeeded,
        )
        .await
    }

    /// 写 client 上的会话操作（goal、close、setModel 等）。
    async fn session_write(
        &self,
        params: &Value,
        method: &str,
        fields: &[&str],
        timeout: Option<Duration>,
    ) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        let session_id = required_str(params, "sessionId")?;
        let process = self.write_client(&target).await?;
        let mut request = Map::new();
        request.insert("sessionId".into(), json!(session_id));
        copy_defined(&mut request, params, fields);
        let request = Value::Object(request);
        match timeout {
            Some(timeout) => {
                process
                    .request_with_timeout(method, &request, timeout)
                    .await
            }
            None => process.request_value(method, &request).await,
        }
    }

    pub async fn compact_session(&self, params: &Value) -> Result<Value, RpcError> {
        // compact 的维护窗口可能是分钟级；放宽的是 ACK 边界，终态仍由会话推送。
        self.session_write(
            params,
            "session/compact",
            &["inputId", "instructions", "expectedRevision"],
            Some(SESSION_COMPACT_TIMEOUT),
        )
        .await
    }

    pub async fn goal_session(&self, params: &Value) -> Result<Value, RpcError> {
        self.session_write(
            params,
            "session/goal",
            &["inputId", "action", "objective", "expectedRevision"],
            None,
        )
        .await
    }

    /// 返回 `closed ?? true`（兼容不返回 closed 字段的 Agent）。
    pub async fn close_session(&self, params: &Value) -> Result<bool, RpcError> {
        let mut request = params.clone();
        if !truthy(&params["expectedPersistence"])
            && let Some(object) = request.as_object_mut()
        {
            object.remove("expectedPersistence");
        }
        let result = self
            .session_write(&request, "session/close", &["expectedPersistence"], None)
            .await?;
        Ok(result["closed"].as_bool().unwrap_or(true))
    }

    pub async fn set_model(&self, params: &Value) -> Result<Value, RpcError> {
        self.session_write(
            params,
            "session/setModel",
            &["model", "expectedRevision", "persistAsWorkspaceLastUsed"],
            None,
        )
        .await
    }

    pub async fn set_thought_level(&self, params: &Value) -> Result<Value, RpcError> {
        self.session_write(
            params,
            "session/setThoughtLevel",
            &[
                "thoughtLevel",
                "expectedRevision",
                "persistAsWorkspaceLastUsed",
            ],
            None,
        )
        .await
    }

    pub async fn set_mode(&self, params: &Value) -> Result<Value, RpcError> {
        self.session_write(
            params,
            "session/setMode",
            &["mode", "expectedRevision"],
            None,
        )
        .await
    }

    /// legacy `session/send`（V4 界面不用）：未带浏览器环境时收集一次。
    pub async fn send_prompt(&self, params: &Value) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        let session_id = required_str(params, "sessionId")?;
        let process = self.write_client(&target).await?;
        let mut ambient = params
            .get("browserAmbientContext")
            .filter(|value| !value.is_null())
            .cloned();
        if ambient.is_none()
            && let Some(browser) = &self.browser
        {
            let client_mode = params["clientMode"]
                .as_str()
                .unwrap_or("desktop-continuous");
            ambient = browser
                .ambient_context(session_id, &target, client_mode)
                .await;
        }
        let mut full = Map::new();
        full.insert("sessionId".into(), json!(session_id));
        copy_truthy(&mut full, params, &["modelSelection", "modelExecution"]);
        copy_defined(
            &mut full,
            params,
            &["inputId", "queryId", "content", "attachments"],
        );
        if let Some(ambient) = ambient {
            full.insert("browserAmbientContext".into(), ambient);
        }
        copy_defined(
            &mut full,
            params,
            &[
                "expectedRevision",
                "expectedProviderRevision",
                "automationId",
                "offPeakTaskId",
                "offPeakRunType",
                "botDeliveryTarget",
                "toolDenylist",
            ],
        );
        let error = match process
            .request_value("session/send", &Value::Object(full.clone()))
            .await
        {
            Ok(result) => return Ok(result),
            Err(error) => error,
        };
        let omitted = compat_fields(&error, SESSION_SEND_COMPAT);
        if omitted.is_empty() {
            return Err(error);
        }
        process
            .request_value("session/send", &without(full, &omitted))
            .await
    }

    pub async fn grant_workspace_hook_trust(&self, params: &Value) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        let process = self.read_client(&target).await?;
        let mut request = Map::new();
        request.insert("workspace".into(), target.workspace_ref());
        copy_defined(
            &mut request,
            params,
            &["bundleDigest", "hookDeclarationDigest"],
        );
        process
            .request_value("workspace/hooks/trustGrant", &Value::Object(request))
            .await
    }

    // ── 生成文本、连通性、用量 ──

    pub async fn generate_workspace_text(&self, params: &Value) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        let process = self.write_client(&target).await?;
        self.chat
            .before_use(&process, UseReason::GenerateText)
            .await?;
        let operation_id = uuid_v4();
        let mut request = Map::new();
        request.insert("workspace".into(), target.workspace_ref());
        copy_defined(&mut request, params, &["selection"]);
        copy_truthy(&mut request, params, &["prompt", "messages", "tools"]);
        copy_defined(&mut request, params, &["querySource"]);
        copy_truthy(&mut request, params, &["maxOutputTokens"]);
        request.insert("operationId".into(), json!(operation_id));
        let request = Value::Object(request);
        let mut guard = CancelOnDrop {
            process: Some(process.clone()),
            operation_id,
        };
        // thinking 模型的长请求可能超过默认 3 分钟；调用方显式给出 deadline 时以它为准。
        let timeout = params["requestTimeoutMs"]
            .as_f64()
            .filter(|ms| ms.is_finite() && *ms > 0.0)
            .map(|ms| Duration::from_millis(ms as u64));
        let result = match timeout {
            Some(timeout) => {
                process
                    .request_with_timeout("workspace/generateText", &request, timeout)
                    .await
            }
            None => {
                process
                    .request_value("workspace/generateText", &request)
                    .await
            }
        };
        guard.process = None;
        result
    }

    pub async fn test_model_connectivity(&self, params: &Value) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        let process = self.write_client(&target).await?;
        self.chat
            .before_use(&process, UseReason::TestModelConnectivity)
            .await?;
        let mut request = Map::new();
        request.insert("workspace".into(), target.workspace_ref());
        copy_defined(&mut request, params, &["selection"]);
        process
            .request_value("provider/testModelConnectivity", &Value::Object(request))
            .await
    }

    /// usage 表在全局会话库：任一已运行的 chat 进程即可回答全应用范围。
    pub async fn app_usage_stats(&self, params: &Value) -> Result<Value, RpcError> {
        let process = self
            .chat
            .live_processes()
            .await
            .into_iter()
            .next()
            .ok_or_else(|| fault("no_active_workspace"))?;
        let mut request = Map::new();
        copy_defined(&mut request, params, &["range", "timeZone"]);
        process
            .request_value("v4/usage/stats", &Value::Object(request))
            .await
    }

    /// 资源管理器：三个 lane 的存活、存储已就绪进程各自的子进程映射（失败时为空）。
    pub async fn collect_child_processes(&self) -> Value {
        let processes: Vec<Arc<AgentProcess>> = self
            .chat
            .all_live_processes()
            .await
            .into_iter()
            .filter(|process| !process.client.storage_waiting() && process.pid.is_some())
            .collect();
        let entries = futures_util::future::join_all(processes.iter().map(|process| async move {
            let children = process
                .observe_with_timeout(
                    "process/childProcesses",
                    &json!({}),
                    CHILD_PROCESSES_TIMEOUT,
                )
                .await
                .ok()
                .and_then(|result| result["processes"].as_array().cloned())
                .unwrap_or_default();
            let mut entry = Map::new();
            entry.insert("pid".into(), json!(process.pid));
            entry.insert("provider".into(), json!(AGENT_PROVIDER));
            entry.insert("workspacePath".into(), json!(process.target.workspace_path));
            if let Some(lane) = process.lane().identity_label() {
                entry.insert("lane".into(), json!(lane));
            }
            entry.insert("children".into(), Value::Array(children));
            Value::Object(entry)
        }))
        .await;
        Value::Array(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invalid_params(message: &str, issues: Value) -> RpcError {
        RpcError::new("ZCodeProtocolClientError", message)
            .with_code(-32602)
            .with_data(json!({"message": issues.to_string()}))
    }

    #[test]
    fn compat_retry_only_trusts_structured_issues() {
        let error = invalid_params(
            "Invalid params — unrecognized keys",
            json!([
                {"code": "unrecognized_keys", "path": [], "keys": ["thoughtLevel", "mcpServers"]},
                {"code": "unrecognized_keys", "path": ["model"], "keys": ["x"]},
            ]),
        );
        assert_eq!(
            unrecognized_top_level_keys(&error),
            ["thoughtLevel", "mcpServers"]
        );
        assert_eq!(
            compat_fields(&error, SESSION_CREATE_COMPAT),
            ["thoughtLevel", "mcpServers"]
        );
        // 含不可省略的键（importedHistory）时不重试。
        let strict = invalid_params(
            "Invalid params",
            json!([{"code": "unrecognized_keys", "path": [], "keys": ["importedHistory"]}]),
        );
        assert!(compat_fields(&strict, SESSION_CREATE_COMPAT).is_empty());
        let other = RpcError::new("Error", "Invalid params").with_code(-32600);
        assert!(unrecognized_top_level_keys(&other).is_empty());
    }

    #[test]
    fn truthiness_and_model_format_follow_javascript() {
        assert!(!truthy(&json!("")) && !truthy(&json!(0)) && !truthy(&json!(false)));
        assert!(truthy(&json!("x")) && truthy(&json!([])) && truthy(&json!(1)));
        assert_eq!(
            format_model(
                &json!({"providerId": "p", "modelId": "m", "options": {"reasoningLevel": "high"}})
            ),
            "p/m$high"
        );
        assert_eq!(format_model(&json!({"providerId": "p"})), "");
    }
}
