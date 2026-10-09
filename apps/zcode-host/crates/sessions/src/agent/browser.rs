//! 浏览器控制：CLI 的 `interaction/browserList` / `interaction/browserExecute` 经 Main 的
//! WebContentsView + CDP 执行（docs/specs/rust-host-agent.md §2.5；browserControlMainBridge.ts、
//! zcodeAgentBrowserAmbientContext.ts、browserRecordingArtifactMaterializer.ts）。
//!
//! ```text
//! CLI ─ browserExecute ─► execute：校验 id/代际、requestId 去重、登记在途 + 计时器
//!                          ├─► Main browser-execute-request
//!                          │◄─ Main browser-execute-result（ControlRouter → handle_result）
//!                          ├─ recordingStatus 完成且带 outputPath：复制到 workspace
//!                          └─ 超时：摘除在途 → Main cancelRequest → timeout（sideEffect 按命令）
//! ```
//! 在途表只有这里写；迟到的结果在表里找不到，直接丢弃。
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::sync::oneshot;
use zcode_host_control::{ControlRouter, WindowId};
use zcode_host_kit::log;

use super::extensions::{Extensions, ReverseContext, ReverseReply};
use super::schema::{self, Field, Ty};
use super::service::uuid_v4;
use crate::main_bridge::MainBridge;

pub const EXECUTE_REQUEST: &str = "browser-execute-request";
pub const EXECUTE_RESULT: &str = "browser-execute-result";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const TRANSPORT_GRACE: Duration = Duration::from_secs(2);
const MAX_AMBIENT_URL_CHARS: usize = 4096;

/// 一次执行的参数（BrowserControlMainBridge.execute 的 input）。
#[derive(Debug, Clone, Default)]
pub struct ExecuteInput {
    pub request_id: Option<String>,
    pub browser_id: Option<String>,
    pub browser_generation: Option<u64>,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub workspace_key: Option<String>,
    pub workspace_path: Option<String>,
    pub workspace_identity: Option<String>,
    pub remote_session_id: Option<String>,
    pub client_mode: Option<String>,
    pub session_context: Option<String>,
    /// 发起调用的连接所属主窗口（rust-u4-app-host.md §4.1 `browser-execute-request.windowId`）。只有能直接
    /// 归属到连接的请求才有：发送时收集浏览器环境（ambient context）。CLI 回合中的 `browserExecute`
    /// 没有“回合 → 连接”的既有关联，省略，Main 用任意一个主窗口。
    pub window_id: Option<WindowId>,
    pub command: Value,
}

struct PendingEntry {
    sender: oneshot::Sender<Value>,
    /// 区分同 requestId 的不同登记（只允许结算自己）。
    token: u64,
}

struct Inner {
    main: Option<MainBridge>,
    browser_id: String,
    generation: u64,
    timeout: Duration,
    pending: Mutex<HashMap<String, PendingEntry>>,
    next_token: std::sync::atomic::AtomicU64,
    closed: std::sync::atomic::AtomicBool,
}

#[derive(Clone)]
pub struct BrowserBridge {
    inner: Arc<Inner>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn failure(
    code: &str,
    message: impl Into<String>,
    side_effect: Option<&str>,
    elapsed: u64,
) -> Value {
    let mut error = Map::new();
    error.insert("code".into(), json!(code));
    error.insert("message".into(), json!(message.into()));
    if let Some(side_effect) = side_effect {
        error.insert("sideEffect".into(), json!(side_effect));
    }
    json!({"ok": false, "error": error, "elapsedMs": elapsed})
}

/// mayHaveSideEffects：超时时无法证明动作是否已下发，有副作用的命令标记 uncertain。
fn may_have_side_effects(command: &Value) -> bool {
    let method = command["method"].as_str().unwrap_or_default();
    if method == "playwright" && command["action"]["name"] == "locator" {
        return matches!(
            command["action"]["operation"].as_str(),
            Some(
                "click"
                    | "dblclick"
                    | "downloadMedia"
                    | "fill"
                    | "press"
                    | "selectOption"
                    | "setChecked"
            )
        );
    }
    matches!(
        method,
        "navigate"
            | "back"
            | "forward"
            | "reload"
            | "click"
            | "fill"
            | "type"
            | "press"
            | "cuaKeypress"
            | "scroll"
            | "cuaScroll"
            | "domCuaScroll"
            | "hover"
            | "select"
            | "check"
            | "drag"
            | "cuaDrag"
            | "recordingStart"
            | "recordingCancel"
            | "handleDialog"
            | "close"
            | "finalize"
            | "newTab"
    )
}

fn millis(value: &Value) -> Option<Duration> {
    value
        .as_f64()
        .filter(|ms| ms.is_finite() && *ms >= 0.0)
        .map(|ms| Duration::from_millis(ms as u64))
}

impl BrowserBridge {
    pub fn new(main: Option<MainBridge>) -> Self {
        Self::with_timeout(main, DEFAULT_TIMEOUT)
    }

    pub fn with_timeout(main: Option<MainBridge>, timeout: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                main,
                browser_id: format!("iab:{}", uuid_v4()),
                generation: now_ms(),
                timeout,
                pending: Mutex::new(HashMap::new()),
                next_token: std::sync::atomic::AtomicU64::new(1),
                closed: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<String, PendingEntry>> {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 登记 Main 的结果消息与两个反向请求处理器。
    pub fn install(&self, router: &mut ControlRouter, extensions: &Extensions) {
        let bridge = self.clone();
        router.on(EXECUTE_RESULT, move |message| {
            bridge.handle_result(&message)
        });
        let bridge = self.clone();
        extensions.register_reverse(
            "interaction/browserList",
            Arc::new(move |context| {
                let bridge = bridge.clone();
                Box::pin(async move { bridge.reverse_list(&context) })
            }),
        );
        let bridge = self.clone();
        extensions.register_reverse(
            "interaction/browserExecute",
            Arc::new(move |context| {
                let bridge = bridge.clone();
                Box::pin(async move { bridge.reverse_execute(context).await })
            }),
        );
    }

    /// IAB 描述（与 browserControlMainBridge.ts 逐字相同）。
    pub fn descriptor(&self) -> Value {
        json!({
            "id": self.inner.browser_id,
            "generation": self.inner.generation,
            "type": "iab",
            "name": "ZCode In-app Browser",
            "capabilities": {
                "browser": [{
                    "id": "visibility",
                    "description": "Use to show or hide the browser to the user, and to determine the browser's current visibility. Keep browser work in the background unless the user asks to see it or live viewing is useful. When the browser should be visible, call set(true).",
                }],
                "tab": [],
            },
            "apiSupportOverrides": {
                "BrowserUser.claimTab": true,
                "Tabs.finalize": true,
                "Tab.markDeliverable": true,
                "Tab.markHandoff": true,
                "BrowserRecordingAPI.start": true,
                "BrowserRecordingAPI.status": true,
                "BrowserRecordingAPI.cancel": true,
            },
            "metadata": {"provider": "zcode-desktop-iab"},
        })
    }

    pub fn list(&self) -> Vec<Value> {
        vec![self.descriptor()]
    }

    fn request_timeout(&self, command: &Value) -> Duration {
        match command["method"].as_str() {
            Some("playwrightWaitForTimeout") => {
                millis(&command["timeoutMs"]).unwrap_or_default() + TRANSPORT_GRACE
            }
            Some("playwright") => {
                let action = &command["action"];
                let timeout = if action.get("timeoutMs").is_some() {
                    millis(&action["timeoutMs"]).unwrap_or(self.inner.timeout)
                } else {
                    self.inner.timeout
                };
                timeout + TRANSPORT_GRACE
            }
            _ => self.inner.timeout,
        }
    }

    fn request_message(&self, request_id: &str, input: &ExecuteInput, command: &Value) -> Value {
        let workspace_key = input
            .workspace_key
            .clone()
            .unwrap_or_else(|| input.session_id.clone());
        let workspace_path = input
            .workspace_path
            .clone()
            .unwrap_or_else(|| workspace_key.clone());
        let mut message = Map::new();
        message.insert("type".into(), json!(EXECUTE_REQUEST));
        message.insert("requestId".into(), json!(request_id));
        message.insert("browserId".into(), json!(self.inner.browser_id));
        message.insert("browserGeneration".into(), json!(self.inner.generation));
        message.insert("sessionId".into(), json!(input.session_id));
        if let Some(turn_id) = &input.turn_id {
            message.insert("turnId".into(), json!(turn_id));
        }
        message.insert("workspaceKey".into(), json!(workspace_key));
        message.insert("workspacePath".into(), json!(workspace_path));
        if let Some(identity) = &input.workspace_identity {
            message.insert("workspaceIdentity".into(), json!(identity));
        }
        if let Some(remote) = &input.remote_session_id {
            message.insert("remoteSessionId".into(), json!(remote));
        }
        message.insert(
            "clientMode".into(),
            json!(input.client_mode.as_deref().unwrap_or("desktop-continuous")),
        );
        message.insert(
            "sessionContext".into(),
            json!(input.session_context.as_deref().unwrap_or("live")),
        );
        if let Some(window_id) = input.window_id {
            message.insert("windowId".into(), json!(window_id.get()));
        }
        message.insert("command".into(), command.clone());
        Value::Object(message)
    }

    pub async fn execute(&self, input: ExecuteInput) -> Value {
        let inner = &self.inner;
        if let Some(requested) = &input.browser_id
            && *requested != inner.browser_id
        {
            return failure(
                "backend_unavailable",
                format!("browser backend '{requested}' is no longer available"),
                None,
                0,
            );
        }
        if let Some(generation) = input.browser_generation
            && generation != inner.generation
        {
            return failure(
                "backend_unavailable",
                format!(
                    "browser backend '{}' generation {generation} is stale",
                    inner.browser_id
                ),
                None,
                0,
            );
        }
        let request_id = input.request_id.clone().unwrap_or_else(uuid_v4);
        let started = std::time::Instant::now();
        let elapsed = || started.elapsed().as_millis() as u64;
        let (sender, receiver) = oneshot::channel();
        let token = inner
            .next_token
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        {
            let mut pending = self.pending();
            if pending.contains_key(&request_id) {
                // requestId 是结果与等待者的关联键，覆盖同键登记会把旧结果交给新请求。
                return failure(
                    "duplicate_request_id",
                    format!("browser requestId '{request_id}' is already running"),
                    Some("none"),
                    0,
                );
            }
            pending.insert(request_id.clone(), PendingEntry { sender, token });
        }
        let command = input.command.clone();
        let timeout = self.request_timeout(&command);
        let Some(main) = inner
            .main
            .as_ref()
            .filter(|_| !inner.closed.load(std::sync::atomic::Ordering::SeqCst))
        else {
            self.take_if(&request_id, token);
            return failure(
                "backend_unavailable",
                "parentPort unavailable",
                None,
                elapsed(),
            );
        };
        main.post(self.request_message(&request_id, &input, &command));
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(result)) => self.finish(result, &input, &command, elapsed()).await,
            // 在途表被清空（Host 退出）：结果不可判定。
            Ok(Err(_)) => failure(
                "backend_unavailable",
                "browser bridge disposed while command was pending",
                Some("uncertain"),
                elapsed(),
            ),
            Err(_) => {
                if !self.take_if(&request_id, token) {
                    return failure(
                        "backend_unavailable",
                        "browser bridge disposed while command was pending",
                        Some("uncertain"),
                        elapsed(),
                    );
                }
                // 只结束本地等待会让 Main 里的动作继续执行；用同一 scope 发反向 cancel。
                let cancel = json!({"method": "cancelRequest", "requestId": request_id});
                main.post(self.request_message(&uuid_v4(), &input, &cancel));
                let method = command["method"].as_str().unwrap_or_default();
                failure(
                    "timeout",
                    format!("browser 命令 {method} 超时（{}ms）", timeout.as_millis()),
                    Some(if may_have_side_effects(&command) {
                        "uncertain"
                    } else {
                        "none"
                    }),
                    elapsed(),
                )
            }
        }
    }

    /// 只摘除自己登记的那一项。
    fn take_if(&self, request_id: &str, token: u64) -> bool {
        let mut pending = self.pending();
        if pending
            .get(request_id)
            .is_some_and(|entry| entry.token == token)
        {
            pending.remove(request_id);
            true
        } else {
            false
        }
    }

    async fn finish(
        &self,
        result: Value,
        input: &ExecuteInput,
        command: &Value,
        elapsed: u64,
    ) -> Value {
        let recording = &result["recording"];
        let completed = result["ok"] == true && recording["status"] == "completed";
        let artifact = recording
            .get("artifact")
            .filter(|artifact| artifact.is_object());
        let output_path = (command["method"] == "recordingStatus")
            .then(|| command["outputPath"].as_str())
            .flatten()
            .filter(|path| !path.is_empty());
        match (completed, artifact, output_path) {
            (true, Some(artifact), Some(output_path)) => {
                match materialize_recording(artifact, output_path, input).await {
                    Ok(materialized) => {
                        let mut result = result.clone();
                        result["recording"]["artifact"] = materialized;
                        result
                    }
                    Err(message) => failure("execution_error", message, Some("none"), elapsed),
                }
            }
            // 未带 outputPath 时 Main 的临时录制路径不能交给 Agent。
            (_, Some(_), None) if recording["status"] == "completed" => {
                let mut result = result.clone();
                if let Some(recording) = result["recording"].as_object_mut() {
                    recording.remove("artifact");
                }
                result
            }
            _ => result,
        }
    }

    /// Main 的 `browser-execute-result`。
    pub fn handle_result(&self, message: &Value) {
        let Some(request_id) = message["requestId"].as_str() else {
            log::warn("browser-execute-result without requestId");
            return;
        };
        let entry = self.pending().remove(request_id);
        if let Some(entry) = entry {
            let _ = entry.sender.send(message["result"].clone());
        }
    }

    /// Host 退出：在途请求以 `backend_unavailable`、`sideEffect:"uncertain"` 结束。
    pub fn dispose(&self) {
        self.inner
            .closed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.pending().clear();
    }

    fn reverse_list(&self, context: &ReverseContext) -> ReverseReply {
        let Some(object) = context.params.as_object() else {
            return invalid("interaction/browserList");
        };
        if !schema::check_object(object, LIST_PARAMS) || !trimmed_non_empty(object) {
            return invalid("interaction/browserList");
        }
        ReverseReply::Result(json!({"browsers": self.list()}))
    }

    async fn reverse_execute(&self, context: ReverseContext) -> ReverseReply {
        let Some(input) = parse_execute(&context) else {
            return invalid("interaction/browserExecute");
        };
        ReverseReply::Result(self.execute(input).await)
    }

    /// collectBrowserAmbientContext：只读收集；任何失败都返回 None，不能阻断发送。
    pub async fn ambient_context(
        &self,
        session_id: &str,
        target: &super::WorkspaceTarget,
        client_mode: &str,
        window_id: Option<WindowId>,
    ) -> Option<Value> {
        let browser = self
            .list()
            .into_iter()
            .find(|browser| browser["type"] == "iab")?;
        let base = ExecuteInput {
            browser_id: browser["id"].as_str().map(str::to_owned),
            browser_generation: browser["generation"].as_u64(),
            session_id: session_id.to_owned(),
            workspace_key: Some(target.key()),
            workspace_path: Some(target.workspace_path.clone()),
            workspace_identity: target.workspace_identity.clone(),
            remote_session_id: target.remote_session_id.clone(),
            client_mode: Some(client_mode.to_owned()),
            session_context: Some("live".into()),
            window_id,
            ..ExecuteInput::default()
        };
        let controlled = ExecuteInput {
            request_id: Some(uuid_v4()),
            command: json!({"method": "list"}),
            ..base.clone()
        };
        let users = ExecuteInput {
            request_id: Some(uuid_v4()),
            command: json!({"method": "listUserTabs"}),
            ..base
        };
        let (controlled, users) = tokio::join!(self.execute(controlled), self.execute(users));
        let tabs = |result: &Value, key: &str| -> Vec<Value> {
            if result["ok"] == true {
                result[key].as_array().cloned().unwrap_or_default()
            } else {
                Vec::new()
            }
        };
        let controlled_tabs = tabs(&controlled, "tabs");
        let user_tabs = tabs(&users, "userTabs");
        let mut ids = std::collections::BTreeSet::new();
        for tab in &controlled_tabs {
            ids.insert(tab["tabId"].to_string());
        }
        for tab in &user_tabs {
            ids.insert(tab["id"].to_string());
        }
        if ids.is_empty() {
            return None;
        }
        // 显式 finalize 后页面属于 user tabs；漏 finalize 的 handoff 仍属于 controlled tabs。
        let url = controlled_tabs
            .iter()
            .find(|tab| tab["active"] == true)
            .and_then(|tab| tab["url"].as_str())
            .or_else(|| user_tabs.first().and_then(|tab| tab["url"].as_str()))
            .or_else(|| controlled_tabs.last().and_then(|tab| tab["url"].as_str()));
        let mut context = Map::new();
        context.insert("tabCount".into(), json!(ids.len()));
        if let Some(url) = url.and_then(sanitize_ambient_url) {
            context.insert("currentUrl".into(), json!(url));
        }
        Some(Value::Object(context))
    }
}

fn invalid(method: &str) -> ReverseReply {
    ReverseReply::Error {
        code: -32602,
        message: format!("Invalid {method} params"),
    }
}

const NONEMPTY: Ty = Ty::Str { min: 1, max: None };
const CLIENT_MODE: Ty = Ty::Enum(&["desktop-continuous", "web-remote-replayable"]);
const SESSION_CONTEXT: Ty = Ty::Enum(&["live", "cached"]);
const LIST_PARAMS: &[Field] = &[
    Field {
        name: "requestId",
        ty: NONEMPTY,
        required: true,
    },
    Field {
        name: "sessionId",
        ty: NONEMPTY,
        required: true,
    },
    Field {
        name: "turnId",
        ty: NONEMPTY,
        required: false,
    },
    Field {
        name: "workspaceKey",
        ty: NONEMPTY,
        required: true,
    },
    Field {
        name: "workspacePath",
        ty: NONEMPTY,
        required: true,
    },
    Field {
        name: "workspaceIdentity",
        ty: NONEMPTY,
        required: false,
    },
    Field {
        name: "remoteSessionId",
        ty: NONEMPTY,
        required: false,
    },
    Field {
        name: "clientMode",
        ty: CLIENT_MODE,
        required: true,
    },
    Field {
        name: "sessionContext",
        ty: SESSION_CONTEXT,
        required: true,
    },
];
const EXECUTE_PARAMS: &[Field] = &[
    Field {
        name: "requestId",
        ty: NONEMPTY,
        required: true,
    },
    Field {
        name: "sessionId",
        ty: NONEMPTY,
        required: true,
    },
    Field {
        name: "turnId",
        ty: NONEMPTY,
        required: false,
    },
    Field {
        name: "browserId",
        ty: NONEMPTY,
        required: false,
    },
    Field {
        name: "browserGeneration",
        ty: Ty::Int {
            nonnegative: true,
            positive: false,
        },
        required: false,
    },
    Field {
        name: "workspaceKey",
        ty: NONEMPTY,
        required: false,
    },
    Field {
        name: "workspacePath",
        ty: NONEMPTY,
        required: false,
    },
    Field {
        name: "workspaceIdentity",
        ty: NONEMPTY,
        required: false,
    },
    Field {
        name: "remoteSessionId",
        ty: NONEMPTY,
        required: false,
    },
    Field {
        name: "clientMode",
        ty: CLIENT_MODE,
        required: false,
    },
    Field {
        name: "sessionContext",
        ty: SESSION_CONTEXT,
        required: false,
    },
];

/// `nonEmptyString = z.string().trim().min(1)`：去掉首尾空白后仍非空。
fn trimmed_non_empty(object: &Map<String, Value>) -> bool {
    object
        .values()
        .filter_map(Value::as_str)
        .all(|text| !text.trim().is_empty())
}

fn text(object: &Map<String, Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(|text| text.trim().to_owned())
}

/// zcodeBrowserExecuteParamsSchema；`command` 的完整 schema 由 Main 校验。
fn parse_execute(context: &ReverseContext) -> Option<ExecuteInput> {
    let mut object = context.params.as_object()?.clone();
    let command = object.remove("command")?;
    if !command["method"].is_string() {
        return None;
    }
    if !schema::check_object(&object, EXECUTE_PARAMS) || !trimmed_non_empty(&object) {
        return None;
    }
    let target = &context.target;
    Some(ExecuteInput {
        request_id: text(&object, "requestId"),
        browser_id: text(&object, "browserId"),
        browser_generation: object.get("browserGeneration").and_then(Value::as_u64),
        session_id: text(&object, "sessionId")?,
        turn_id: text(&object, "turnId"),
        workspace_key: text(&object, "workspaceKey").or_else(|| Some(target.key())),
        workspace_path: text(&object, "workspacePath")
            .or_else(|| Some(target.workspace_path.clone())),
        workspace_identity: text(&object, "workspaceIdentity").or_else(|| {
            target
                .workspace_identity
                .clone()
                .filter(|identity| !identity.is_empty())
        }),
        remote_session_id: text(&object, "remoteSessionId"),
        client_mode: text(&object, "clientMode").or_else(|| Some("desktop-continuous".into())),
        session_context: text(&object, "sessionContext").or_else(|| Some("live".into())),
        // CLI 的反向请求只知道会话与回合，不知道是哪条连接发起的回合：不猜窗口。
        window_id: None,
        command,
    })
}

/// sanitizeAmbientUrl：只接受 about/http/https，去掉用户名密码，长度 ≤ 4096。
pub fn sanitize_ambient_url(raw: &str) -> Option<String> {
    let mut url = url::Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "about" | "http" | "https") {
        return None;
    }
    let _ = url.set_username("");
    let _ = url.set_password(None);
    let value = url.as_str().to_owned();
    (value.encode_utf16().count() <= MAX_AMBIENT_URL_CHARS).then_some(value)
}

/// path.resolve 的词法版本（不访问文件系统）。
fn lexical_resolve(base: &Path, relative: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for component in base.join(relative).components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// materializeBrowserRecordingArtifact（本地）：复制 Main 的临时 WebM 到 workspace 内的 outputPath。
async fn materialize_recording(
    artifact: &Value,
    output_path: &str,
    input: &ExecuteInput,
) -> Result<Value, String> {
    if input.remote_session_id.is_some() {
        return Err(
            "remote Browser recording materialization is unavailable for this session".into(),
        );
    }
    let local_path = artifact["path"]
        .as_str()
        .ok_or("browser recording artifact has no path")?;
    let workspace = input
        .workspace_path
        .as_deref()
        .ok_or("recording outputPath must stay inside the workspace")?;
    let root = lexical_resolve(Path::new(workspace), "");
    let target = lexical_resolve(&root, output_path);
    if target == root || !target.starts_with(&root) {
        return Err("recording outputPath must stay inside the workspace".into());
    }
    if !target.to_string_lossy().to_lowercase().ends_with(".webm") {
        return Err("recording outputPath must end with .webm".into());
    }
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| error.to_string())?;
    }
    let staging = PathBuf::from(format!(
        "{}.zcode-recording-{}.tmp",
        target.display(),
        uuid_v4()
    ));
    let result = async {
        tokio::fs::copy(local_path, &staging).await?;
        // Windows 不能用 rename 覆盖已有文件；先删除明确的目标 WebM，再提交 staging 文件。
        match tokio::fs::remove_file(&target).await {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        tokio::fs::rename(&staging, &target).await
    }
    .await;
    let _ = tokio::fs::remove_file(&staging).await;
    result.map_err(|error| error.to_string())?;
    let mut materialized = artifact.clone();
    materialized["path"] = json!(target.display().to_string());
    Ok(materialized)
}

#[cfg(test)]
#[path = "browser_tests.rs"]
mod tests;
