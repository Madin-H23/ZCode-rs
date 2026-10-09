//! 账号栈的集成测试：已登录的 Z.ai 账号（临时凭据）、127.0.0.1 mock 的 ZCode / Z.ai 业务后端、
//! 真实的 Rust CLI。验证账号 provider 进入模型选择、Account Config 同步到 CLI、
//! 反向请求按账号鉴权应答。不访问任何真实服务，不读写开发者的 ~/.zcode。
#![expect(
    clippy::disallowed_methods,
    reason = "集成测试从进程环境取被测二进制与 PATH"
)]
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use zcode_host_control::{ControlSink, HostToMain, InitLocal};
use zcode_host_services::account::AccountStack;
use zcode_host_services::account::credential::{CredentialStore, credential_secret};
use zcode_host_services::agent::extensions::{ReverseContext, ReverseReply, UseReason};
use zcode_host_services::agent::{
    AgentRuntime, AgentRuntimeConfig, RuntimePolicy, WorkspaceTarget,
};
use zcode_host_services::providers::ProviderRuntime;
use zcode_host_services::settings::SettingStore;
use zcode_host_services::{HostEnvironment, ServiceContext};

struct NullSink;

impl ControlSink for NullSink {
    fn send(&self, _message: HostToMain) {}
}

/// runtime 的选择：缺省嵌入式；`ZCODE_TEST_CLI_RUST_BINARY` 指向可执行文件时走子进程链路。
fn runtime_vars() -> BTreeMap<String, String> {
    match std::env::var_os("ZCODE_TEST_CLI_RUST_BINARY").map(PathBuf::from) {
        Some(cli) => {
            assert!(
                cli.is_file(),
                "ZCODE_TEST_CLI_RUST_BINARY is not a file: {}",
                cli.display()
            );
            BTreeMap::from([
                (
                    "ZCODE_AGENT_SERVER_COMMAND".to_owned(),
                    cli.to_string_lossy().into_owned(),
                ),
                (
                    "ZCODE_AGENT_SERVER_RUNTIME".to_owned(),
                    "zcode-cli-rust".to_owned(),
                ),
            ])
        }
        None => BTreeMap::from([(
            "ZCODE_AGENT_SERVER_RUNTIME".to_owned(),
            "embedded".to_owned(),
        )]),
    }
}

fn jwt(exp: i64) -> String {
    let encode =
        |value: Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string());
    format!(
        "{}.{}.sig",
        encode(json!({"alg": "none"})),
        encode(json!({"exp": exp}))
    )
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// mock 业务后端：记录 (method, path, authorization)，按路径应答。
async fn serve_backend(listener: TcpListener, log: Arc<Mutex<Vec<(String, String, String)>>>) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let log = log.clone();
        tokio::spawn(async move {
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 8192];
            let head_end = loop {
                let Ok(read) = stream.read(&mut chunk).await else {
                    return;
                };
                if read == 0 {
                    return;
                }
                buffer.extend_from_slice(&chunk[..read]);
                if let Some(index) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
            let mut lines = head.split("\r\n");
            let mut request_line = lines.next().unwrap_or("").split(' ');
            let method = request_line.next().unwrap_or("").to_owned();
            let target = request_line.next().unwrap_or("").to_owned();
            let path = target.split('?').next().unwrap_or("").to_owned();
            let authorization = lines
                .filter_map(|line| line.split_once(':'))
                .find(|(key, _)| key.eq_ignore_ascii_case("authorization"))
                .map(|(_, value)| value.trim().to_owned())
                .unwrap_or_default();
            log.lock()
                .unwrap()
                .push((method.clone(), path.clone(), authorization));
            let body = match path.as_str() {
                "/api/v1/zcode-plan/billing/balance" => json!({"code": 0, "data": {
                    "server_time": now_seconds(),
                    "plans": [{"user_plan_id": "up1", "plan_id": "zcode-v3-start-plan", "status": "active", "ends_at": now_seconds() + 86400}],
                    "balances": [{"user_plan_id": "up1", "plan_id": "zcode-v3-start-plan", "capabilities": ["model:glm-5.3-flash"], "total_units": 100, "used_units": 10, "remaining_units": 90}],
                }}),
                "/api/biz/customer/getCustomerInfo" => {
                    json!({"code": 200, "data": {"organizations": [
                        {"organizationId": "org-1", "organizationName": "默认机构", "projects": [{"projectId": "proj-1", "projectName": "默认项目"}]},
                    ]}})
                }
                "/api/biz/v1/organization/org-1/projects/proj-1/api_keys" => {
                    json!({"code": 200, "data": [{"name": "zcode-api-key", "apiKey": "key-abc"}]})
                }
                "/api/biz/v1/organization/org-1/projects/proj-1/api_keys/copy/key-abc" => {
                    json!({"code": 200, "data": {"secretKey": "secret-xyz"}})
                }
                "/api/biz/subscription/list" => json!({"code": 200, "data": [
                    {"productId": "pro", "productName": "GLM Coding Pro", "status": "VALID", "inCurrentPeriod": true, "billingCycle": "monthly"},
                ]}),
                "/api/monitor/usage/quota/limit" => {
                    json!({"code": 200, "data": {"level": "pro", "limits": [
                        {"type": "TIME_LIMIT", "remaining": 7, "percentage": 30, "nextResetTime": 123},
                    ]}})
                }
                "/api/monitor/credit-usage/activity" => json!({"code": 200, "data": {
                    "summary": {"totalTokens": 5}, "series": [{"date": "2026-01-01", "totalTokens": 5}],
                }}),
                "/api/monitor/credit-usage/usage-detail" => json!({"code": 200, "data": {
                    "modelUsage": {"xTime": [], "modelDataList": [{"modelName": "GLM-5", "totalTokens": 5}]},
                }}),
                "/api/monitor/usage/model-performance-day" => {
                    json!({"code": 0, "data": {"x_time": ["d1"], "liteDecodeSpeed": [42]}})
                }
                _ => json!({"code": 404}),
            };
            let body = body.to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        });
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn logged_in_account_reaches_model_selection_and_cli() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn(serve_backend(listener, requests.clone()));

    let home_text = home.path().to_string_lossy().into_owned();
    let mut vars = BTreeMap::from([
        ("HOME".to_owned(), home_text.clone()),
        ("USERPROFILE".to_owned(), home_text.clone()),
        ("ZCODE_DATA_BASE_DIR".to_owned(), home_text.clone()),
        ("ZCODE_DESKTOP_HOME_DIR".to_owned(), home_text.clone()),
        (
            "ZCODE_CREDENTIAL_SECRET".to_owned(),
            "account-cli-test".to_owned(),
        ),
        ("ZCODE_BASE_URL".to_owned(), origin.clone()),
        ("ZAI_BUSINESS_BASE_URL".to_owned(), origin.clone()),
        ("ZAI_OAUTH_ORIGIN".to_owned(), origin.clone()),
        ("BIGMODEL_API_BASE_URL".to_owned(), origin.clone()),
        ("ZCODE_APP_VERSION".to_owned(), "9.9.9".to_owned()),
        ("PATH".to_owned(), std::env::var("PATH").unwrap_or_default()),
        ("NO_PROXY".to_owned(), "127.0.0.1,localhost".to_owned()),
    ]);
    let environment = HostEnvironment::capture(vars.clone()).unwrap();
    let config_dir = environment.app_config_dir();
    std::fs::create_dir_all(&config_dir).unwrap();

    // 已登录的 Z.ai 账号：与 Node 相同的凭据键与加密格式。
    let credentials = CredentialStore::new(&config_dir, &credential_secret(&vars));
    credentials
        .save("oauth:active_provider", "zai")
        .await
        .unwrap();
    credentials
        .save("oauth:zai:access_token", "zai-business-token")
        .await
        .unwrap();
    let start_jwt = jwt(now_seconds() + 3600);
    credentials.save("zcodejwttoken", &start_jwt).await.unwrap();
    credentials
        .save(
            "oauth:zai:user_info",
            &json!({"user_id": "user-1", "name": "Alice"}).to_string(),
        )
        .await
        .unwrap();
    std::fs::write(
        config_dir.join("setting.json"),
        json!({
            "providerFamilyDomain": "zai",
            "providerFamilyDomainMigrated": true,
            "providerFamilyConnectionSelections": {"zai": {"kind": "individual-coding-plan"}},
        })
        .to_string(),
    )
    .unwrap();

    let builtin = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../config/provider/zcode-builtin.json")
        .canonicalize()
        .unwrap();
    let settings = SettingStore::new(home.path());
    let providers = ProviderRuntime::new(Some(builtin), config_dir.join("provider_config.json"));
    let context = ServiceContext {
        environment,
        init: InitLocal::default(),
        control: Arc::new(NullSink),
    };
    let account = AccountStack::new(&context, &settings, &providers);
    vars.extend(runtime_vars());
    let runtime = AgentRuntime::new(
        AgentRuntimeConfig {
            base_env: vars.clone(),
            command_vars: vars.clone(),
            search_roots: Vec::new(),
            fallback_cwd: None,
        },
        settings.clone(),
        providers.clone(),
    );
    account.attach_agent(&runtime, &context);
    let (_broadcast, broadcaster) = zcode_host_services::broadcast::service_with_sender();
    account.start(broadcaster);

    // 模型选择：个人 Coding Plan 与 Start Plan（只含余额白名单里的模型）。
    let (revision, resolved) = providers.snapshot().await;
    let view = resolved.model_selection_view(revision, None);
    let ids: Vec<&str> = view["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|provider| provider["providerId"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "account:zai-individual-coding-plan",
            "account:zai-start-plan"
        ],
        "{view}"
    );
    let start = view["providers"][1]["models"].as_array().unwrap();
    assert_eq!(start.len(), 1);
    assert_eq!(start[0]["modelId"], "GLM-5.3-Flash");
    let settings_view = resolved.provider_settings_view(revision);
    let state_of = |id: &str| {
        settings_view["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|provider| provider["providerId"] == id)
            .map(|provider| provider["accountState"].clone())
            .unwrap()
    };
    let individual = state_of("account:zai-individual-coding-plan");
    assert_eq!(individual["availability"], "available");
    assert_eq!(individual["entitled"], true);
    assert_eq!(individual["current"], true);
    assert_eq!(individual["connectionKey"].as_str().unwrap().len(), 64);
    assert_eq!(
        state_of("account:zai-team-coding-plan")["availability"],
        "unknown"
    );
    assert_eq!(
        state_of("account:bigmodel-start-plan")["unavailableReason"],
        "not-connected"
    );
    // 个人 Coding Plan key 已按账号身份缓存进凭据库。
    assert_eq!(
        credentials
            .load("account-provider:coding-plan:account:zai-individual-coding-plan:account:user-1:api-key")
            .await
            .unwrap()
            .as_deref(),
        Some("key-abc.secret-xyz")
    );
    {
        let log = requests.lock().unwrap();
        let balance = log
            .iter()
            .find(|(_, path, _)| path == "/api/v1/zcode-plan/billing/balance")
            .unwrap();
        assert_eq!(balance.2, format!("Bearer {start_jwt}"));
        let subscription = log
            .iter()
            .find(|(_, path, _)| path == "/api/biz/subscription/list")
            .unwrap();
        assert_eq!(subscription.2, "key-abc.secret-xyz");
    }

    // 额度与套餐快照（Composer 工具栏）：个人 Coding Plan 与 Start Plan。
    let usage = zcode_host_services::account::usage::UsageStats::new(
        account.api.clone(),
        account.auth.clone(),
        account.credentials.clone(),
        account.billing.clone(),
        account.official_mcp.clone(),
        Arc::new(|_| Box::pin(async { Ok(Value::Null) })),
    );
    let individual = usage
        .entitlement_snapshot(&json!({
            "preferredProviderId": "account:zai-individual-coding-plan",
            "accountAccess": {"type": "zhipu-account", "accountType": "zai", "mode": "individual-coding-plan", "entitled": true},
        }))
        .await
        .unwrap();
    assert_eq!(
        individual["subscription"]["details"][0]["productName"],
        "GLM Coding Pro"
    );
    assert_eq!(individual["remaining"]["count"], 7);
    assert_eq!(individual["quota"]["level"], "pro");
    assert_eq!(
        individual["context"],
        json!({"scope": "personal", "productId": "pro", "displayName": "GLM Coding Pro"})
    );
    assert!(individual.get("unavailableReason").is_none());
    let start = usage
        .entitlement_snapshot(&json!({
            "preferredProviderId": "account:zai-start-plan",
            "accountAccess": {"type": "zhipu-account", "accountType": "zai", "mode": "start-plan", "entitled": true},
        }))
        .await
        .unwrap();
    assert_eq!(start["provider"]["name"], "Z.ai - Coding Plan");
    assert_eq!(start["quota"]["level"], "Start");
    assert_eq!(
        start["subscription"]["details"][0]["productId"],
        "zcode-v3-start-plan"
    );

    // Coding Plan 用量面板：quota + credit-usage + 模型性能，按个人套餐 key 鉴权。
    let panel = usage
        .coding_plan_usage(&json!({
            "range": "7d", "timeZone": "UTC",
            "preferredProviderId": "account:zai-individual-coding-plan",
            "accountAccess": {"type": "zhipu-account", "accountType": "zai", "mode": "individual-coding-plan", "entitled": true},
        }))
        .await
        .unwrap();
    assert_eq!(panel["quota"]["level"], "pro");
    assert_eq!(panel["modelUsage"]["totalTokensUsage"], 5);
    assert_eq!(panel["modelUsage"]["xTime"].as_array().unwrap().len(), 7);
    assert_eq!(panel["activity"]["summary"]["favoriteModelName"], "GLM-5");
    assert_eq!(panel["health"]["liteDecodeSpeed"], json!([42]));
    assert_eq!(
        panel["sourceProvider"]["id"],
        "account:zai-individual-coding-plan"
    );
    {
        let log = requests.lock().unwrap();
        let detail = log
            .iter()
            .find(|(_, path, _)| path == "/api/monitor/credit-usage/usage-detail")
            .unwrap();
        assert_eq!(detail.2, "key-abc.secret-xyz");
    }

    // 反向请求：按模型固定的 Account Access 应答，不需要 UI。
    let handler = runtime
        .extensions()
        .reverse("interaction/requestProviderRuntimeHeaders")
        .unwrap();
    let target = WorkspaceTarget::local(workspace.path().to_string_lossy().as_ref());
    let request = |provider: &str, mode: &str| ReverseContext {
        target: target.clone(),
        workspace_key: target.key(),
        generation: 1,
        method: "interaction/requestProviderRuntimeHeaders".into(),
        params: json!({
            "requestId": format!("r-{mode}"), "sessionId": "s1", "reason": "model-request",
            "workspace": {"workspaceKey": target.key(), "workspacePath": target.workspace_path},
            "modelSelection": {"providerId": provider, "modelId": "GLM-5.3-Flash", "options": {"reasoningLevel": "enabled"}},
            "providerId": provider,
            "accountAccess": {"type": "zhipu-account", "accountType": "zai", "mode": mode, "entitled": true},
        }),
    };
    assert_eq!(
        handler(request(
            "account:zai-individual-coding-plan",
            "individual-coding-plan"
        ))
        .await,
        ReverseReply::Result(
            json!({"headersApplied": true, "requestAuth": {"apiKey": "key-abc.secret-xyz"}})
        )
    );
    assert_eq!(
        handler(request("account:zai-start-plan", "start-plan")).await,
        ReverseReply::Result(json!({"headersApplied": true, "requestAuth": {"apiKey": start_jwt}}))
    );
    let team = handler(request("account:zai-team-coding-plan", "team-coding-plan")).await;
    assert_eq!(
        team,
        ReverseReply::Result(json!({
            "headersApplied": false,
            "errorMessage": "Account request credential is unavailable: account:zai-team-coding-plan",
        }))
    );

    // 官方 MCP 身份头：先做 origin 校验，再用当前个人套餐的 MaaS JWT。
    let official = runtime
        .extensions()
        .reverse("interaction/requestOfficialMcpAuthHeaders")
        .unwrap();
    let official_request = |origin: &str| ReverseContext {
        target: target.clone(),
        workspace_key: target.key(),
        generation: 1,
        method: "interaction/requestOfficialMcpAuthHeaders".into(),
        params: json!({"requestId": "m1", "pluginId": "zcode", "mcpKey": "search",
            "targetOrigin": origin, "workspace": {"workspaceKey": target.key(), "workspacePath": target.workspace_path}}),
    };
    assert_eq!(
        official(official_request("https://evil.example")).await,
        ReverseReply::Result(json!({"ok": false, "reason": "official_mcp_origin_untrusted"}))
    );

    // Account Config 同步到真实 CLI：使用前钩子发送 provider/updateAccountConfig，CLI 回执 revision。
    let process = runtime
        .get_client(&target, RuntimePolicy::StartIfNeeded)
        .await
        .unwrap();
    runtime
        .before_use(&process, UseReason::CreateSession)
        .await
        .unwrap();
    // 同一 revision 不重复交付。
    runtime
        .before_use(&process, UseReason::ConversationSubscribe)
        .await
        .unwrap();
    runtime.dispose_all().await;
}
