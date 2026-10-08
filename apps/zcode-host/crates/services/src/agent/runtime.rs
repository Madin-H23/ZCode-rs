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
use super::process::{self, StderrTail};
use crate::providers::ProviderRuntime;
use crate::settings::SettingStore;

pub const RUNTIME_UNAVAILABLE_CODE: &str = "ZCODE_AGENT_RUNTIME_UNAVAILABLE";
pub const PROVIDER_NOT_READY_CODE: &str = "ZCODE_AGENT_PROVIDER_NOT_READY";
const EXIT_FLUSH_WAIT: Duration = Duration::from_secs(1);
const DISPOSE_WAIT: Duration = Duration::from_secs(3);

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

    fn event_fields(&self) -> Map<String, Value> {
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
        let pid = self.pid.map(|pid| pid.to_string()).unwrap_or_default();
        let mut identity = Map::new();
        identity.insert("generation".into(), json!(self.generation));
        identity.insert(
            "identity".into(),
            json!(format!("{}:{}:{pid}", self.key, self.generation)),
        );
        if let Some(pid) = self.pid {
            identity.insert("processId".into(), json!(pid));
        }
        identity.insert("workspaceKey".into(), json!(self.key));
        Value::Object(identity)
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

pub struct AgentRuntimeConfig {
    /// 清洗后的 Host 环境 + Main 的登录 shell 补丁（runtime_env::effective_base_env）。
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
    app_preferences: Mutex<Option<Value>>,
    monitors: Mutex<Vec<JoinHandle<()>>>,
    disposed: AtomicBool,
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
            }),
        }
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
    pub async fn get_client(
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
        self.get_client(target, RuntimePolicy::StartIfNeeded).await
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

    async fn start(&self, target: &WorkspaceTarget) -> Result<Arc<AgentProcess>, RpcError> {
        let config = &self.inner.config;
        let workspace = Path::new(&target.workspace_path);
        let command = command::resolve(&config.command_vars, &config.search_roots, workspace)
            .map_err(|error| RpcError::new("Error", format!("{error:#}")))?
            .ok_or_else(|| {
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
        let spawned = process::spawn(&command, &cwd, &env).map_err(|error| {
            RpcError::new(
                "Error",
                format!(
                    "failed to start ZCode agent {}: {error}",
                    command.program.display()
                ),
            )
        })?;
        let key = target.key();
        let generation = {
            let mut generations = lock(&self.inner.generations);
            let next = generations.entry(key.clone()).or_default();
            *next += 1;
            *next
        };
        let label = format!("agent[{key}#{generation}]");
        log::info(format!(
            "{label} started pid={} storageGate={}",
            spawned.pid.map(|pid| pid.to_string()).unwrap_or_default(),
            command.supports_storage_startup
        ));
        let tail = process::drain_stderr(spawned.stderr, label.clone());
        let handler = Arc::new(Handler {
            key: key.clone(),
            generation,
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
        });
        if generation > 1 {
            self.inner.restarted.emit(json!({
                "workspaceKey": process.key,
                "runtimeIdentity": process.runtime_identity(),
            }));
        }
        self.inner
            .lifecycle
            .emit(lifecycle_event(&process, "available"));
        let monitor = tokio::spawn(monitor(
            self.clone(),
            spawned.child,
            process.clone(),
            tail,
            label,
        ));
        {
            let mut monitors = lock(&self.inner.monitors);
            monitors.retain(|handle| !handle.is_finished());
            monitors.push(monitor);
        }
        let preferences = lock(&self.inner.app_preferences).clone();
        if let Some(preferences) = preferences {
            push_preferences(&process, &preferences).await;
        }
        Ok(process)
    }

    /// `syncAppRuntimePreferences`：记住最新值，推给正在运行的进程；新进程启动时回放。
    pub fn sync_app_preferences(&self, preferences: Value) {
        *lock(&self.inner.app_preferences) = Some(preferences.clone());
        for process in self.running() {
            let preferences = preferences.clone();
            tokio::spawn(async move { push_preferences(&process, &preferences).await });
        }
    }

    /// 回收一个 workspace 的进程（关闭 workspace / 手动重启）。
    pub async fn dispose_workspace(&self, target: &WorkspaceTarget) {
        let slot = self.slot(&target.key());
        let process = slot.lock().await.take();
        if let Some(process) = process {
            process.client.close("dispose_workspace");
        }
    }

    /// Host 退出：关闭全部进程并等待回收（上限 3 s）。
    pub async fn dispose_all(&self) {
        self.inner.disposed.store(true, Ordering::SeqCst);
        let slots: Vec<Slot> = lock(&self.inner.slots).values().cloned().collect();
        for slot in slots {
            if let Some(process) = slot.lock().await.take() {
                process.client.close("host_dispose");
            }
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

/// 偏好推送失败只记日志：CLI 用自己的默认值继续工作。
async fn push_preferences(process: &AgentProcess, preferences: &Value) {
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
            "updateInteractionPreferences failed: {}",
            error.message
        ));
        return;
    }
    let model_io = json!({
        "workspace": workspace,
        "preferences": {
            "fullRetentionEnabled": preferences["modelIoFullRetentionEnabled"].as_bool() == Some(true),
        },
    });
    if let Err(error) = process
        .request("workspace/updateModelIoPreferences", &model_io)
        .await
        && error.code_i64() != Some(-32601)
    {
        log::warn(format!(
            "updateModelIoPreferences failed: {}",
            error.message
        ));
    }
}

async fn monitor(
    runtime: AgentRuntime,
    mut child: Child,
    process: Arc<AgentProcess>,
    tail: StderrTail,
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
            format!("{reason} ({status})")
        }
        reason = process.client.closed() => reason,
    };
    runtime.release(&process).await;
    runtime
        .inner
        .lifecycle
        .emit(lifecycle_event(&process, "unavailable"));
    let expected = matches!(reason.as_str(), "dispose_workspace" | "host_dispose");
    if expected {
        log::info(format!("{label} closed: {reason}"));
    } else {
        log::warn(format!(
            "{label} closed: {reason}; stderr tail:\n{}",
            tail.lines().join("\n")
        ));
    }
    process.client.close_input().await;
    process::terminate(&mut child, process.pid).await;
}

struct Handler {
    key: String,
    generation: u64,
    settings: SettingStore,
    frames: EventHub<Arc<Frame>>,
}

impl ClientHandler for Handler {
    fn notification(&self, _client: &Arc<ProtocolClient>, notification: Notification) {
        if notification.method != "v4/conversation/frame" {
            log::debug(format!(
                "agent[{}] notification ignored: {}",
                self.key, notification.method
            ));
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
            "interaction/requestPermission" | "interaction/requestUserInput" => return,
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
