//! OAuth 服务的行为测试：127.0.0.1 mock 的 ZCode / Z.ai / BigModel 后端，临时凭据目录。
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use serde_json::{Value, json};

use super::credential::CredentialStore;
use super::endpoint::Endpoints;
use super::net::testing::{MockServer, Recorded, serve};
use super::net::{ApiClient, HeaderList, NetworkOptions};
use super::oauth::OAuthService;
use super::oauth_adapters::now_ms;
use super::oauth_repo::{
    ACTIVE_PROVIDER_KEY, BIGMODEL, OAuthCredentialRepo, ZAI, ZCODE_JWT_TOKEN_KEY,
    access_token_key, user_info_key,
};

pub fn jwt(exp_seconds: i64) -> String {
    let encode = |value: Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
    };
    format!(
        "{}.{}.sig",
        encode(json!({"alg": "none"})),
        encode(json!({"exp": exp_seconds}))
    )
}

pub struct Harness {
    pub server: MockServer,
    pub service: OAuthService,
    pub credentials: CredentialStore,
    pub clock: Arc<AtomicI64>,
    pub logouts: Arc<Mutex<Vec<(String, Option<String>)>>>,
    _dir: tempfile::TempDir,
}

pub fn endpoints_for(origin: &str) -> Endpoints {
    Endpoints::from_env(BTreeMap::from([
        ("ZCODE_BASE_URL".to_owned(), origin.to_owned()),
        ("ZAI_BUSINESS_BASE_URL".to_owned(), origin.to_owned()),
        ("ZAI_OAUTH_ORIGIN".to_owned(), origin.to_owned()),
        ("BIGMODEL_API_BASE_URL".to_owned(), origin.to_owned()),
        ("ZCODE_APP_VERSION".to_owned(), "1.0.0".to_owned()),
    ]))
}

pub async fn harness(route: super::net::testing::Route) -> Harness {
    let server = serve(route).await;
    let dir = tempfile::tempdir().unwrap();
    let credentials = CredentialStore::new(dir.path(), "secret");
    let api = ApiClient::with_network(
        endpoints_for(&server.origin),
        dir.path().to_path_buf(),
        NetworkOptions::default(),
    );
    let clock = Arc::new(AtomicI64::new(now_ms()));
    let tick = clock.clone();
    let service = OAuthService::with_clock(
        OAuthCredentialRepo::new(credentials.clone()),
        api,
        Arc::new(move || tick.load(Ordering::SeqCst)),
    );
    let logouts = Arc::new(Mutex::new(Vec::new()));
    let recorded = logouts.clone();
    service.set_logout_hook(Arc::new(move |provider, identity| {
        recorded.lock().unwrap().push((provider, identity));
        Box::pin(async { Ok(()) })
    }));
    Harness {
        server,
        service,
        credentials,
        clock,
        logouts,
        _dir: dir,
    }
}

fn zai_backend(poll_ready_after: usize) -> super::net::testing::Route {
    let polls = Arc::new(AtomicUsize::new(0));
    Arc::new(move |request: &Recorded| {
        let path = request.target.split('?').next().unwrap_or("");
        match path {
            "/api/v1/oauth/cli/init" => {
                let body: Value = serde_json::from_str(&request.body).unwrap();
                let provider = body["provider"].as_str().unwrap().to_owned();
                (
                    200,
                    json!({"code": 0, "data": {
                        "flow_id": "flow 1",
                        "authorize_url": "https://auth.example/authorize?state=st1&redirect_uri=old",
                        "expires_at": now_ms() / 1000 + 600,
                        "poll_interval_sec": 1,
                        "provider": provider,
                    }})
                    .to_string(),
                )
            }
            "/api/v1/oauth/cli/poll/flow%201" => {
                let count = polls.fetch_add(1, Ordering::SeqCst);
                if count < poll_ready_after {
                    return (200, json!({"code": 0, "data": {"status": "pending"}}).to_string());
                }
                (
                    200,
                    json!({"code": 0, "data": {
                        "status": "ready",
                        "token": jwt(now_ms() / 1000 + 3600),
                        "user": {"user_id": "u1", "name": "Alice", "email": "a@example.com"},
                        "zai": {"access_token": "zai-oauth"},
                        "bigmodel": {"access_token": "bm-access", "refresh_token": "bm-refresh"},
                    }})
                    .to_string(),
                )
            }
            "/api/auth/z/login" => (
                200,
                json!({"code": 0, "data": {"access_token": "zai-business", "expires_in": 3600}})
                    .to_string(),
            ),
            "/api/v1/oauth/token" => (
                200,
                json!({"code": 0, "data": {
                    "token": jwt(now_ms() / 1000 + 3600),
                    "bigmodel": {"access_token": "bm-callback", "refresh_token": "r"},
                }})
                .to_string(),
            ),
            "/api/biz/customer/getCustomerInfo" => (
                200,
                json!({"data": {"customerNumber": "c-9", "customerName": " ", "nickName": "Nick"}})
                    .to_string(),
            ),
            _ => (404, "{}".to_owned()),
        }
    })
}

#[tokio::test]
async fn polling_login_persists_session_and_restores_it() {
    let h = harness(zai_backend(1)).await;
    let started = h.service.start_oauth_with_polling(ZAI).await.unwrap();
    assert_eq!(started["state"], "st1");
    let authorize = started["authorizeUrl"].as_str().unwrap();
    assert!(authorize.starts_with("https://auth.example/authorize?state=st1&redirect_uri="));
    assert!(authorize.contains("redirect_uri=http%3A%2F%2F127.0.0.1"));
    let init = &h.server.requests()[0];
    assert_eq!(init.method, "POST");
    assert_eq!(init.header("authorization").map(str::len), Some(7 + 64));

    // 第一次轮询仍是 pending；间隔未到时不发请求。
    assert_eq!(h.service.poll_pending_oauth().await.unwrap(), Value::Null);
    assert_eq!(h.service.poll_pending_oauth().await.unwrap(), Value::Null);
    assert_eq!(h.server.requests().len(), 2);
    h.clock.fetch_add(1_000, Ordering::SeqCst);
    let result = h.service.poll_pending_oauth().await.unwrap();
    assert_eq!(
        result,
        json!({"kind": "session", "provider": "zai", "userInfo": {"id": "u1", "username": "Alice", "displayName": "Alice"}})
    );
    assert_eq!(
        h.credentials.load(&access_token_key(ZAI)).await.unwrap().as_deref(),
        Some("zai-business")
    );
    assert_eq!(
        h.credentials.load(ACTIVE_PROVIDER_KEY).await.unwrap().as_deref(),
        Some("zai")
    );
    let stored_user: Value =
        serde_json::from_str(&h.credentials.load(&user_info_key(ZAI)).await.unwrap().unwrap())
            .unwrap();
    assert_eq!(stored_user["user_id"], "u1");
    let restored = h.service.restore_cached_session_state().await.unwrap();
    assert_eq!(restored["status"], "authenticated");
    assert_eq!(restored["userInfo"]["displayName"], "Alice");

    // 回调迟到：去重，不重复落盘，归因仍保存。
    let duplicate = h
        .service
        .handle_callback("zcode://oauth/callback?state=st1&code=c&utm_source=web")
        .await
        .unwrap();
    assert_eq!(duplicate, json!({"kind": "duplicate", "provider": "zai"}));
    assert_eq!(
        h.service.repo().login_attribution().await.unwrap().unwrap()["utm_source"],
        "web"
    );
    assert!(h.service.handle_callback("zcode://oauth/callback?state=other&code=c").await.is_err());
}

#[tokio::test]
async fn switching_provider_clears_the_other_identity_and_logout_notifies() {
    let h = harness(zai_backend(0)).await;
    h.service.start_oauth_with_polling(ZAI).await.unwrap();
    h.service.poll_pending_oauth().await.unwrap();
    h.service.start_oauth_with_polling(BIGMODEL).await.unwrap();
    let result = h.service.poll_pending_oauth().await.unwrap();
    assert_eq!(result["provider"], "bigmodel");
    assert_eq!(h.credentials.load(&access_token_key(ZAI)).await.unwrap(), None);
    assert_eq!(
        h.credentials.load(&access_token_key(BIGMODEL)).await.unwrap().as_deref(),
        Some("bm-access")
    );
    let stored: Value = serde_json::from_str(
        &h.credentials.load(&user_info_key(BIGMODEL)).await.unwrap().unwrap(),
    )
    .unwrap();
    assert_eq!(stored["rawProfile"]["zcodeProfileSchemaVersion"], 2);

    // 非当前 provider 的 logout 不动登录事实。
    h.service.logout(Some(ZAI.into())).await.unwrap();
    assert!(h.logouts.lock().unwrap().is_empty());
    h.service.logout(None).await.unwrap();
    assert_eq!(
        h.logouts.lock().unwrap().clone(),
        vec![("bigmodel".to_owned(), Some("u1".to_owned()))]
    );
    assert_eq!(h.credentials.load(ACTIVE_PROVIDER_KEY).await.unwrap(), None);
    assert_eq!(h.credentials.load(ZCODE_JWT_TOKEN_KEY).await.unwrap(), None);
    assert_eq!(
        h.service.restore_cached_session_state().await.unwrap(),
        json!({"status": "signed-out"})
    );
}

#[tokio::test]
async fn deep_link_callback_exchanges_code_and_cancel_discards_late_results() {
    let h = harness(zai_backend(0)).await;
    let started = h.service.start_oauth(BIGMODEL).await.unwrap();
    let state = started["state"].as_str().unwrap().to_owned();
    assert!(
        started["authorizeUrl"]
            .as_str()
            .unwrap()
            .contains(&format!("appId=zcode&state={state}"))
    );
    // 纯归因回调推进 phase，不清 state；同一 state 的第二次纯归因被拒。
    let attribution = h
        .service
        .handle_callback(&format!("zcode://oauth/callback?state={state}&channel_id=c1"))
        .await
        .unwrap();
    assert_eq!(attribution["kind"], "attribution");
    assert!(
        h.service
            .handle_callback(&format!("zcode://oauth/callback?state={state}&channel_id=c1"))
            .await
            .is_err()
    );
    let session = h
        .service
        .handle_callback(&format!("zcode://oauth/callback?state={state}&authCode=abc"))
        .await
        .unwrap();
    assert_eq!(session["kind"], "session");
    assert_eq!(session["userInfo"]["username"], "Nick");
    assert_eq!(session["userInfo"]["id"], "c-9");
    let token_request = h
        .server
        .requests()
        .into_iter()
        .find(|r| r.target == "/api/v1/oauth/token")
        .unwrap();
    let body: Value = serde_json::from_str(&token_request.body).unwrap();
    assert_eq!(body["provider"], "bigmodel");
    assert_eq!(body["code"], "abc");
    // 客户信息接口直接传 token，不带 Bearer。
    let customer = h
        .server
        .requests()
        .into_iter()
        .find(|r| r.target == "/api/biz/customer/getCustomerInfo")
        .unwrap();
    assert_eq!(customer.header("authorization"), Some("bm-callback"));

    // 取消后，迟到的 polling 结果直接丢弃。
    h.service.start_oauth_with_polling(ZAI).await.unwrap();
    h.service.cancel_pending(None).await;
    assert_eq!(h.service.poll_pending_oauth().await.unwrap(), Value::Null);
    assert_eq!(
        h.credentials.load(ACTIVE_PROVIDER_KEY).await.unwrap().as_deref(),
        Some("bigmodel")
    );
}

#[tokio::test]
async fn expired_jwt_requires_reauthentication_and_401_logout_is_conditional() {
    let h = harness(zai_backend(0)).await;
    h.service.start_oauth_with_polling(ZAI).await.unwrap();
    h.service.poll_pending_oauth().await.unwrap();
    let current_jwt = h.credentials.load(ZCODE_JWT_TOKEN_KEY).await.unwrap().unwrap();

    // 别的 token 的 401 不退出。
    let mut headers = HeaderList::default();
    headers.set("Authorization", "Bearer stale");
    assert!(
        !h.service
            .logout_if_current_credential_request(format!("{}/x", h.server.origin), headers)
            .await
            .unwrap()
    );
    // userinfo 接口带当前业务 token 的 401 也算当前凭据。
    let mut userinfo = HeaderList::default();
    userinfo.set("Authorization", "Bearer zai-business");
    assert!(
        h.service
            .is_current_credential_request(
                &format!("{}/api/oauth/userinfo", h.server.origin),
                &userinfo
            )
            .await
    );

    h.credentials
        .save(ZCODE_JWT_TOKEN_KEY, &jwt(now_ms() / 1000 - 10))
        .await
        .unwrap();
    let restored = h.service.restore_cached_session_state().await.unwrap();
    assert_eq!(
        restored,
        json!({"status": "reauthentication-required", "reason": "jwt-expired"})
    );
    assert_eq!(h.credentials.load(ACTIVE_PROVIDER_KEY).await.unwrap(), None);
    assert_eq!(h.logouts.lock().unwrap().len(), 1);
    assert_ne!(current_jwt, "");
}

#[tokio::test]
async fn terminal_poll_errors_end_the_flow_and_refresh_token_is_unsupported() {
    let h = harness(Arc::new(|request: &Recorded| {
        if request.target.starts_with("/api/v1/oauth/cli/init") {
            (
                200,
                json!({"code": 0, "data": {
                    "flow_id": "f", "authorize_url": "https://a.example/?state=s",
                    "expires_at": now_ms() / 1000 + 600, "poll_interval_sec": 1,
                }})
                .to_string(),
            )
        } else {
            (403, json!({"msg": "denied"}).to_string())
        }
    }))
    .await;
    h.service.start_oauth_with_polling(ZAI).await.unwrap();
    let failure = h.service.poll_pending_oauth().await.unwrap_err();
    assert_eq!(failure.message, "denied");
    assert_eq!(h.service.poll_pending_oauth().await.unwrap(), Value::Null);
    assert_eq!(
        h.service.refresh_token(None).await.unwrap_err().message,
        "当前没有可用的登录 provider"
    );
    assert_eq!(
        h.service
            .refresh_token(Some(ZAI.into()))
            .await
            .unwrap_err()
            .message,
        "Z.ai OAuth 暂未提供 refresh token 交换接口，请重新登录"
    );
    assert_eq!(
        h.service.providers(),
        vec![
            json!({"id": "bigmodel", "displayName": "BigModel", "enabled": true, "order": 0}),
            json!({"id": "zai", "displayName": "Z.ai", "enabled": true, "order": 1}),
        ]
    );
}
