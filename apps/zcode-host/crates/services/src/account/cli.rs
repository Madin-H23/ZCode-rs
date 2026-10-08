//! 账号与 CLI 的集成（packages/services/src/zcode-agent/zcodeAgentService.ts 的账号部分、
//! runtime-tools/agentProxyEnv.ts `buildAgentEndpointOriginEnv`、zcode-agent/agentTelemetryEnv.ts）。
//!
//! - Account Config 同步：每次使用 CLI 之前（使用前钩子）把当前账号 overlay 发给该进程；
//!   账号层变化时推送给已用过的进程。按进程串行，已收到同一 revision 时跳过。
//! - 反向请求：`interaction/requestProviderRuntimeHeaders`（账号鉴权）、
//!   `interaction/requestOfficialMcpAuthHeaders`（官方 MCP 身份头）。
//! - 通知：`interaction/providerRuntimeHeadersCancelled` 撤销同一请求，迟到的结果不再应答。
//! - spawn 环境：`ZCODE_BASE_URL`（含设置覆盖）与遥测身份。
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, Weak};

use futures_util::StreamExt;
use serde_json::{Value, json};
use sha2::Digest;
use zcode_host_kit::{RpcError, log};

use super::account_providers::AccountProviderSource;
use super::auth::AccountRequestAuth;
use super::endpoint::Endpoints;
use super::oauth_repo::OAuthCredentialRepo;
use super::official_mcp::{DEV_TRUSTED_ORIGINS_ENV, OfficialMcpCredentials, is_origin_trusted};
use crate::agent::extensions::{NotificationContext, ReverseContext, ReverseReply};
use crate::agent::{AgentProcess, AgentRuntime};

/// 只定向给 Agent telemetry bootstrap 的键（`SANITIZED_RUNTIME_ENV_KEYS` 中的遥测部分）。
const TELEMETRY_KEYS: [&str; 17] = [
    "OTEL_EXPORTER_OTLP_ENDPOINT",
    "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
    "OTEL_EXPORTER_OTLP_HEADERS",
    "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
    "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
    "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
    "OTEL_SERVICE_NAME",
    "OTEL_RESOURCE_ATTRIBUTES",
    "OTEL_EXPORTER_OTLP_COMPRESSION",
    "ZCODE_MODEL_TELEMETRY_ENABLED",
    "ZCODE_TELEMETRY_DEVICE_MID",
    "ZCODE_TELEMETRY_USER_ID",
    "ZCODE_TELEMETRY_USER_ID_HASH",
    "ZCODE_TELEMETRY_USER_SUBJECT_ID",
    "ZCODE_TELEMETRY_IDENTITY_STATE",
    "ZCODE_TELEMETRY_RUNTIME_SURFACE",
    "ZCODE_TELEMETRY_RUNTIME_DISTRIBUTION",
];

/// `buildAgentTelemetrySpawnEnv`：没有 OTLP 端点时为空。
pub fn telemetry_spawn_env(
    telemetry: &BTreeMap<String, String>,
    device_mid: Option<&str>,
    user_id: Option<&str>,
    runtime_surface: &str,
) -> BTreeMap<String, String> {
    if !telemetry.contains_key("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
        && !telemetry.contains_key("OTEL_EXPORTER_OTLP_ENDPOINT")
    {
        return BTreeMap::new();
    }
    let mut env = telemetry.clone();
    let device = device_mid.map(str::trim).filter(|v| !v.is_empty());
    if let Some(device) = device {
        env.insert("ZCODE_TELEMETRY_DEVICE_MID".into(), device.to_owned());
    }
    match user_id.map(str::trim).filter(|v| !v.is_empty()) {
        Some(user) => {
            env.insert(
                "ZCODE_TELEMETRY_IDENTITY_STATE".into(),
                "authenticated".into(),
            );
            let digest = sha2::Sha256::digest(user.as_bytes());
            env.insert(
                "ZCODE_TELEMETRY_USER_SUBJECT_ID".into(),
                digest.iter().map(|byte| format!("{byte:02x}")).collect(),
            );
        }
        None => {
            env.insert(
                "ZCODE_TELEMETRY_IDENTITY_STATE".into(),
                if device.is_some() {
                    "anonymous"
                } else {
                    "unknown"
                }
                .into(),
            );
        }
    }
    env.insert(
        "ZCODE_TELEMETRY_RUNTIME_SURFACE".into(),
        runtime_surface.into(),
    );
    env
}

/// Host 环境里给 Agent 的遥测配置（Main 只把 OTLP 连接配置定向下发给 Host）。
pub fn captured_telemetry_env(vars: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    TELEMETRY_KEYS
        .iter()
        .filter_map(|key| {
            vars.get(*key)
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(|value| ((*key).to_owned(), value.to_owned()))
        })
        .collect()
}

struct Tracked {
    process: Weak<AgentProcess>,
    /// 串行化同一进程的读取与交付；值是已确认收到的 revision。
    received: Arc<tokio::sync::Mutex<Option<String>>>,
}

#[derive(Clone)]
pub struct AccountConfigSync {
    source: AccountProviderSource,
    processes: Arc<Mutex<HashMap<(String, u64), Tracked>>>,
}

impl AccountConfigSync {
    pub fn new(source: AccountProviderSource) -> Self {
        Self {
            source,
            processes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn slot(&self, process: &Arc<AgentProcess>) -> Arc<tokio::sync::Mutex<Option<String>>> {
        let mut processes = self.processes.lock().unwrap_or_else(|p| p.into_inner());
        processes.retain(|_, tracked| {
            tracked
                .process
                .upgrade()
                .is_some_and(|process| !process.client.is_closed())
        });
        processes
            .entry((process.key.clone(), process.generation))
            .or_insert_with(|| Tracked {
                process: Arc::downgrade(process),
                received: Arc::new(tokio::sync::Mutex::new(None)),
            })
            .received
            .clone()
    }

    /// `syncAccountProviderConfigToClient`。
    pub async fn sync(&self, process: &Arc<AgentProcess>, reason: &str) -> Result<(), RpcError> {
        // 原生核心必须显式声明不支持；未声明时照常同步。
        if process
            .capabilities()
            .await
            .is_some_and(|capabilities| capabilities["accountProviderConfig"] == false)
        {
            return Ok(());
        }
        let slot = self.slot(process);
        let mut received = slot.lock().await;
        let snapshot = self
            .source
            .read()
            .await
            .map_err(|message| RpcError::new("Error", message))?;
        if received.as_deref() == Some(snapshot.revision.as_str()) {
            return Ok(());
        }
        let params = json!({
            "revision": snapshot.revision,
            "basedOnZCodeBuiltinRevision": snapshot.based_on_builtin_revision,
            "providers": snapshot.providers,
            "states": snapshot.states,
        });
        let result = process
            .request_value("provider/updateAccountConfig", &params)
            .await?;
        if result["receivedRevision"] != snapshot.revision.as_str() {
            return Err(RpcError::new(
                "Error",
                "Account Config 接收回执版本与交付版本不一致",
            ));
        }
        *received = Some(snapshot.revision.clone());
        log::info(format!(
            "[zcode-agent] account provider config 已交付到 ZCode agent reason={reason} providerCount={} status={}",
            result["providerCount"], result["status"]
        ));
        Ok(())
    }

    /// 任一仍在运行的已用进程（App 用量读全局会话库，任意 workspace 的 CLI 都可以）。
    pub fn any_process(&self) -> Option<Arc<AgentProcess>> {
        self.processes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .filter_map(|tracked| tracked.process.upgrade())
            .find(|process| !process.client.is_closed())
    }

    /// 账号层变化：推送给仍在运行、用过的进程（`handleAccountProviderConfigChanged`）。
    pub fn start_push(&self) {
        let sync = self.clone();
        let mut changes = Box::pin(self.source.changes().subscribe());
        tokio::spawn(async move {
            while let Some(reason) = changes.next().await {
                let processes: Vec<Arc<AgentProcess>> = sync
                    .processes
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .values()
                    .filter_map(|tracked| tracked.process.upgrade())
                    .filter(|process| !process.client.is_closed())
                    .collect();
                for process in processes {
                    if let Err(error) = sync.sync(&process, &reason).await {
                        log::warn(format!(
                            "[zcode-agent] account provider config 热同步失败 reason={reason}: {}",
                            error.message
                        ));
                    }
                }
            }
        });
    }
}

type PendingKey = (String, u64, String, String);

/// 进行中的 `requestProviderRuntimeHeaders`：键为 (workspaceKey, 进程代, sessionId, requestId)，
/// 值为本次请求的序号，取消或换新请求后旧解析的结果不再应答。
#[derive(Clone, Default)]
struct PendingHeaders(
    Arc<Mutex<HashMap<PendingKey, u64>>>,
    Arc<std::sync::atomic::AtomicU64>,
);

impl PendingHeaders {
    fn begin(&self, key: PendingKey) -> u64 {
        let id = self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key, id);
        id
    }

    fn is_current(&self, key: &PendingKey, id: u64) -> bool {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).get(key) == Some(&id)
    }

    fn finish(&self, key: &PendingKey, id: u64) {
        let mut map = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if map.get(key) == Some(&id) {
            map.remove(key);
        }
    }

    fn cancel(&self, key: &PendingKey) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(key)
            .is_some()
    }
}

/// workspace 引用的 key（`workspaceIdentity?.trim() || workspaceKey/path`）。
fn workspace_key_of(workspace: &Value) -> Option<String> {
    workspace["workspaceKey"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            workspace["workspaceIdentity"]
                .as_str()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        })
        .or_else(|| workspace["workspacePath"].as_str().map(str::to_owned))
}

fn non_empty(value: &Value) -> Option<&str> {
    value.as_str().filter(|text| !text.trim().is_empty())
}

/// 登记 CLI 集成（使用前钩子、反向请求、通知、spawn 环境）。
#[allow(clippy::too_many_arguments)]
pub fn register(
    runtime: &AgentRuntime,
    sync: AccountConfigSync,
    auth: AccountRequestAuth,
    official: OfficialMcpCredentials,
    endpoints: Endpoints,
    repo: OAuthCredentialRepo,
    telemetry: BTreeMap<String, String>,
    device_mid: Option<String>,
) {
    let extensions = runtime.extensions();
    {
        let sync = sync.clone();
        extensions.add_use_hook(Arc::new(move |process, reason| {
            let sync = sync.clone();
            Box::pin(async move { sync.sync(&process, &format!("{reason:?}")).await })
        }));
    }
    sync.start_push();

    let pending = PendingHeaders::default();
    {
        let pending = pending.clone();
        extensions.register_reverse(
            "interaction/requestProviderRuntimeHeaders",
            Arc::new(move |context: ReverseContext| {
                let (pending, auth) = (pending.clone(), auth.clone());
                Box::pin(async move { provider_runtime_headers(context, pending, auth).await })
            }),
        );
    }
    {
        let pending = pending.clone();
        extensions.register_notification(
            "interaction/providerRuntimeHeadersCancelled",
            Arc::new(move |context: NotificationContext| {
                let params = &context.params;
                let (Some(request_id), Some(session_id)) =
                    (non_empty(&params["requestId"]), non_empty(&params["sessionId"]))
                else {
                    return;
                };
                // 只撤销同一 workspace 的请求；旧进程或别的 workspace 的取消不能删新请求。
                if workspace_key_of(&params["workspace"]).as_deref()
                    != Some(context.workspace_key.as_str())
                {
                    return;
                }
                let key = (
                    context.workspace_key.clone(),
                    context.generation,
                    session_id.to_owned(),
                    request_id.to_owned(),
                );
                if pending.cancel(&key) {
                    log::info(format!(
                        "[zcode-agent] Provider runtime headers 请求已取消 requestId={request_id} sessionId={session_id} workspaceKey={}",
                        context.workspace_key
                    ));
                }
            }),
        );
    }
    {
        let endpoints = endpoints.clone();
        extensions.register_reverse(
            "interaction/requestOfficialMcpAuthHeaders",
            Arc::new(move |context: ReverseContext| {
                let (official, endpoints) = (official.clone(), endpoints.clone());
                Box::pin(async move { official_mcp_headers(context, official, endpoints).await })
            }),
        );
    }
    {
        let endpoints = endpoints.clone();
        extensions.add_env_contributor(Arc::new(move |_target| {
            let (endpoints, telemetry, repo, device_mid) = (
                endpoints.clone(),
                telemetry.clone(),
                repo.clone(),
                device_mid.clone(),
            );
            Box::pin(async move {
                let mut env = BTreeMap::new();
                // Host 解析出的权威 origin（含设置覆盖）下发给 Agent，两侧信任判定输入一致。
                if let Ok(origin) = endpoints.current_origin().await
                    && !origin.trim().is_empty()
                {
                    env.insert("ZCODE_BASE_URL".to_owned(), origin.trim().to_owned());
                }
                let configured = telemetry.contains_key("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
                    || telemetry.contains_key("OTEL_EXPORTER_OTLP_ENDPOINT");
                let user = if configured {
                    repo.active_user_profile()
                        .await
                        .ok()
                        .flatten()
                        .map(|p| p.id)
                } else {
                    None
                };
                env.extend(telemetry_spawn_env(
                    &telemetry,
                    device_mid.as_deref(),
                    user.as_deref(),
                    "desktop_local_host",
                ));
                env
            })
        }));
    }
}

async fn provider_runtime_headers(
    context: ReverseContext,
    pending: PendingHeaders,
    auth: AccountRequestAuth,
) -> ReverseReply {
    let params = &context.params;
    let valid = non_empty(&params["requestId"]).is_some()
        && non_empty(&params["sessionId"]).is_some()
        && non_empty(&params["providerId"]).is_some()
        && params["workspace"].is_object()
        && params["modelSelection"].is_object()
        && params["reason"] == "model-request";
    if !valid {
        return ReverseReply::Error {
            code: -32602,
            message: "Invalid provider runtime headers request params".to_owned(),
        };
    }
    let key = (
        context.workspace_key.clone(),
        context.generation,
        params["sessionId"].as_str().unwrap_or("").to_owned(),
        params["requestId"].as_str().unwrap_or("").to_owned(),
    );
    let provider_id = params["providerId"].as_str().unwrap_or("");
    log::info(format!(
        "[zcode-agent] 收到 ZCode provider runtime headers 请求 providerId={provider_id} modelId={} requestId={} workspaceKey={}",
        params["modelSelection"]["modelId"], key.3, key.0
    ));
    let access = &params["accountAccess"];
    if !access.is_object() {
        // 没有账号凭据可解析：直接失败，不让 CLI 等到 180 s 超时。
        return ReverseReply::Result(json!({
            "headersApplied": false,
            "errorMessage": "Provider request auth is unavailable",
        }));
    }
    let id = pending.begin(key.clone());
    let result = auth.resolve_current(provider_id, access).await;
    // 账号解析是异步 IO：取消或换代后不把迟到的材料发给已撤销的请求。
    if !pending.is_current(&key, id) {
        return ReverseReply::Hold;
    }
    pending.finish(&key, id);
    match result {
        Ok(api_key) => {
            log::info(format!(
                "[zcode-agent] ZCode provider runtime headers 已应用 providerId={provider_id} requestId={}",
                key.3
            ));
            ReverseReply::Result(
                json!({"headersApplied": true, "requestAuth": {"apiKey": api_key}}),
            )
        }
        Err(error) => {
            log::warn(format!(
                "[zcode-agent] ZCode provider runtime headers 应用失败 providerId={provider_id}: {error}"
            ));
            ReverseReply::Result(
                json!({"headersApplied": false, "errorMessage": error.to_string()}),
            )
        }
    }
}

async fn official_mcp_headers(
    context: ReverseContext,
    official: OfficialMcpCredentials,
    endpoints: Endpoints,
) -> ReverseReply {
    let params = &context.params;
    let valid = ["requestId", "pluginId", "mcpKey", "targetOrigin"]
        .iter()
        .all(|key| non_empty(&params[*key]).is_some())
        && params["workspace"].is_object();
    if !valid {
        return ReverseReply::Error {
            code: -32602,
            message: "Invalid interaction/requestOfficialMcpAuthHeaders params".to_owned(),
        };
    }
    // 读取凭据之前先做 host 侧可信校验：未命中不会读入任何凭据。
    let origin = endpoints.current_origin().await.ok();
    let (trusted, detail) = is_origin_trusted(
        params["targetOrigin"].as_str().unwrap_or(""),
        endpoints
            .vars()
            .get(DEV_TRUSTED_ORIGINS_ENV)
            .map(String::as_str),
        origin.as_deref(),
    );
    if !trusted {
        log::warn(format!(
            "[zcode-agent] 官方 MCP 身份头请求未通过 host 侧可信校验 detail={detail} pluginId={} mcpKey={}",
            params["pluginId"], params["mcpKey"]
        ));
        return ReverseReply::Result(
            json!({"ok": false, "reason": "official_mcp_origin_untrusted"}),
        );
    }
    match official.resolve_headers().await {
        Ok(headers) => {
            let mut names: Vec<String> = headers.keys().map(|name| name.to_lowercase()).collect();
            names.sort();
            log::debug(format!(
                "[zcode-agent] 官方 MCP 身份头已解析 headers={names:?}"
            ));
            ReverseReply::Result(json!({"ok": true, "headers": headers}))
        }
        Err(reason) => {
            log::info(format!(
                "[zcode-agent] 官方 MCP 身份头不可用 reason={reason} pluginId={}",
                params["pluginId"]
            ));
            ReverseReply::Result(json!({"ok": false, "reason": reason}))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_env_only_when_endpoint_configured() {
        let mut vars = BTreeMap::new();
        vars.insert("OTEL_TRACES_SAMPLER".to_owned(), "x".to_owned());
        let captured = captured_telemetry_env(&vars);
        assert!(
            telemetry_spawn_env(&captured, Some("m"), Some("u"), "desktop_local_host").is_empty()
        );
        vars.insert(
            "OTEL_EXPORTER_OTLP_ENDPOINT".to_owned(),
            " http://127.0.0.1:4318 ".to_owned(),
        );
        let captured = captured_telemetry_env(&vars);
        let env = telemetry_spawn_env(&captured, Some("m"), Some("u"), "desktop_local_host");
        assert_eq!(env["OTEL_EXPORTER_OTLP_ENDPOINT"], "http://127.0.0.1:4318");
        assert_eq!(env["ZCODE_TELEMETRY_IDENTITY_STATE"], "authenticated");
        assert_eq!(env["ZCODE_TELEMETRY_USER_SUBJECT_ID"].len(), 64);
        assert_eq!(env["ZCODE_TELEMETRY_DEVICE_MID"], "m");
        assert_eq!(env["ZCODE_TELEMETRY_RUNTIME_SURFACE"], "desktop_local_host");
        let anonymous = telemetry_spawn_env(&captured, None, None, "desktop_local_host");
        assert_eq!(anonymous["ZCODE_TELEMETRY_IDENTITY_STATE"], "unknown");
    }

    #[test]
    fn pending_headers_cancel_only_matching_request() {
        let pending = PendingHeaders::default();
        let key: PendingKey = ("/w".into(), 1, "s".into(), "r".into());
        let first = pending.begin(key.clone());
        assert!(pending.is_current(&key, first));
        assert!(pending.cancel(&key));
        assert!(!pending.is_current(&key, first));
        let second = pending.begin(key.clone());
        pending.finish(&key, first);
        assert!(pending.is_current(&key, second));
    }
}
