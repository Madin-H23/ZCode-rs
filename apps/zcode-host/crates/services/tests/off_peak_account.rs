//! 闲时任务的真实凭据链（docs/specs/rust-host-integration.md §2.1）：已登录的 Z.ai 账号（Node 加密
//! 格式的临时凭据）+ 127.0.0.1 mock 的 ZCode / Z.ai 业务后端与闲时服务端。不访问任何真实服务，
//! 不读写开发者的 ~/.zcode。
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use base64::Engine;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use zcode_host_control::{ControlSink, HostToMain, InitLocal};
use zcode_host_services::account::AccountStack;
use zcode_host_services::account::credential::{CredentialStore, credential_secret};
use zcode_host_services::off_peak::client::ServerClient;
use zcode_host_services::off_peak::credentials::{AccountDeps, CredentialSource};
use zcode_host_services::off_peak::models::request_auth;
use zcode_host_services::providers::ProviderRuntime;
use zcode_host_services::settings::SettingStore;
use zcode_host_services::{HostEnvironment, ServiceContext};

struct NullSink;

impl ControlSink for NullSink {
    fn send(&self, _message: HostToMain) {}
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

/// 一次请求：(method, path, 小写头名 → 值)。
type Recorded = (String, String, BTreeMap<String, String>);

/// mock 后端：账号业务接口按 account_cli 的形状应答，闲时接口按服务端 v2 应答。
async fn serve_backend(listener: TcpListener, log: Arc<Mutex<Vec<Recorded>>>) {
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
            let headers: BTreeMap<String, String> = lines
                .filter_map(|line| line.split_once(':'))
                .map(|(key, value)| (key.trim().to_lowercase(), value.trim().to_owned()))
                .collect();
            log.lock()
                .unwrap()
                .push((method.clone(), path.clone(), headers));
            let body = match path.as_str() {
                "/api/v1/zcode-plan/billing/balance" => json!({"code": 0, "data": {
                    "server_time": now_seconds(), "plans": [], "balances": [],
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
                    {"productId": "pro", "productName": "GLM Coding Pro", "status": "VALID", "inCurrentPeriod": true},
                ]}),
                "/api/v1/off-peak/ticket" => json!({
                    "ticket_id": "ticket-1", "state": "queued", "position": 2, "next_poll_after": 7,
                }),
                "/api/v1/off-peak/ticket/availability" => json!({"can_take_number": true}),
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
async fn logged_in_coding_plan_drives_off_peak_requests() {
    let home = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn(serve_backend(listener, requests.clone()));

    let home_text = home.path().to_string_lossy().into_owned();
    let vars = BTreeMap::from([
        ("HOME".to_owned(), home_text.clone()),
        ("USERPROFILE".to_owned(), home_text.clone()),
        ("ZCODE_DATA_BASE_DIR".to_owned(), home_text.clone()),
        ("ZCODE_DESKTOP_HOME_DIR".to_owned(), home_text.clone()),
        (
            "ZCODE_CREDENTIAL_SECRET".to_owned(),
            "off-peak-account-test".to_owned(),
        ),
        ("ZCODE_BASE_URL".to_owned(), origin.clone()),
        ("ZAI_BUSINESS_BASE_URL".to_owned(), origin.clone()),
        ("ZAI_OAUTH_ORIGIN".to_owned(), origin.clone()),
        ("BIGMODEL_API_BASE_URL".to_owned(), origin.clone()),
        ("ZCODE_APP_VERSION".to_owned(), "9.9.9".to_owned()),
        ("NO_PROXY".to_owned(), "127.0.0.1,localhost".to_owned()),
    ]);
    let environment = HostEnvironment::capture(vars.clone()).unwrap();
    let config_dir = environment.app_config_dir();
    std::fs::create_dir_all(&config_dir).unwrap();

    // 已登录的 Z.ai 账号，选中个人 Coding Plan（与 Node 相同的凭据键与加密格式）。
    let credentials = CredentialStore::new(
        &zcode_host_services::testing::spawner(),
        &config_dir,
        &credential_secret(&vars),
    );
    credentials
        .save("oauth:active_provider", "zai")
        .await
        .unwrap();
    credentials
        .save("oauth:zai:access_token", "zai-business-token")
        .await
        .unwrap();
    let zcode_jwt = jwt(now_seconds() + 3600);
    credentials.save("zcodejwttoken", &zcode_jwt).await.unwrap();
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
    let settings = SettingStore::new(&zcode_host_services::testing::spawner(), home.path());
    let providers = ProviderRuntime::new(
        &zcode_host_services::testing::spawner(),
        Some(builtin),
        config_dir.join("provider_config.json"),
    );
    let context = ServiceContext {
        environment,
        init: InitLocal::default(),
        control: Arc::new(NullSink),
    };
    let account = AccountStack::new(
        &zcode_host_services::testing::spawner(),
        &context,
        &settings,
        &providers,
    );
    let (_broadcast, broadcaster) = zcode_host_services::broadcast::service_with_sender();
    account.start(broadcaster);

    let source = CredentialSource::new(
        vars.clone(),
        Some(AccountDeps {
            credentials: account.credentials.clone(),
            auth: account.auth.clone(),
            providers: providers.clone(),
        }),
    );
    assert_eq!(
        source.support().await.unwrap(),
        json!({"supported": true, "kind": "zai-personal", "providerFamily": "zai",
               "providerId": "account:zai-individual-coding-plan"})
    );
    let resolved = source.resolve().await.unwrap();
    assert_eq!(resolved.jwt, zcode_jwt);
    assert_eq!(resolved.coding_plan_api_key, "key-abc.secret-xyz");
    assert_eq!(
        request_auth(&resolved, "ticket-9")["headers"],
        json!({
            "Authorization": format!("Bearer {zcode_jwt}"),
            "X-Coding-Plan-Api-Key": "key-abc.secret-xyz",
            "X-Off-Peak-Ticket-ID": "ticket-9",
        })
    );

    // 取号：Node offPeakServerClient 的头（来源头 + 双凭证 + request id），origin 来自 ZCODE_BASE_URL。
    let client = ServerClient::new(
        vars.clone(),
        source.clone(),
        Some(account.api.source_headers().clone()),
        None,
    )
    .unwrap();
    let ticket = client.take_ticket("offpeak-1").await.unwrap();
    assert_eq!(ticket.ticket_id, "ticket-1");
    assert_eq!(ticket.position, Some(2));
    assert_eq!(ticket.next_poll_after_ms, Some(7_000));
    assert_eq!(
        client.take_number_availability().await.unwrap(),
        json!({"canTakeNumber": true})
    );
    {
        let log = requests.lock().unwrap();
        let (method, _, headers) = log
            .iter()
            .find(|(_, path, _)| path == "/api/v1/off-peak/ticket")
            .unwrap();
        assert_eq!(method, "POST");
        assert_eq!(headers["authorization"], format!("Bearer {zcode_jwt}"));
        assert_eq!(headers["x-coding-plan-api-key"], "key-abc.secret-xyz");
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(headers["x-zcode-app-version"], "9.9.9");
        assert_eq!(headers["user-agent"], "ZCode/9.9.9");
        assert_eq!(headers["x-title"], "Z Code@electron");
        assert!(!headers["x-request-id"].is_empty());
        assert!(!headers.contains_key("bigmodel-organization"));
        let (_, _, availability) = log
            .iter()
            .find(|(_, path, _)| path == "/api/v1/off-peak/ticket/availability")
            .unwrap();
        assert!(!availability.contains_key("content-type"));
    }

    // 另一家的登录身份不能和本家的 key 拼在一起。
    credentials
        .save("oauth:active_provider", "bigmodel")
        .await
        .unwrap();
    assert_eq!(
        source.support().await.unwrap(),
        json!({"supported": false, "reason": "provider_identity_mismatch"})
    );
    credentials
        .save("oauth:active_provider", "zai")
        .await
        .unwrap();
    // JWT 缺失：support 报 jwt_missing，取号在发请求前失败。
    credentials.delete("zcodejwttoken").await.unwrap();
    assert_eq!(
        source.support().await.unwrap(),
        json!({"supported": false, "reason": "jwt_missing"})
    );
    let error = client.take_ticket("offpeak-2").await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "off-peak requires zcode login (jwt missing)"
    );
}
