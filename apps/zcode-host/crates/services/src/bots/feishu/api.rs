//! 飞书 / Lark 开放平台 HTTP 调用（providers/feishuProvider.ts、feishuAppRegistration.ts）。
//!
//! 所有基址可由 Host 环境变量覆盖，测试与 E2E 只指向 127.0.0.1 上的 mock；生产使用官方域名。
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::bots::now_ms;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(10);
const REGISTRATION_PATH: &str = "/oauth/v1/app/registration";
const REGISTRATION_SOURCE: &str = "node-sdk/zcode";

/// 各域名的基址（不带末尾 `/`）。
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub feishu_open: String,
    pub lark_open: String,
    pub feishu_accounts: String,
    pub lark_accounts: String,
}

impl Endpoints {
    /// 官方域名；`ZCODE_FEISHU_OPEN_BASE_URL` 等变量可覆盖（只供测试与 E2E 指向本机 mock）。
    pub fn from_vars(vars: &BTreeMap<String, String>) -> Self {
        let pick = |key: &str, default: &str| {
            vars.get(key)
                .map(|value| value.trim().trim_end_matches('/').to_owned())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| default.to_owned())
        };
        Self {
            feishu_open: pick("ZCODE_FEISHU_OPEN_BASE_URL", "https://open.feishu.cn"),
            lark_open: pick("ZCODE_LARK_OPEN_BASE_URL", "https://open.larksuite.com"),
            feishu_accounts: pick(
                "ZCODE_FEISHU_ACCOUNTS_BASE_URL",
                "https://accounts.feishu.cn",
            ),
            lark_accounts: pick(
                "ZCODE_LARK_ACCOUNTS_BASE_URL",
                "https://accounts.larksuite.com",
            ),
        }
    }

    pub fn open(&self, provider: &str) -> &str {
        if provider == "lark" {
            &self.lark_open
        } else {
            &self.feishu_open
        }
    }

    fn accounts(&self, domain: &str) -> &str {
        if domain == "lark" {
            &self.lark_accounts
        } else {
            &self.feishu_accounts
        }
    }
}

/// `fetchBotProviderJson` 的结果。
struct JsonResponse {
    ok: bool,
    status: u16,
    payload: Value,
    log_id: Option<String>,
}

pub struct FeishuApi {
    endpoints: Endpoints,
    http: reqwest::Client,
    /// `accessTokenCache`：`<domain>:<appId>:<credentialRef>` → (token, 过期时刻)。
    tokens: Mutex<HashMap<String, (String, i64)>>,
}

/// 发送卡片 / 取 token 所需的 Bot 字段。
pub struct AppCredentials<'a> {
    pub provider: &'a str,
    pub app_id: &'a str,
    pub credential_ref: &'a str,
    pub app_secret: &'a str,
}

impl FeishuApi {
    pub fn new(endpoints: Endpoints) -> Result<Self> {
        Ok(Self {
            endpoints,
            http: crate::support::http::client(REQUEST_TIMEOUT)?,
            tokens: Mutex::new(HashMap::new()),
        })
    }

    pub fn endpoints(&self) -> &Endpoints {
        &self.endpoints
    }

    async fn json(&self, request: reqwest::RequestBuilder) -> Result<JsonResponse> {
        let response = request.timeout(REQUEST_TIMEOUT).send().await?;
        let status = response.status();
        let log_id = response
            .headers()
            .get("x-tt-logid")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let text = response.text().await?;
        let payload = if text.is_empty() {
            Value::Null
        } else {
            match serde_json::from_str(&text) {
                Ok(value) => value,
                Err(error) if status.is_success() => return Err(error.into()),
                Err(_) => Value::Null,
            }
        };
        Ok(JsonResponse {
            ok: status.is_success(),
            status: status.as_u16(),
            payload,
            log_id,
        })
    }

    /// `readTenantAccessToken`：缓存 90 分钟，剩余不足 60 秒时重新获取。
    pub async fn tenant_token(&self, app: &AppCredentials<'_>) -> Result<String> {
        let domain = if app.provider == "lark" {
            "lark"
        } else {
            "feishu"
        };
        let cache_key = format!("{domain}:{}:{}", app.app_id, app.credential_ref);
        if let Some((token, expires_at)) = self
            .tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&cache_key)
            .cloned()
            && expires_at > now_ms() + 60_000
        {
            return Ok(token);
        }
        let url = format!(
            "{}/open-apis/auth/v3/tenant_access_token/internal",
            self.endpoints.open(app.provider)
        );
        let response = self
            .json(
                self.http
                    .post(url)
                    .header("content-type", "application/json")
                    .body(json!({"app_id": app.app_id, "app_secret": app.app_secret}).to_string()),
            )
            .await?;
        if !response.ok {
            bail!(
                "Feishu tenant_access_token failed: HTTP {}",
                response.status
            );
        }
        let token = response.payload["tenant_access_token"]
            .as_str()
            .filter(|_| response.payload["code"] == 0);
        let Some(token) = token else {
            let message = response.payload["msg"]
                .as_str()
                .filter(|message| !message.is_empty())
                .unwrap_or("Feishu tenant_access_token failed.");
            bail!("{message}");
        };
        self.tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(cache_key, (token.to_owned(), now_ms() + 90 * 60_000));
        Ok(token.to_owned())
    }

    /// `fetchFeishuAppDisplayName`。
    async fn app_name(&self, provider: &str, token: &str, app_id: &str) -> Result<Option<String>> {
        let url = format!(
            "{}/open-apis/application/v6/applications/{app_id}?lang=zh_cn",
            self.endpoints.open(provider)
        );
        let response = self
            .json(
                self.http
                    .get(url)
                    .header("authorization", format!("Bearer {token}")),
            )
            .await?;
        if !response.ok {
            bail!(
                "Feishu get application info failed app={app_id}: HTTP {}",
                response.status
            );
        }
        if response.payload["code"] != 0 {
            let message = response.payload["msg"]
                .as_str()
                .filter(|message| !message.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("Feishu get application info failed app={app_id}."));
            bail!("{message}");
        }
        Ok(resolve_app_display_name(&response.payload))
    }

    /// `readFeishuAppDisplayName`：先按 app_id 读；失败或为空时退到 `me`。
    pub async fn read_app_display_name(&self, app: &AppCredentials<'_>) -> Result<Option<String>> {
        let token = self.tenant_token(app).await?;
        let by_id = self.app_name(app.provider, &token, app.app_id).await;
        if let Ok(Some(name)) = &by_id
            && !name.trim().is_empty()
        {
            return Ok(Some(name.clone()));
        }
        match self.app_name(app.provider, &token, "me").await {
            Ok(name) => Ok(name),
            Err(error) => Err(by_id.err().unwrap_or(error)),
        }
    }

    /// `sendFeishuInteractiveCard`：返回消息 id。
    pub async fn send_card(
        &self,
        provider: &str,
        token: &str,
        receive_id: &str,
        card: &Value,
    ) -> Result<Option<String>> {
        let receive_id_type = if receive_id.starts_with("oc_") {
            "chat_id"
        } else {
            "open_id"
        };
        let url = format!(
            "{}/open-apis/im/v1/messages?receive_id_type={receive_id_type}",
            self.endpoints.open(provider)
        );
        let body = json!({
            "receive_id": receive_id,
            "msg_type": "interactive",
            "content": card.to_string(),
        });
        let response = self
            .json(
                self.http
                    .post(url)
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(body.to_string()),
            )
            .await?;
        if !response.ok || response.payload["code"] != 0 {
            return Err(message_error(
                "send interactive message",
                response.status,
                &response.payload,
                response.log_id.as_deref(),
                Some(receive_id_type),
            ));
        }
        Ok(response.payload["data"]["message_id"]
            .as_str()
            .map(str::to_owned))
    }

    /// 长连接接入点（node-sdk `pullConnectConfig`）：返回 (ws url, ClientConfig)。
    pub async fn ws_endpoint(
        &self,
        provider: &str,
        app_id: &str,
        app_secret: &str,
    ) -> Result<(String, Value), EndpointError> {
        let url = format!("{}/callback/ws/endpoint", self.endpoints.open(provider));
        let response = self
            .json(
                self.http
                    .post(url)
                    .header("locale", "zh")
                    .header("content-type", "application/json")
                    .body(json!({"AppID": app_id, "AppSecret": app_secret}).to_string()),
            )
            .await
            .map_err(|error| EndpointError::Retryable(error.to_string()))?;
        let code = response.payload["code"].as_i64().unwrap_or(-1);
        if code != 0 {
            // internal_error（1000040343）可重试；其余（含鉴权失败）是终态。
            let message = if code == 1 {
                "system busy".to_owned()
            } else {
                response.payload["msg"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            };
            return Err(
                if code == 1_000_040_343 || !response.ok && response.payload.is_null() {
                    EndpointError::Retryable(format!("code={code}, msg={message}"))
                } else {
                    EndpointError::Fatal(format!(
                        "pullConnectConfig failed: code={code}, msg={message}"
                    ))
                },
            );
        }
        let data = &response.payload["data"];
        let url = data["URL"]
            .as_str()
            .ok_or_else(|| EndpointError::Retryable("missing URL".into()))?;
        Ok((url.to_owned(), data["ClientConfig"].clone()))
    }

    async fn registration(&self, domain: &str, form: &[(&str, &str)]) -> Result<Value> {
        let url = format!("{}{REGISTRATION_PATH}", self.endpoints.accounts(domain));
        let body: String = form
            .iter()
            .map(|(key, value)| format!("{}={}", form_encode(key), form_encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        let response = self
            .http
            .post(url)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .timeout(REGISTRATION_TIMEOUT)
            .send()
            .await?;
        Ok(response.json::<Value>().await?)
    }

    /// `beginFeishuAppRegistration`：轮询域固定为 feishu。
    pub async fn begin_registration(&self, domain: Option<&str>) -> Result<Value> {
        let domain = if domain == Some("lark") {
            "lark"
        } else {
            "feishu"
        };
        let poll_domain = "feishu";
        let init = self
            .registration(poll_domain, &[("action", "init")])
            .await?;
        let supported = init["supported_auth_methods"]
            .as_array()
            .is_some_and(|methods| methods.iter().any(|method| method == "client_secret"));
        if !supported {
            bail!("Current Feishu environment does not support client_secret registration.");
        }
        let begin = self
            .registration(
                poll_domain,
                &[
                    ("action", "begin"),
                    ("archetype", "PersonalAgent"),
                    ("auth_method", "client_secret"),
                    ("request_user_info", "open_id"),
                ],
            )
            .await?;
        let (Some(device_code), Some(uri)) = (
            begin["device_code"]
                .as_str()
                .filter(|text| !text.is_empty()),
            begin["verification_uri_complete"]
                .as_str()
                .filter(|text| !text.is_empty()),
        ) else {
            bail!("Feishu app registration did not return a device code.");
        };
        let mut qr = url::Url::parse(uri)?;
        set_query(&mut qr, "from", "sdk");
        set_query(&mut qr, "source", REGISTRATION_SOURCE);
        set_query(&mut qr, "tp", "sdk");
        let expire_in = begin["expire_in"].as_i64().unwrap_or(600);
        Ok(json!({
            "deviceCode": device_code,
            "qrUrl": qr.to_string(),
            "userCode": begin["user_code"].as_str().unwrap_or_default(),
            "interval": begin["interval"].as_i64().unwrap_or(5),
            "expiresAt": now_ms() + expire_in * 1000,
            "domain": domain,
            "pollDomain": poll_domain,
        }))
    }

    /// `pollFeishuAppRegistration`。
    pub async fn poll_registration(&self, params: &Value) -> Result<Value> {
        let device_code = params["deviceCode"]
            .as_str()
            .ok_or_else(|| anyhow!("deviceCode is required"))?;
        let domain = params["domain"].as_str().unwrap_or("feishu");
        let poll_domain = params["pollDomain"].as_str().unwrap_or(domain);
        let poll = self
            .registration(
                poll_domain,
                &[("action", "poll"), ("device_code", device_code)],
            )
            .await?;
        let brand = poll["user_info"]["tenant_brand"].as_str();
        let result_domain = brand.unwrap_or(domain);
        if brand == Some("lark") && poll_domain != "lark" {
            return Ok(json!({
                "status": "pending", "interval": 0, "domain": result_domain, "pollDomain": "lark",
            }));
        }
        if let (Some(id), Some(secret)) = (
            poll["client_id"].as_str().filter(|text| !text.is_empty()),
            poll["client_secret"]
                .as_str()
                .filter(|text| !text.is_empty()),
        ) {
            let mut out = json!({
                "status": "success", "appId": id, "appSecret": secret, "domain": result_domain,
            });
            if let Some(name) = registration_app_name(&poll) {
                out["appName"] = json!(name);
            }
            if let Some(open_id) = poll["user_info"]["open_id"].as_str() {
                out["openId"] = json!(open_id);
            }
            return Ok(out);
        }
        let error = poll["error"].as_str().unwrap_or_default();
        Ok(match error {
            "" | "authorization_pending" => {
                json!({"status": "pending", "interval": 5, "domain": result_domain})
            }
            "slow_down" => json!({"status": "pending", "interval": 10, "domain": result_domain}),
            "access_denied" => json!({"status": "access_denied", "domain": result_domain}),
            "expired_token" => json!({"status": "expired", "domain": result_domain}),
            _ => json!({
                "status": "error",
                "message": format!(
                    "{error}: {}",
                    poll["error_description"].as_str().unwrap_or("unknown")
                ),
                "domain": result_domain,
            }),
        })
    }
}

/// 长连接接入点失败：可重试（网络、系统繁忙）或终态（凭据错误等）。
#[derive(Debug)]
pub enum EndpointError {
    Retryable(String),
    Fatal(String),
}

impl std::fmt::Display for EndpointError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Retryable(message) | Self::Fatal(message) => formatter.write_str(message),
        }
    }
}

/// `createFeishuMessageError`。
fn message_error(
    operation: &str,
    status: u16,
    payload: &Value,
    log_id: Option<&str>,
    receive_id_type: Option<&str>,
) -> anyhow::Error {
    let mut details = Vec::new();
    if let Some(code) = payload["code"].as_i64() {
        details.push(format!("code={code}"));
    }
    if let Some(message) = payload["msg"].as_str().filter(|text| !text.is_empty()) {
        details.push(format!("msg={message}"));
    }
    if let Some(log) = payload["error"]["log_id"].as_str().or(log_id) {
        details.push(format!("log_id={log}"));
    }
    if let Some(kind) = receive_id_type {
        details.push(format!("receive_id_type={kind}"));
    }
    let suffix = if details.is_empty() {
        String::new()
    } else {
        format!(", {}", details.join(", "))
    };
    anyhow!("Feishu {operation} failed: HTTP {status}{suffix}")
}

/// `resolveFeishuAppDisplayName`。
fn resolve_app_display_name(payload: &Value) -> Option<String> {
    let app = &payload["data"]["app"];
    let trimmed = |value: &Value| {
        value
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    if let Some(name) = trimmed(&app["app_name"]) {
        return Some(name);
    }
    let i18n = app["i18n"].as_array();
    let primary = app["primary_language"].as_str().map(str::trim);
    if let Some(name) = i18n
        .and_then(|items| {
            items
                .iter()
                .find(|item| item["i18n_key"].as_str() == primary && primary.is_some())
        })
        .and_then(|item| trimmed(&item["name"]))
    {
        return Some(name);
    }
    i18n.and_then(|items| items.iter().find_map(|item| trimmed(&item["name"])))
}

/// `readRegistrationAppName`。
fn registration_app_name(poll: &Value) -> Option<String> {
    [
        &poll["app_name"],
        &poll["client_name"],
        &poll["name"],
        &poll["app"]["app_name"],
        &poll["app"]["name"],
    ]
    .into_iter()
    .find_map(|value| {
        value
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    })
}

/// `URLSearchParams.set`：已有同名参数时替换第一个并删除其余，否则追加。
fn set_query(url: &mut url::Url, key: &str, value: &str) {
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut replaced = false;
    for (name, current) in url.query_pairs() {
        if name == key {
            if !replaced {
                replaced = true;
                pairs.push((name.into_owned(), value.to_owned()));
            }
        } else {
            pairs.push((name.into_owned(), current.into_owned()));
        }
    }
    if !replaced {
        pairs.push((key.to_owned(), value.to_owned()));
    }
    url.query_pairs_mut().clear().extend_pairs(pairs);
}

/// `application/x-www-form-urlencoded`（URLSearchParams 的编码）。
fn form_encode(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}
