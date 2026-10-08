//! 每个 workspace 一个 CLI 进程（zcodeAgentProcessManager.ts 的 chat lane 与 zcodeAgentService.ts 的
//! wireClient / getClient / getReadOnlyClient）。
//!
//! 所有者与事件顺序：
//!
//! ```text
//! get_client(target) ──(per-key 锁，single-flight)──► spawn ──► generation += 1
//!     ├─ generation > 1: onAgentRuntimeRestarted {workspaceKey, runtimeIdentity}
//!     ├─ onAgentRuntimeLifecycle available
//!     └─ 回放 app 运行时偏好（若曾同步过）后才把 client 交给调用方
//! monitor(进程)：stdout 关闭 / 协议错误 / 请求超时 / 进程退出 / dispose
//!     ├─ 先从槽位摘除（之后的 get_client 必然启动下一代）
//!     ├─ onAgentRuntimeLifecycle unavailable（每代只发一次）
//!     └─ 关闭 stdin → 回收进程树
//! ```
//!
//! 只做懒重启：进程没了不自动拉起，下一次 `get_client` 才启动新一代。
//!
//! 同一套实现服务三个 lane（docs/specs/rust-host-agent.md §2.1）：chat 按 workspace 分进程，
//! plugin-management 与 mcp-status 是控制面 lane，各自一张进程表，共享扩展点与通知总线。
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Map, Value, json};
use tokio::process::Child;
use tokio::task::JoinHandle;
use zcode_host_kit::{EventHub, RpcError, log};

use super::client::{ClientError, ClientHandler, Notification, ProtocolClient, ReverseRequest};
use super::command::{self, AgentCommand};
use super::extensions::{
    AgentNotification, Extensions, ProcessEvent, ReverseContext, ReverseReply, UseReason,
};
use super::process::{self, StderrTail};
use crate::providers::ProviderRuntime;
use crate::settings::SettingStore;

pub const RUNTIME_UNAVAILABLE_CODE: &str = "ZCODE_AGENT_RUNTIME_UNAVAILABLE";
pub const PROVIDER_NOT_READY_CODE: &str = "ZCODE_AGENT_PROVIDER_NOT_READY";
const EXIT_FLUSH_WAIT: Duration = Duration::from_secs(1);
const DISPOSE_WAIT: Duration = Duration::from_secs(3);
/// mcp-status lane 的空闲回收阈值（MCP_STATUS_LANE_IDLE_TIMEOUT_MS）。
pub const MCP_STATUS_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Node `ZCODE_AGENT_PROVIDER`。
pub const AGENT_PROVIDER: &str = "glm";

/// CLI 进程泳道（zcodeAgentService.ts 的三个 process manager）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    Chat,
    PluginManagement,
    McpStatus,
}

impl Lane {
    /// runtime identity 与 `agent-process-*` 消息里的 lane；Node 只给 mcp-status 设了 lane。
    pub fn identity_label(self) -> Option<&'static str> {
        match self {
            Lane::McpStatus => Some("mcp-status"),
            Lane::Chat | Lane::PluginManagement => None,
        }
    }

    /// 资源样本的 lane 标签（wireClient 的 `lane` 参数）。
    pub fn sample_label(self) -> &'static str {
        match self {
            Lane::Chat => "chat",
            Lane::PluginManagement => "plugin",
            Lane::McpStatus => "mcp-status",
        }
    }

    /// Node 只给 chat 与 mcp-status 的 process manager 注入了 lifecycle reporter。
    fn reports_lifecycle(self) -> bool {
        !matches!(self, Lane::PluginManagement)
    }

    /// 只有 chat lane 登记为 active client：回放偏好、推送功能门禁策略。
    fn serves_workspaces(self) -> bool {
        matches!(self, Lane::Chat)
    }
}

/// parseRemoteWorkspaceIdentity 的判定部分：`remote:<ssh|wsl|docker>:<authority…>:/path`。
pub fn is_remote_identity(identity: &str) -> bool {
    let Some(rest) = identity.strip_prefix("remote:") else {
        return false;
    };
    let Some((kind, mut rest)) = rest.split_once(':') else {
        return false;
    };
    let segments = match kind {
        "ssh" => 3,
        "wsl" | "docker" => 1,
        _ => return false,
    };
    for _ in 0..segments {
        match rest.split_once(':') {
            Some((segment, next)) if !segment.is_empty() => rest = next,
            _ => return false,
        }
    }
    if kind == "wsl" && !rest.starts_with('/') {
        match rest.split_once(':') {
            Some((user, next)) if !user.is_empty() => rest = next,
            _ => return false,
        }
    }
    rest.starts_with('/')
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

/// `ZCodeAgentWorkspaceTarget`：identity 只用于隔离与路由，文件操作永远用 path。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceTarget {
    pub workspace_path: String,
    #[serde(default)]
    pub workspace_identity: Option<String>,
    #[serde(default)]
    pub remote_session_id: Option<String>,
}

fn trimmed(value: &Option<String>) -> Option<&str> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

impl WorkspaceTarget {
    pub fn local(path: impl Into<String>) -> Self {
        Self {
            workspace_path: path.into(),
            workspace_identity: None,
            remote_session_id: None,
        }
    }

    /// 从调用参数里读取；其余字段忽略。
    pub fn from_value(value: &Value) -> Result<Self, RpcError> {
        let target: Self = serde_json::from_value(value.clone())
            .map_err(|error| invalid_target(&error.to_string()))?;
        if target.workspace_path.trim().is_empty() {
            return Err(invalid_target("workspacePath is required"));
        }
        Ok(target)
    }

    /// 身份 key：`workspaceIdentity?.trim() || workspacePath`。
    pub fn key(&self) -> String {
        trimmed(&self.workspace_identity)
            .unwrap_or(&self.workspace_path)
            .to_owned()
    }

    /// `ZCodeWorkspaceRef`（发给 CLI 的 workspace 字段）。
    /// 本地 workspace：没有 remoteSessionId，identity 也不是远端 identity。
    pub fn is_local(&self) -> bool {
        trimmed(&self.remote_session_id).is_none()
            && self
                .workspace_identity
                .as_deref()
                .is_none_or(|identity| !is_remote_identity(identity))
    }

    pub fn workspace_ref(&self) -> Value {
        let mut map = Map::new();
        map.insert("workspacePath".into(), json!(self.workspace_path));
        if let Some(identity) = trimmed(&self.workspace_identity) {
            map.insert("workspaceIdentity".into(), json!(identity));
        }
        if let Some(remote) = trimmed(&self.remote_session_id) {
            map.insert("remoteSessionId".into(), json!(remote));
        }
        map.insert("workspaceKey".into(), json!(self.key()));
        Value::Object(map)
    }

    pub(crate) fn event_fields(&self) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert("workspacePath".into(), json!(self.workspace_path));
        if let Some(identity) = trimmed(&self.workspace_identity) {
            map.insert("workspaceIdentity".into(), json!(identity));
        }
        map
    }
}

fn invalid_target(detail: &str) -> RpcError {
    RpcError::new("Error", format!("invalid workspace target: {detail}"))
        .with_code("fault.invalidArgument")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimePolicy {
    StartIfNeeded,
    ExistingOnly,
}

impl RuntimePolicy {
    pub fn from_value(value: Option<&Value>) -> Self {
        match value.and_then(Value::as_str) {
            Some("existing-only") => Self::ExistingOnly,
            _ => Self::StartIfNeeded,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameKind {
    Conversation,
    SessionsIndex,
    WorkspaceConfig,
}

impl FrameKind {
    fn of_topic(topic: &str) -> Option<Self> {
        if topic.starts_with("conversation/") {
            Some(Self::Conversation)
        } else if topic.starts_with("sessions-index/") {
            Some(Self::SessionsIndex)
        } else if topic.starts_with("workspace-config/") {
            Some(Self::WorkspaceConfig)
        } else {
            None
        }
    }
}

/// 一条 V4 物理帧：只解析路由字段，`raw` 原样转发（分片由 Renderer 自己拼）。
#[derive(Debug)]
pub struct Frame {
    pub workspace_key: String,
    pub generation: u64,
    pub kind: FrameKind,
    pub topic: String,
    pub subscription_id: String,
    pub raw: Box<RawValue>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireRouting {
    wire_version: u64,
    kind: String,
    topic: String,
    subscription_id: String,
    logical_frame_id: String,
    #[allow(dead_code)]
    logical_frame_ordinal: f64,
}

fn parse_frame(raw: Box<RawValue>, key: &str, generation: u64) -> Result<Frame, String> {
    let routing: WireRouting =
        serde_json::from_str(raw.get()).map_err(|error| error.to_string())?;
    if routing.wire_version != 3 {
        return Err(format!("unsupported wireVersion {}", routing.wire_version));
    }
    if routing.kind != "complete" && routing.kind != "fragment" {
        return Err(format!("unknown frame kind {}", routing.kind));
    }
    if routing.topic.is_empty()
        || routing.subscription_id.is_empty()
        || routing.logical_frame_id.is_empty()
    {
        return Err("empty routing field".into());
    }
    let kind = FrameKind::of_topic(&routing.topic)
        .ok_or_else(|| format!("unknown topic {}", routing.topic))?;
    Ok(Frame {
        workspace_key: key.to_owned(),
        generation,
        kind,
        topic: routing.topic,
        subscription_id: routing.subscription_id,
        raw,
    })
}

pub struct AgentProcess {
    pub target: WorkspaceTarget,
    pub key: String,
    pub generation: u64,
    pub pid: Option<u32>,
    pub client: Arc<ProtocolClient>,
    capabilities: tokio::sync::OnceCell<Option<Value>>,
    lane: Lane,
    /// `agent-<uuid>`：生命周期事件的关联 id（不含 workspaceKey）。
    instance_id: String,
    started_at_ms: u64,
    /// 首次通过 provider 门禁的时间（markReady）。
    ready_at_ms: std::sync::OnceLock<u64>,
    /// 已推送到本进程的 app 偏好版本；锁同时串行化同一 workspace 的偏好推送。
    preferences_applied: tokio::sync::Mutex<u64>,
    /// 功能门禁策略每个进程只推一次。
    policies: tokio::sync::OnceCell<()>,
}

/// CLI 错误转成 RPC 错误：业务错误保留数字 code 与 data，UI 按它们分支。
pub fn client_error(error: ClientError) -> RpcError {
    match error {
        ClientError::Remote {
            code,
            message,
            data,
        } => {
            let error = RpcError::new("ZCodeProtocolClientError", message).with_code(code);
            match data {
                Some(data) => error.with_data(data),
                None => error,
            }
        }
        other => RpcError::new("Error", other.to_string()),
    }
}

impl AgentProcess {
    pub fn runtime_identity(&self) -> Value {
        let pid = self
            .pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        let mut identity = Map::new();
        identity.insert("generation".into(), json!(self.generation));
        let text = match self.lane.identity_label() {
            Some(lane) => format!("{}:{}:{pid}:{lane}", self.key, self.generation),
            None => format!("{}:{}:{pid}", self.key, self.generation),
        };
        identity.insert("identity".into(), json!(text));
        if let Some(pid) = self.pid {
            identity.insert("processId".into(), json!(pid));
        }
        if let Some(lane) = self.lane.identity_label() {
            identity.insert("lane".into(), json!(lane));
        }
        identity.insert("workspaceKey".into(), json!(self.key));
        Value::Object(identity)
    }

    pub fn lane(&self) -> Lane {
        self.lane
    }

    /// 发请求并指定超时；超时同样回收进程（插件市场 I/O、compact ACK 等放宽超时的请求）。
    pub async fn request_with_timeout(
        &self,
        method: &str,
        params: &Value,
        timeout: Duration,
    ) -> Result<Value, RpcError> {
        let raw = match self
            .client
            .request_raw(method, &params.to_string(), timeout)
            .await
        {
            Err(ClientError::Timeout(method)) => {
                self.client.close("watchdog_recycle");
                return Err(client_error(ClientError::Timeout(method)));
            }
            other => other.map_err(client_error)?,
        };
        serde_json::from_str(raw.get())
            .map_err(|error| RpcError::new("Error", format!("invalid {method} result: {error}")))
    }

    /// 观察型请求（`lifecycle: "observation"`）并指定超时：超时只失败本次，不回收进程。
    pub async fn observe_with_timeout(
        &self,
        method: &str,
        params: &Value,
        timeout: Duration,
    ) -> Result<Value, RpcError> {
        let raw = self
            .client
            .request_raw(method, &params.to_string(), timeout)
            .await
            .map_err(client_error)?;
        serde_json::from_str(raw.get())
            .map_err(|error| RpcError::new("Error", format!("invalid {method} result: {error}")))
    }

    /// 首次通过 provider 门禁（Node markReady）；返回是否是第一次。
    fn mark_ready(&self) -> bool {
        self.ready_at_ms.set(now_ms()).is_ok()
    }

    fn lifecycle_base(&self) -> Option<Map<String, Value>> {
        let pid = self.pid?;
        let mut base = Map::new();
        if let Some(lane) = self.lane.identity_label() {
            base.insert("lane".into(), json!(lane));
        }
        base.insert("pid".into(), json!(pid));
        base.insert("provider".into(), json!(AGENT_PROVIDER));
        base.insert("workspacePath".into(), json!(self.target.workspace_path));
        Some(base)
    }

    /// 发请求；超时说明 CLI 卡死，关闭连接让 monitor 回收进程（下次请求懒重启）。
    pub async fn request(&self, method: &str, params: &Value) -> Result<Box<RawValue>, RpcError> {
        match self.client.request(method, params).await {
            Err(ClientError::Timeout(method)) => {
                self.client.close("watchdog_recycle");
                Err(client_error(ClientError::Timeout(method)))
            }
            other => other.map_err(client_error),
        }
    }

    /// 被动观察类请求（syncer 回源等）：超时只失败本次请求，不回收进程
    /// （对应 Node 的 `lifecycle: "observation"`），避免观察者拖垮正在运行的会话。
    pub async fn observe(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        let raw = self
            .client
            .request(method, params)
            .await
            .map_err(client_error)?;
        serde_json::from_str(raw.get())
            .map_err(|error| RpcError::new("Error", format!("invalid {method} result: {error}")))
    }

    pub async fn request_value(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        let raw = self.request(method, params).await?;
        serde_json::from_str(raw.get())
            .map_err(|error| RpcError::new("Error", format!("invalid {method} result: {error}")))
    }

    /// `runtime/capabilities`（按进程缓存）；老 CLI 不支持时为 None。
    pub async fn capabilities(&self) -> Option<Value> {
        self.capabilities
            .get_or_init(|| async {
                match self.request_value("runtime/capabilities", &json!({})).await {
                    Ok(value) => Some(value),
                    Err(error) => {
                        log::debug(format!(
                            "runtime/capabilities unavailable: {}",
                            error.message
                        ));
                        None
                    }
                }
            })
            .await
            .clone()
    }
}

#[derive(Clone)]
pub struct AgentRuntimeConfig {
    /// 清洗后的 Host 环境 + Main 的登录 shell 补丁（support::env::ProcessEnv::for_children）。
    pub base_env: BTreeMap<String, String>,
    /// 命令解析读取的变量（`ZCODE_AGENT_SERVER_*` 等），即 Host 自身环境。
    pub command_vars: BTreeMap<String, String>,
    /// 向上查找开发态 `zcode.cjs` 的起点。
    pub search_roots: Vec<PathBuf>,
    pub fallback_cwd: Option<PathBuf>,
}

type Slot = Arc<tokio::sync::Mutex<Option<Arc<AgentProcess>>>>;

struct RuntimeInner {
    config: AgentRuntimeConfig,
    settings: SettingStore,
    providers: ProviderRuntime,
    slots: Mutex<HashMap<String, Slot>>,
    generations: Mutex<HashMap<String, u64>>,
    lifecycle: EventHub<Value>,
    restarted: EventHub<Value>,
    frames: EventHub<Arc<Frame>>,
    /// 最新一次 `syncAppRuntimePreferences`（版本号，归一化后的值）。
    app_preferences: Mutex<Option<(u64, Value)>>,
    monitors: Mutex<Vec<JoinHandle<()>>>,
    disposed: AtomicBool,
    extensions: Arc<Extensions>,
    lane: Lane,
    idle_timeout: Option<Duration>,
    /// `{workspaceKey, snapshot:{generation, state}}`（onDynamicStorageStartupState）。
    storage: EventHub<Value>,
    /// 由本 runtime 派生的控制面 lane；dispose_all 一并回收。
    lanes: Mutex<Vec<AgentRuntime>>,
}

#[derive(Clone)]
pub struct AgentRuntime {
    inner: Arc<RuntimeInner>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl AgentRuntime {
    pub fn new(
        config: AgentRuntimeConfig,
        settings: SettingStore,
        providers: ProviderRuntime,
    ) -> Self {
        Self::build(
            config,
            settings,
            providers,
            Arc::new(Extensions::default()),
            Lane::Chat,
            None,
        )
    }

    fn build(
        config: AgentRuntimeConfig,
        settings: SettingStore,
        providers: ProviderRuntime,
        extensions: Arc<Extensions>,
        lane: Lane,
        idle_timeout: Option<Duration>,
    ) -> Self {
        Self {
            inner: Arc::new(RuntimeInner {
                config,
                settings,
                providers,
                slots: Mutex::new(HashMap::new()),
                generations: Mutex::new(HashMap::new()),
                lifecycle: EventHub::new(),
                restarted: EventHub::new(),
                frames: EventHub::new(),
                app_preferences: Mutex::new(None),
                monitors: Mutex::new(Vec::new()),
                disposed: AtomicBool::new(false),
                extensions,
                lane,
                idle_timeout,
                storage: EventHub::new(),
                lanes: Mutex::new(Vec::new()),
            }),
        }
    }

    /// 派生一个控制面 lane：共享命令解析、环境、扩展点与通知总线，进程表独立。
    /// `idle_timeout` 只给 mcp-status 用（无在途请求且存储就绪后空闲这么久就回收）。
    pub fn lane(&self, lane: Lane, idle_timeout: Option<Duration>) -> AgentRuntime {
        let runtime = Self::build(
            self.inner.config.clone(),
            self.inner.settings.clone(),
            self.inner.providers.clone(),
            self.inner.extensions.clone(),
            lane,
            idle_timeout,
        );
        lock(&self.inner.lanes).push(runtime.clone());
        runtime
    }

    pub fn lane_kind(&self) -> Lane {
        self.inner.lane
    }

    /// 本 lane 的存储启动帧事件（onDynamicStorageStartupState）。
    pub fn storage_events(&self) -> EventHub<Value> {
        self.inner.storage.clone()
    }

    /// getStorageStartupState：没有进程时 None。
    pub async fn storage_state(&self, target: &WorkspaceTarget) -> Option<Value> {
        let process = self.existing(target).await?;
        Some(json!({
            "generation": process.generation,
            "state": process.client.storage_state().unwrap_or(Value::Null),
        }))
    }

    /// 其他服务登记反向请求处理器、spawn 环境与使用前钩子的入口。
    pub fn extensions(&self) -> &Arc<Extensions> {
        &self.inner.extensions
    }

    /// 依次执行使用前钩子（例如同步 Account Config）；任一失败则本次使用失败。
    pub async fn before_use(
        &self,
        process: &Arc<AgentProcess>,
        reason: UseReason,
    ) -> Result<(), RpcError> {
        for hook in self.inner.extensions.use_hooks() {
            hook(process.clone(), reason).await?;
        }
        Ok(())
    }

    pub fn lifecycle(&self) -> EventHub<Value> {
        self.inner.lifecycle.clone()
    }

    pub fn restarted(&self) -> EventHub<Value> {
        self.inner.restarted.clone()
    }

    pub fn frames(&self) -> EventHub<Arc<Frame>> {
        self.inner.frames.clone()
    }

    fn slot(&self, key: &str) -> Slot {
        lock(&self.inner.slots)
            .entry(key.to_owned())
            .or_default()
            .clone()
    }

    /// 只读路径（订阅、读行、文件变更）：没有 provider 门禁。
    /// start-if-needed 时，chat lane 在交出进程前补推 app 偏好与功能门禁策略。
    pub async fn get_client(
        &self,
        target: &WorkspaceTarget,
        policy: RuntimePolicy,
    ) -> Result<Arc<AgentProcess>, RpcError> {
        let process = self.acquire(target, policy).await?;
        if policy == RuntimePolicy::StartIfNeeded {
            self.prepare_for_use(&process).await?;
        }
        Ok(process)
    }

    /// 取进程但不补推偏好（prepareStorage：Node 直接用 process manager）。
    pub async fn acquire(
        &self,
        target: &WorkspaceTarget,
        policy: RuntimePolicy,
    ) -> Result<Arc<AgentProcess>, RpcError> {
        let slot = self.slot(&target.key());
        let mut current = slot.lock().await;
        if let Some(process) = current.as_ref()
            && !process.client.is_closed()
        {
            return Ok(process.clone());
        }
        if policy == RuntimePolicy::ExistingOnly || self.inner.disposed.load(Ordering::SeqCst) {
            return Err(
                RpcError::new("Error", "ZCode Agent runtime is not running.")
                    .with_code(RUNTIME_UNAVAILABLE_CODE),
            );
        }
        let process = self.start(target).await?;
        *current = Some(process.clone());
        Ok(process)
    }

    /// 写路径（命令、附件上传）：先过 provider/model 就绪门禁。
    pub async fn get_ready_client(
        &self,
        target: &WorkspaceTarget,
    ) -> Result<Arc<AgentProcess>, RpcError> {
        let (revision, resolved) = self.inner.providers.snapshot().await;
        if !resolved.has_selectable_model() {
            return Err(RpcError::new(
                "Error",
                "当前没有可用的模型供应商和模型，请先登录或配置 API Key。",
            )
            .with_code(PROVIDER_NOT_READY_CODE)
            .with_data(json!({
                "providerCount": resolved.registry_provider_count(),
                "reason": "provider_not_ready",
                "revision": revision.to_string(),
                "workspaceKey": target.key(),
                "workspacePath": target.workspace_path,
            })));
        }
        let process = self
            .get_client(target, RuntimePolicy::StartIfNeeded)
            .await?;
        if process.mark_ready() {
            self.report_ready(&process);
        }
        Ok(process)
    }

    /// `zcode-agent.initialize`：预热（走写门禁）；从不抛错，失败以 `available:false` 表达。
    pub async fn initialize(&self, target: &WorkspaceTarget) -> Value {
        match self.get_ready_client(target).await {
            Ok(_) => json!({
                "available": true,
                "workspaceKey": target.key(),
                "protocolName": "ZCode Protocol",
                "protocolVersion": 1,
                "transportKind": "stdio",
            }),
            Err(error) => {
                let mut result = Map::new();
                result.insert("available".into(), json!(false));
                result.insert("workspaceKey".into(), json!(target.key()));
                result.insert("protocolName".into(), json!("ZCode Protocol"));
                result.insert("protocolVersion".into(), json!(1));
                result.insert("reason".into(), json!(error.message));
                if error.code_str() == Some(PROVIDER_NOT_READY_CODE) {
                    result.insert("reasonCode".into(), json!("provider_not_ready"));
                }
                Value::Object(result)
            }
        }
    }

    pub async fn existing(&self, target: &WorkspaceTarget) -> Option<Arc<AgentProcess>> {
        let slot = self.slot(&target.key());
        let current = slot.lock().await;
        current
            .as_ref()
            .filter(|process| !process.client.is_closed())
            .cloned()
    }

    /// 正在运行的进程；正在启动（槽位被占用）的跳过，它们启动时会自行回放偏好。
    fn running(&self) -> Vec<Arc<AgentProcess>> {
        let slots: Vec<Slot> = lock(&self.inner.slots).values().cloned().collect();
        slots
            .iter()
            .filter_map(|slot| slot.try_lock().ok().and_then(|current| current.clone()))
            .filter(|process| !process.client.is_closed())
            .collect()
    }

    /// 本 lane 当前存活的全部进程（等正在启动的槽位就绪）。
    pub async fn live_processes(&self) -> Vec<Arc<AgentProcess>> {
        let slots: Vec<Slot> = lock(&self.inner.slots).values().cloned().collect();
        let mut processes = Vec::new();
        for slot in slots {
            if let Some(process) = slot.lock().await.clone()
                && !process.client.is_closed()
            {
                processes.push(process);
            }
        }
        processes
    }

    /// 本 lane 与派生 lane 的全部存活进程（collectLocalRuntimeChildProcesses）。
    pub async fn all_live_processes(&self) -> Vec<Arc<AgentProcess>> {
        let mut processes = self.live_processes().await;
        let lanes: Vec<AgentRuntime> = lock(&self.inner.lanes).clone();
        for lane in lanes {
            processes.extend(lane.live_processes().await);
        }
        processes
    }

    /// 新进程（或补推失败过的进程）交给调用方之前：追平最新 app 偏好，推一次功能门禁策略。
    async fn prepare_for_use(&self, process: &Arc<AgentProcess>) -> Result<(), RpcError> {
        if !self.inner.lane.serves_workspaces() {
            return Ok(());
        }
        process
            .policies
            .get_or_init(|| push_policies(&self.inner.extensions, process))
            .await;
        self.ensure_preferences(process).await
    }

    /// 在进程的偏好锁内推送当时最新的偏好，直到已推版本等于最新版本。
    async fn ensure_preferences(&self, process: &AgentProcess) -> Result<(), RpcError> {
        loop {
            let latest_version = match &*lock(&self.inner.app_preferences) {
                Some((version, _)) => *version,
                None => return Ok(()),
            };
            let mut applied = process.preferences_applied.lock().await;
            if *applied >= latest_version {
                return Ok(());
            }
            let Some((version, preferences)) = lock(&self.inner.app_preferences).clone() else {
                return Ok(());
            };
            push_preferences(process, &preferences).await?;
            *applied = version;
        }
    }

    async fn spawn_env(
        &self,
        target: &WorkspaceTarget,
        command: &AgentCommand,
    ) -> BTreeMap<String, String> {
        let config = &self.inner.config;
        let mut env = config.base_env.clone();
        let runtime_env = config
            .base_env
            .get("ZCODE_RUNTIME_ENV")
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| matches!(value.as_str(), "development" | "production" | "test"))
            .unwrap_or_else(|| "production".to_owned());
        env.insert("ZCODE_RUNTIME_ENV".into(), runtime_env);
        let settings = self.inner.settings.get().await;
        env.extend(network_env(&settings));
        for contributor in self.inner.extensions.env_contributors() {
            env.extend(contributor(target.clone()).await);
        }
        if let Some(builtin) = self.inner.providers.builtin_file() {
            env.insert(
                "ZCODE_BUILTIN_PROVIDER_CONFIG_FILE".into(),
                builtin.display().to_string(),
            );
        }
        env.insert(
            "ZCODE_PERSONAL_PROVIDER_CONFIG_FILE".into(),
            self.inner.providers.personal_file().display().to_string(),
        );
        env.extend(command.env.clone());
        if let Some(identity) = trimmed(&target.workspace_identity) {
            env.insert("ZCODE_WORKSPACE_IDENTITY".into(), identity.to_owned());
        }
        env
    }

    /// 解析 CLI 命令（会话库准备与 spawn 共用）。
    pub fn resolve_command(&self, workspace: &Path) -> Result<Option<AgentCommand>, RpcError> {
        let config = &self.inner.config;
        command::resolve(&config.command_vars, &config.search_roots, workspace)
            .map_err(|error| RpcError::new("Error", format!("{error:#}")))
    }

    fn next_generation(&self, key: &str) -> u64 {
        let mut generations = lock(&self.inner.generations);
        let next = generations.entry(key.to_owned()).or_default();
        *next += 1;
        *next
    }

    fn emit_process_event(&self, message: Value) {
        if self.inner.lane.reports_lifecycle() {
            self.inner
                .extensions
                .process_events()
                .emit(Arc::new(ProcessEvent {
                    lane: self.inner.lane,
                    message,
                }));
        }
    }

    fn report_ready(&self, process: &AgentProcess) {
        let (Some(mut message), Some(ready_at)) =
            (process.lifecycle_base(), process.ready_at_ms.get())
        else {
            return;
        };
        message.insert("type".into(), json!("agent-process-ready"));
        message.insert("readyAt".into(), json!(ready_at));
        message.insert(
            "startupDurationMs".into(),
            json!(ready_at.saturating_sub(process.started_at_ms)),
        );
        message.insert("runtimeGeneration".into(), json!(process.generation));
        message.insert("runtimeInstanceId".into(), json!(process.instance_id));
        self.emit_process_event(Value::Object(message));
    }

    async fn start(&self, target: &WorkspaceTarget) -> Result<Arc<AgentProcess>, RpcError> {
        let config = &self.inner.config;
        let workspace = Path::new(&target.workspace_path);
        let command = self.resolve_command(workspace)?.ok_or_else(|| {
            RpcError::new(
                "Error",
                "ZCode agent runtime command could not be resolved.",
            )
            .with_code(RUNTIME_UNAVAILABLE_CODE)
        })?;
        let env = self.spawn_env(target, &command).await;
        let cwd = spawn_cwd(
            command.cwd.as_deref(),
            workspace,
            config.fallback_cwd.as_deref(),
        )
        .await;
        let key = target.key();
        let instance_id = format!("agent-{}", super::service::uuid_v4());
        let program = command.program.display().to_string();
        let spawned = match process::spawn(&command, &cwd, &env) {
            Ok(spawned) => spawned,
            Err(error) => {
                let generation = self.next_generation(&key);
                let mut message = Map::new();
                message.insert("type".into(), json!("agent-process-error"));
                if let Some(lane) = self.inner.lane.identity_label() {
                    message.insert("lane".into(), json!(lane));
                }
                message.insert("pid".into(), Value::Null);
                message.insert("provider".into(), json!(AGENT_PROVIDER));
                message.insert("workspacePath".into(), json!(target.workspace_path));
                message.insert("command".into(), json!(program));
                message.insert("args".into(), json!(command.args));
                message.insert("errorName".into(), json!("Error"));
                if let Some(code) = process::errno_code(&error) {
                    message.insert("errorCode".into(), json!(code));
                }
                message.insert("errorMessage".into(), json!(error.to_string()));
                message.insert("runtimeGeneration".into(), json!(generation));
                message.insert("runtimeInstanceId".into(), json!(instance_id));
                message.insert("occurredAt".into(), json!(now_ms()));
                self.emit_process_event(Value::Object(message));
                return Err(RpcError::new(
                    "Error",
                    format!("failed to start ZCode agent {program}: {error}"),
                ));
            }
        };
        let generation = self.next_generation(&key);
        let started_at_ms = now_ms();
        let label = match self.inner.lane.identity_label() {
            Some(lane) => format!("agent[{key}#{generation}:{lane}]"),
            None => format!("agent[{key}#{generation}]"),
        };
        log::info(format!(
            "{label} started pid={} storageGate={}",
            spawned.pid.map(|pid| pid.to_string()).unwrap_or_default(),
            command.supports_storage_startup
        ));
        let diagnostic_sink = self.diagnostic_sink(target, generation, spawned.pid, &instance_id);
        let (tail, stderr_done) =
            process::drain_stderr(spawned.stderr, label.clone(), diagnostic_sink);
        let handler = Arc::new(Handler {
            key: key.clone(),
            target: target.clone(),
            extensions: self.inner.extensions.clone(),
            generation,
            lane: self.inner.lane,
            settings: self.inner.settings.clone(),
            frames: self.inner.frames.clone(),
        });
        let client = ProtocolClient::start(
            spawned.stdout,
            spawned.stdin,
            command.supports_storage_startup,
            handler,
        );
        let process = Arc::new(AgentProcess {
            target: target.clone(),
            key,
            generation,
            pid: spawned.pid,
            client,
            capabilities: tokio::sync::OnceCell::new(),
            lane: self.inner.lane,
            instance_id,
            started_at_ms,
            ready_at_ms: std::sync::OnceLock::new(),
            preferences_applied: tokio::sync::Mutex::new(0),
            policies: tokio::sync::OnceCell::new(),
        });
        if let Some(mut message) = process.lifecycle_base() {
            message.insert("type".into(), json!("agent-process-spawned"));
            message.insert("command".into(), json!(program));
            message.insert("args".into(), json!(command.args));
            message.insert("startedAt".into(), json!(started_at_ms));
            message.insert("runtimeGeneration".into(), json!(generation));
            message.insert("runtimeInstanceId".into(), json!(process.instance_id));
            self.emit_process_event(Value::Object(message));
        }
        if generation > 1 {
            self.inner.restarted.emit(json!({
                "workspaceKey": process.key,
                "runtimeIdentity": process.runtime_identity(),
            }));
        }
        self.inner
            .lifecycle
            .emit(lifecycle_event(&process, "available"));
        self.watch_storage(&process);
        if let Some(timeout) = self.inner.idle_timeout {
            tokio::spawn(idle_reclaim(process.clone(), timeout));
        }
        let monitor = tokio::spawn(monitor(
            self.clone(),
            spawned.child,
            process.clone(),
            tail,
            stderr_done,
            label,
        ));
        {
            let mut monitors = lock(&self.inner.monitors);
            monitors.retain(|handle| !handle.is_finished());
            monitors.push(monitor);
        }
        Ok(process)
    }

    /// stderr 进程诊断 → `agent-process-exception`；身份绑定这次 spawn，迟到的行不会串到新进程。
    fn diagnostic_sink(
        &self,
        target: &WorkspaceTarget,
        generation: u64,
        pid: Option<u32>,
        instance_id: &str,
    ) -> Option<process::DiagnosticSink> {
        let pid = pid?;
        if !self.inner.lane.reports_lifecycle() {
            return None;
        }
        let runtime = self.clone();
        let workspace_path = target.workspace_path.clone();
        let instance_id = instance_id.to_owned();
        Some(Box::new(move |diagnostic| {
            let mut message = Map::new();
            message.insert("type".into(), json!("agent-process-exception"));
            if let Some(lane) = runtime.inner.lane.identity_label() {
                message.insert("lane".into(), json!(lane));
            }
            message.insert("pid".into(), json!(pid));
            message.insert("provider".into(), json!(AGENT_PROVIDER));
            message.insert("workspacePath".into(), json!(workspace_path));
            message.insert("runtimeGeneration".into(), json!(generation));
            message.insert("runtimeInstanceId".into(), json!(instance_id));
            message.insert("diagnostic".into(), diagnostic);
            runtime.emit_process_event(Value::Object(message));
        }))
    }

    /// 每个新进程先发 `{generation, state:null}`，之后每个被接受的存储帧发一次。
    fn watch_storage(&self, process: &Arc<AgentProcess>) {
        let hub = self.inner.storage.clone();
        let key = process.key.clone();
        let generation = process.generation;
        let mut receiver = process.client.watch_storage();
        let client = process.client.clone();
        hub.emit(json!({
            "workspaceKey": key,
            "snapshot": {"generation": generation, "state": Value::Null},
        }));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = receiver.changed() => {
                        if changed.is_err() {
                            return;
                        }
                    }
                    _ = client.closed() => return,
                }
                let state = receiver.borrow_and_update().clone().unwrap_or(Value::Null);
                hub.emit(json!({
                    "workspaceKey": key,
                    "snapshot": {"generation": generation, "state": state},
                }));
            }
        });
    }

    /// `syncAppRuntimePreferences`：记住最新值，推给正在运行的 chat 进程（不拉起空闲 workspace）；
    /// 任一推送失败则调用失败。新进程启动时由 [`Self::prepare_for_use`] 补推。
    pub async fn sync_app_preferences(&self, preferences: Value) -> Result<(), RpcError> {
        let mut normalized = match preferences {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        let full_retention = normalized.get("modelIoFullRetentionEnabled") == Some(&json!(true));
        normalized.insert("modelIoFullRetentionEnabled".into(), json!(full_retention));
        {
            let mut latest = lock(&self.inner.app_preferences);
            let version = latest.as_ref().map(|(version, _)| *version).unwrap_or(0) + 1;
            *latest = Some((version, Value::Object(normalized)));
        }
        let processes = self.running();
        let results = futures_util::future::join_all(
            processes
                .iter()
                .map(|process| self.ensure_preferences(process)),
        )
        .await;
        results
            .into_iter()
            .collect::<Result<Vec<()>, RpcError>>()
            .map(|_| ())
    }

    /// 回收一个 workspace 的进程（关闭 workspace / 手动重启）。
    pub async fn dispose_workspace(&self, target: &WorkspaceTarget) {
        let slot = self.slot(&target.key());
        let process = slot.lock().await.take();
        if let Some(process) = process {
            process.client.close("dispose_workspace");
        }
    }

    /// Host 退出：关闭本 lane 与派生 lane 的全部进程并等待回收（上限 3 s）。
    pub async fn dispose_all(&self) {
        self.inner.disposed.store(true, Ordering::SeqCst);
        let slots: Vec<Slot> = lock(&self.inner.slots).values().cloned().collect();
        for slot in slots {
            if let Some(process) = slot.lock().await.take() {
                process.client.close("host_dispose");
            }
        }
        let lanes: Vec<AgentRuntime> = lock(&self.inner.lanes).clone();
        for lane in &lanes {
            Box::pin(lane.dispose_all()).await;
        }
        let monitors: Vec<JoinHandle<()>> = lock(&self.inner.monitors).drain(..).collect();
        let _ = tokio::time::timeout(DISPOSE_WAIT, futures_util::future::join_all(monitors)).await;
    }

    async fn release(&self, process: &AgentProcess) {
        let slot = self.slot(&process.key);
        let mut current = slot.lock().await;
        if current
            .as_ref()
            .is_some_and(|live| live.generation == process.generation)
        {
            *current = None;
        }
    }
}

fn lifecycle_event(process: &AgentProcess, state: &str) -> Value {
    let mut event = process.target.event_fields();
    event.insert("workspaceKey".into(), json!(process.key));
    event.insert("runtimeIdentity".into(), process.runtime_identity());
    event.insert("state".into(), json!(state));
    Value::Object(event)
}

/// `resolveZCodeAgentSpawnCwd`：命令自带 cwd 优先；用 workspace 路径但它不是目录时回退。
async fn spawn_cwd(
    command_cwd: Option<&Path>,
    workspace: &Path,
    fallback: Option<&Path>,
) -> PathBuf {
    let cwd = command_cwd.unwrap_or(workspace);
    if cwd == workspace
        && let Some(fallback) = fallback
        && !tokio::fs::metadata(cwd)
            .await
            .is_ok_and(|meta| meta.is_dir())
    {
        return fallback.to_owned();
    }
    cwd.to_owned()
}

/// 设置页的代理 / No Proxy / CA（agentProxyEnv.ts buildAgentRuntimeEnv）。
fn network_env(settings: &Value) -> BTreeMap<String, String> {
    let text = |key: &str| {
        settings[key]
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let mut env = BTreeMap::new();
    if let Some(proxy) = text("httpProxy") {
        let has_scheme = proxy.split_once("://").is_some_and(|(scheme, _)| {
            scheme
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
        });
        let proxy = if has_scheme {
            proxy
        } else {
            format!("http://{proxy}")
        };
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "ZCODE_HTTP_PROXY"] {
            env.insert(key.to_owned(), proxy.clone());
        }
    }
    if let Some(no_proxy) = text("httpProxyNoProxy") {
        let tokens: Vec<&str> = no_proxy
            .split(',')
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .collect();
        if !tokens.is_empty() {
            let joined = tokens.join(",");
            for key in ["NO_PROXY", "no_proxy", "ZCODE_NO_PROXY"] {
                env.insert(key.to_owned(), joined.clone());
            }
        }
    }
    if let Some(ca) = text("httpProxyCaCertPath") {
        env.insert("NODE_EXTRA_CA_CERTS".into(), ca.clone());
        env.insert("ZCODE_AGENT_CA_CERT".into(), ca);
    }
    env
}

fn is_method_not_found(error: &RpcError) -> bool {
    error.code_i64() == Some(-32601)
}

/// enqueueInteractionPreferenceSync：交互偏好失败即失败；模型 I/O 偏好只有 `-32601`（旧 CLI）可降级。
async fn push_preferences(process: &AgentProcess, preferences: &Value) -> Result<(), RpcError> {
    let workspace = process.target.workspace_ref();
    let interaction = json!({
        "workspace": workspace,
        "preferences": {
            "askUserQuestionAutoResolutionEnabled":
                preferences["askUserQuestionAutoResolutionEnabled"].as_bool().unwrap_or(true),
        },
    });
    if let Err(error) = process
        .request("workspace/updateInteractionPreferences", &interaction)
        .await
    {
        log::warn(format!(
            "updateInteractionPreferences failed for {}: {}",
            process.key, error.message
        ));
        return Err(error);
    }
    let model_io = json!({
        "workspace": workspace,
        "preferences": {
            "fullRetentionEnabled": preferences["modelIoFullRetentionEnabled"].as_bool() == Some(true),
        },
    });
    match process
        .request("workspace/updateModelIoPreferences", &model_io)
        .await
    {
        Err(error) if !is_method_not_found(&error) => {
            log::warn(format!(
                "updateModelIoPreferences failed for {}: {}",
                process.key, error.message
            ));
            Err(error)
        }
        _ => Ok(()),
    }
}

/// 功能门禁打开时推 workspace 级策略（尽力而为：失败只 warn，`-32601` 是旧 CLI 的正常降级）。
async fn push_policies(extensions: &Extensions, process: &AgentProcess) {
    let workspace = process.target.workspace_ref();
    let mut policies = Vec::new();
    if extensions.off_peak_tools_for(&process.target) {
        policies.push("workspace/updateOffPeakToolPolicy");
    }
    if extensions.dynamic_workflow_enabled().await {
        policies.push("workspace/updateDynamicWorkflowPolicy");
    }
    for method in policies {
        let params = json!({"workspace": workspace, "enabled": true});
        if let Err(error) = process.request(method, &params).await
            && !is_method_not_found(&error)
        {
            log::warn(format!(
                "{method} failed for {}, CLI keeps the default (disabled): {}",
                process.key, error.message
            ));
        }
    }
}

/// mcp-status lane 的空闲回收：无在途请求、存储不在等待，且最后一次请求活动已过阈值。
async fn idle_reclaim(process: Arc<AgentProcess>, timeout: Duration) {
    let interval = (timeout / 4).clamp(Duration::from_millis(10), Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = process.client.closed() => return,
        }
        let client = &process.client;
        let idle_ms = now_ms().saturating_sub(client.last_activity_ms());
        if client.in_flight() == 0
            && !client.storage_waiting()
            && u128::from(idle_ms) >= timeout.as_millis()
        {
            log::info(format!(
                "agent[{}] idle for {idle_ms} ms; reclaiming",
                process.key
            ));
            client.close("idle_timeout");
            return;
        }
    }
}

/// 关闭原因 → (terminationKind, terminationReason)（zcodeAgentProcessManager recordTerminationIntent）。
fn termination(reason: &str) -> (&'static str, &'static str) {
    match reason {
        "dispose_workspace" => ("expected", "workspace-dispose"),
        "host_dispose" => ("expected", "manager-dispose"),
        "idle_timeout" => ("expected", "idle-timeout"),
        "watchdog_recycle" => ("watchdog_recycle", "request-timeout"),
        _ => ("unexpected", "protocol-close"),
    }
}

async fn monitor(
    runtime: AgentRuntime,
    mut child: Child,
    process: Arc<AgentProcess>,
    tail: StderrTail,
    stderr_done: process::StderrDone,
    label: String,
) {
    let reason = tokio::select! {
        status = child.wait() => {
            let status = status.map(|status| status.to_string()).unwrap_or_else(|error| error.to_string());
            // 进程先退出时给 stdout 里剩下的行一点时间被读完。
            let reason = tokio::time::timeout(EXIT_FLUSH_WAIT, process.client.closed())
                .await
                .unwrap_or_else(|_| format!("process_exit: {status}"));
            process.client.close(&reason);
            reason
        }
        reason = process.client.closed() => reason,
    };
    runtime.release(&process).await;
    runtime
        .inner
        .lifecycle
        .emit(lifecycle_event(&process, "unavailable"));
    let (kind, termination_reason) = termination(&reason);
    if kind == "unexpected" {
        log::warn(format!(
            "{label} closed: {reason}; stderr tail:\n{}",
            tail.lines().join("\n")
        ));
    } else {
        log::info(format!("{label} closed: {reason}"));
    }
    process.client.close_input().await;
    process::terminate(&mut child, process.pid).await;
    // 退出先于 stderr EOF 时收齐最后的诊断行（上限 1 s）。
    let _ = tokio::time::timeout(EXIT_FLUSH_WAIT, stderr_done).await;
    let status = child.try_wait().ok().flatten();
    if let Some(mut message) = process.lifecycle_base() {
        let ended_at = now_ms();
        message.insert("type".into(), json!("agent-process-exited"));
        message.insert(
            "exitCode".into(),
            json!(status.and_then(|status| status.code())),
        );
        message.insert(
            "signal".into(),
            json!(status.and_then(process::exit_signal_name)),
        );
        message.insert("endedAt".into(), json!(ended_at));
        message.insert("terminationKind".into(), json!(kind));
        message.insert("terminationReason".into(), json!(termination_reason));
        message.insert(
            "runtimeReady".into(),
            json!(process.ready_at_ms.get().is_some()),
        );
        message.insert("runtimeGeneration".into(), json!(process.generation));
        message.insert("runtimeInstanceId".into(), json!(process.instance_id));
        message.insert(
            "uptimeMs".into(),
            json!(ended_at.saturating_sub(process.started_at_ms)),
        );
        message.insert("stderrLineCount".into(), json!(tail.line_count()));
        let lines = tail.lines();
        if kind == "unexpected" && !lines.is_empty() {
            message.insert("stderrTail".into(), json!(lines));
        }
        runtime.emit_process_event(Value::Object(message));
    }
}

struct Handler {
    key: String,
    target: WorkspaceTarget,
    extensions: Arc<Extensions>,
    generation: u64,
    lane: Lane,
    settings: SettingStore,
    frames: EventHub<Arc<Frame>>,
}

impl Handler {
    /// 非帧通知与可观察的反向请求进通知总线（docs/specs/rust-host-agent.md §2.9）。
    fn publish(&self, method: &str, params: Option<&RawValue>, request: bool) {
        let params = params
            .and_then(|raw| serde_json::from_str::<Value>(raw.get()).ok())
            .unwrap_or(Value::Null);
        self.extensions
            .notifications()
            .emit(Arc::new(AgentNotification {
                target: self.target.clone(),
                workspace_key: self.key.clone(),
                generation: self.generation,
                lane: self.lane,
                method: method.to_owned(),
                params,
                request,
            }));
    }
}

impl ClientHandler for Handler {
    fn notification(&self, _client: &Arc<ProtocolClient>, notification: Notification) {
        if let Some(hook) = self.extensions.notification_hook(&notification.method) {
            let params = notification
                .params
                .as_deref()
                .and_then(|raw| serde_json::from_str(raw.get()).ok())
                .unwrap_or(Value::Null);
            hook(super::extensions::NotificationContext {
                target: self.target.clone(),
                workspace_key: self.key.clone(),
                generation: self.generation,
                method: notification.method.clone(),
                params,
            });
            return;
        }
        if notification.method != "v4/conversation/frame" {
            self.publish(&notification.method, notification.params.as_deref(), false);
            return;
        }
        let Some(raw) = notification.params else {
            log::warn("v4 frame without params dropped");
            return;
        };
        match parse_frame(raw, &self.key, self.generation) {
            Ok(frame) => self.frames.emit(Arc::new(frame)),
            Err(error) => log::warn(format!("invalid v4 frame dropped: {error}")),
        }
    }

    fn request(&self, client: &Arc<ProtocolClient>, request: ReverseRequest) {
        let client = client.clone();
        let params: Value = request
            .params
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw.get()).ok())
            .unwrap_or(Value::Null);
        if let Some(handler) = self.extensions.reverse(&request.method) {
            let context = ReverseContext {
                target: self.target.clone(),
                workspace_key: self.key.clone(),
                generation: self.generation,
                method: request.method.clone(),
                params,
            };
            tokio::spawn(async move {
                match handler(context).await {
                    ReverseReply::Result(result) => client.respond(&request.id, &result).await,
                    ReverseReply::Error { code, message } => {
                        client.respond_error(&request.id, code, &message).await
                    }
                    ReverseReply::ErrorWithData {
                        code,
                        message,
                        data,
                    } => {
                        client
                            .respond_error_data(&request.id, code, &message, &data)
                            .await
                    }
                    ReverseReply::Hold => {}
                }
            });
            return;
        }
        let reply = match request.method.as_str() {
            "session/requestRuntimePreferences" => {
                let settings = self.settings.clone();
                tokio::spawn(async move {
                    let result = runtime_preferences(&settings.get().await, &params);
                    client.respond(&request.id, &result).await;
                });
                return;
            }
            // 权限与提问：CLI 拿这个反向请求和 V4 resolveInteraction 赛跑；回错误会让它在用户作答前
            // 失败，所以永远挂着不答。CLI 会用新 id 重复发送同一个 requestId，同样忽略。
            // legacy 会话事件面仍需观察到它们（permission.request / userInput.request）。
            "interaction/requestPermission" | "interaction/requestUserInput" => {
                self.publish(&request.method, request.params.as_deref(), true);
                return;
            }
            "interaction/requestProviderRuntimeHeaders" => Ok(json!({
                "headersApplied": false,
                "errorMessage": "Provider request auth is unavailable",
            })),
            "interaction/requestOfficialMcpAuthHeaders" => {
                Ok(json!({"ok": false, "reason": "official_auth_unavailable"}))
            }
            "interaction/browserList" => Ok(json!({"browsers": []})),
            "interaction/browserExecute" => Ok(json!({
                "ok": false,
                "error": {"code": "backend_unavailable", "message": "browser control not available"},
                "elapsedMs": 0,
            })),
            other => Err(format!("Unsupported ZCode Protocol request: {other}")),
        };
        tokio::spawn(async move {
            match reply {
                Ok(result) => client.respond(&request.id, &result).await,
                Err(message) => client.respond_error(&request.id, -32601, &message).await,
            }
        });
    }
}

/// 本地 authority 的会话运行时偏好（packages/services/src/node.ts resolveSessionRuntimePreferences）。
fn runtime_preferences(settings: &Value, params: &Value) -> Value {
    let mut result = Map::new();
    result.insert(
        "askUserQuestionAutoResolutionEnabled".into(),
        json!(settings["askUserQuestionAutoResolutionEnabled"] != json!(false)),
    );
    result.insert(
        "nativeSearchEnhancementsEnabled".into(),
        json!(settings["nativeSearchEnhancementsEnabled"] != json!(false)),
    );
    result.insert(
        "memoryEnabled".into(),
        json!(settings["memoryEnabled"] == json!(true)),
    );
    result.insert("modelContextBudgetStrategy".into(), json!("preflight-v1"));
    let shell = &settings["integratedTerminalShell"];
    if params["scope"] == "user-execution" && !shell.is_null() {
        result.insert("integratedTerminalShell".into(), shell.clone());
    }
    Value::Object(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_key_and_ref() {
        let local =
            WorkspaceTarget::from_value(&json!({"workspacePath": "/w", "workspaceIdentity": "  "}))
                .unwrap();
        assert_eq!(local.key(), "/w");
        assert_eq!(
            local.workspace_ref(),
            json!({"workspacePath": "/w", "workspaceKey": "/w"})
        );
        let remote = WorkspaceTarget::from_value(
            &json!({"workspacePath": "/w", "workspaceIdentity": " remote:1 ", "extra": 1}),
        )
        .unwrap();
        assert_eq!(remote.key(), "remote:1");
        assert!(WorkspaceTarget::from_value(&json!({"workspacePath": " "})).is_err());
    }

    #[test]
    fn frames_route_by_topic_prefix() {
        let raw = |topic: &str| {
            RawValue::from_string(format!(
                r#"{{"wireVersion":3,"kind":"complete","deliveryKind":"online","logicalFrameId":"f","logicalFrameOrdinal":0,"topic":"{topic}","subscriptionId":"s","frame":{{}}}}"#
            ))
            .unwrap()
        };
        let frame = parse_frame(raw("conversation/abc"), "/w", 1).unwrap();
        assert_eq!(frame.kind, FrameKind::Conversation);
        assert!(frame.raw.get().contains(r#""frame":{}"#));
        assert_eq!(
            parse_frame(raw("sessions-index//w"), "/w", 1).unwrap().kind,
            FrameKind::SessionsIndex
        );
        assert!(parse_frame(raw("other/x"), "/w", 1).is_err());
    }

    #[test]
    fn proxy_env_normalizes_like_typescript() {
        let env = network_env(&json!({
            "httpProxy": " 127.0.0.1:7890 ",
            "httpProxyNoProxy": "localhost, ,127.0.0.1",
            "httpProxyCaCertPath": "",
        }));
        assert_eq!(env["HTTPS_PROXY"], "http://127.0.0.1:7890");
        assert_eq!(env["no_proxy"], "localhost,127.0.0.1");
        assert!(!env.contains_key("NODE_EXTRA_CA_CERTS"));
        assert_eq!(
            network_env(&json!({"httpProxy": "socks5://h:1"}))["ALL_PROXY"],
            "socks5://h:1"
        );
    }

    #[test]
    fn runtime_preferences_follow_settings() {
        let settings = json!({"memoryEnabled": true, "nativeSearchEnhancementsEnabled": false, "integratedTerminalShell": {"id": "x"}});
        let materialize =
            runtime_preferences(&settings, &json!({"scope": "runtime-materialization"}));
        assert_eq!(
            materialize,
            json!({
                "askUserQuestionAutoResolutionEnabled": true,
                "nativeSearchEnhancementsEnabled": false,
                "memoryEnabled": true,
                "modelContextBudgetStrategy": "preflight-v1",
            })
        );
        let execute = runtime_preferences(&settings, &json!({"scope": "user-execution"}));
        assert_eq!(execute["integratedTerminalShell"], json!({"id": "x"}));
    }
}
