//! Coding Plan 权益查询（packages/services/src/model-provider/codingPlanProviderAvailability.ts、
//! bigmodel/codingPlanEntitlement.ts、model-provider/zaiStartPlanBilling.ts、
//! bigmodelStartPlanZcodeJwt.ts、providers/api/apiKeyHeaders.ts）。
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use serde_json::{Map, Value, json};
use zcode_host_kit::log;

use super::credential::CredentialStore;
use super::net::{ApiClient, ApiError, ApiRequest};
use super::oauth_adapters::now_ms;
use super::oauth_repo::{ACTIVE_PROVIDER_KEY, BIGMODEL, ZCODE_JWT_TOKEN_KEY, access_token_key};

const REQUEST_TIMEOUT_MS: u64 = 15_000;
const OFFICIAL_GLM_MODEL_IDS: [&str; 22] = [
    "GLM-5.3",
    "GLM-5.3-Flash",
    "GLM-5V-Turbo",
    "GLM-5.2",
    "GLM-5.1",
    "GLM-5.1-Highspeed",
    "GLM-5",
    "GLM-5-Turbo",
    "GLM-4.7",
    "GLM-4.7-FlashX",
    "GLM-4.7-Flash",
    "GLM-4.6",
    "GLM-4.5-Air",
    "GLM-4.5",
    "GLM-4.6V",
    "GLM-4.6V-Flash",
    "GLM-4.6V-FlashX",
    "GLM-4.1V-Thinking-FlashX",
    "GLM-4.1V-Thinking-Flash",
    "GLM-4-FlashX-250414",
    "GLM-4-Flash-250414",
    "GLM-4V-Flash",
];

/// `normalizeOfficialGlmModelId`：大小写不敏感地映射到官方规范 ID。
pub fn normalize_official_glm_model_id(model_id: &str) -> String {
    OFFICIAL_GLM_MODEL_IDS
        .iter()
        .find(|id| id.eq_ignore_ascii_case(model_id))
        .map(|id| (*id).to_owned())
        .unwrap_or_else(|| model_id.to_owned())
}

/// `normalizeApiKeyForHeader`：去 Bearer 前缀；优先取结构化 key，否则取可见 ASCII 前缀。
pub fn normalize_api_key_for_header(value: &str) -> String {
    let trimmed = value.trim();
    let without_bearer = if trimmed
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer"))
        && trimmed[6..].starts_with(char::is_whitespace)
    {
        trimmed[6..].trim()
    } else {
        trimmed
    };
    let is_key_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let chars: Vec<(usize, char)> = without_bearer.char_indices().collect();
    // /[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}/：第一个匹配。
    for start in 0..chars.len() {
        if start > 0 && is_key_char(chars[start - 1].1) {
            continue;
        }
        let mut index = start;
        while index < chars.len() && is_key_char(chars[index].1) {
            index += 1;
        }
        let left = index - start;
        if left >= 8 && index < chars.len() && chars[index].1 == '.' {
            let mut end = index + 1;
            while end < chars.len() && is_key_char(chars[end].1) {
                end += 1;
            }
            if end - index > 8 {
                // 贪婪匹配可能从更靠后的位置开始；正则的第一个匹配起点是最左的合法起点。
                let begin = chars[start].0;
                let finish = chars
                    .get(end)
                    .map(|(offset, _)| *offset)
                    .unwrap_or(without_bearer.len());
                return without_bearer[begin..finish].to_owned();
            }
        }
    }
    let prefix: String = without_bearer
        .chars()
        .take_while(|c| ('\x21'..='\x7e').contains(c))
        .collect();
    prefix.trim().to_owned()
}

#[derive(Debug, Clone, PartialEq)]
pub enum Availability {
    Available {
        models: Option<Vec<String>>,
    },
    Pending {
        effective_at: f64,
        models: Vec<String>,
    },
    Unavailable(&'static str),
    Unknown,
}

/// `CodingPlanEntitlement`。
#[derive(Debug, Clone, PartialEq)]
pub enum Entitlement {
    Available(Value),
    Unavailable(Option<&'static str>),
    Unknown,
}

impl Entitlement {
    pub fn kind(&self) -> &'static str {
        match self {
            Entitlement::Available(_) => "available",
            Entitlement::Unavailable(_) => "unavailable",
            Entitlement::Unknown => "unknown",
        }
    }
}

fn successful_data(payload: &Value) -> Option<&Value> {
    if !payload.is_object() {
        return None;
    }
    let code = &payload["code"];
    let success = &payload["success"];
    if (!code.is_null() && !code.is_number()) || (!success.is_null() && !success.is_boolean()) {
        return None;
    }
    if success == false {
        return None;
    }
    if let Some(code) = code.as_f64()
        && code != 0.0
        && code != 200.0
    {
        return None;
    }
    payload.get("data")
}

fn is_coding_product(value: &Value) -> bool {
    value.is_object()
        && ["productId", "productName"].iter().any(|key| {
            value[*key]
                .as_str()
                .is_some_and(|field| field.to_lowercase().contains("coding"))
        })
}

/// `personalSchema` 的 safeParse：只校验采用的字段。
fn parse_personal(item: &Value) -> Option<Value> {
    let optional_string = |key: &str| {
        item.get(key)
            .is_none_or(|value| value.is_null() || value.is_string())
    };
    if !item["status"].is_string()
        || !item["inCurrentPeriod"].is_boolean()
        || ![
            "productId",
            "productName",
            "nextRenewTime",
            "valid",
            "billingCycle",
        ]
        .iter()
        .all(|key| optional_string(key) && !item.get(*key).is_some_and(Value::is_null))
        || item
            .get("autoRenew")
            .is_some_and(|value| !(value.is_number() || value.is_boolean()))
    {
        return None;
    }
    let mut parsed = Map::new();
    for key in [
        "productId",
        "productName",
        "status",
        "inCurrentPeriod",
        "autoRenew",
        "nextRenewTime",
        "valid",
        "billingCycle",
    ] {
        if let Some(value) = item.get(key) {
            parsed.insert(key.to_owned(), value.clone());
        }
    }
    Some(Value::Object(parsed))
}

pub fn is_active_personal_coding_plan(subscription: &Value) -> bool {
    is_coding_product(subscription)
        && subscription["status"] == "VALID"
        && subscription["inCurrentPeriod"] == true
}

/// `fetchPersonalCodingPlanEntitlement`。
pub async fn fetch_personal_entitlement(
    api: &ApiClient,
    authorization: &str,
    url: &str,
    timeout_ms: u64,
) -> Result<Entitlement, ApiError> {
    if authorization.trim().is_empty() {
        return Ok(Entitlement::Unknown);
    }
    let payload = api
        .read_json(
            ApiRequest::get(url)
                .timeout_ms(timeout_ms)
                .header("Authorization", authorization.to_owned()),
        )
        .await?;
    let Some(list) = successful_data(&payload).and_then(Value::as_array) else {
        return Ok(Entitlement::Unknown);
    };
    let mut malformed = false;
    for item in list {
        match parse_personal(item) {
            Some(parsed) if is_active_personal_coding_plan(&parsed) => {
                return Ok(Entitlement::Available(parsed));
            }
            Some(_) => {}
            None if is_coding_product(item) => malformed = true,
            None => {}
        }
    }
    Ok(if malformed {
        Entitlement::Unknown
    } else {
        Entitlement::Unavailable(None)
    })
}

/// `fetchTeamCodingPlanEntitlement`。
pub async fn fetch_team_entitlement(
    api: &ApiClient,
    authorization: &str,
    host: &str,
    team: (&str, &str),
    timeout_ms: u64,
) -> Result<Entitlement, ApiError> {
    if authorization.trim().is_empty() {
        return Ok(Entitlement::Unknown);
    }
    let payload = api
        .read_json(
            ApiRequest::get(format!(
                "{}/api/biz/team/subscribe/product/querySubscribeDetail",
                host.strip_suffix('/').unwrap_or(host)
            ))
            .timeout_ms(timeout_ms)
            .header("Authorization", authorization.to_owned())
            .header("bigmodel-organization", team.0.to_owned())
            .header("bigmodel-project", team.1.to_owned()),
        )
        .await?;
    let Some(subscription) = successful_data(&payload).filter(|data| data.is_object()) else {
        return Ok(Entitlement::Unknown);
    };
    let nullish_string = |key: &str| {
        subscription
            .get(key)
            .is_none_or(|value| value.is_null() || value.is_string())
    };
    if !subscription["hasSubscription"].is_boolean()
        || ![
            "status",
            "memberGrantStatus",
            "productId",
            "productName",
            "subscribeEndTime",
            "subscribePeriod",
        ]
        .iter()
        .all(|key| nullish_string(key))
    {
        return Ok(Entitlement::Unknown);
    }
    if subscription["hasSubscription"] == false {
        return Ok(Entitlement::Unavailable(None));
    }
    let status = subscription["status"].as_str();
    let grant = subscription["memberGrantStatus"].as_str();
    if status == Some("EXPIRED") {
        return Ok(Entitlement::Unavailable(Some("expired")));
    }
    if status == Some("EFFECTIVE") && grant == Some("UNASSIGNED") {
        return Ok(Entitlement::Unavailable(Some("unassigned")));
    }
    if status != Some("EFFECTIVE") || grant != Some("VALID") {
        return Ok(Entitlement::Unknown);
    }
    let mut parsed = Map::new();
    for key in [
        "hasSubscription",
        "status",
        "memberGrantStatus",
        "productId",
        "productName",
        "subscribeEndTime",
        "subscribePeriod",
    ] {
        if let Some(value) = subscription.get(key) {
            parsed.insert(key.to_owned(), value.clone());
        }
    }
    Ok(Entitlement::Available(Value::Object(parsed)))
}

type BalanceFuture = Shared<BoxFuture<'static, Result<Value, ApiError>>>;

/// Start Plan 余额（`fetchZaiStartPlanBalanceEnvelope`）：同一授权的请求在发起后 1 秒内复用，
/// 失败同样复用，避免 429 后立即重复请求。
#[derive(Clone)]
pub struct StartPlanBilling {
    api: ApiClient,
    in_flight: Arc<Mutex<HashMap<String, (u64, BalanceFuture)>>>,
    sequence: Arc<std::sync::atomic::AtomicU64>,
}

impl StartPlanBilling {
    pub fn new(api: ApiClient) -> Self {
        Self {
            api,
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            sequence: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// `buildZaiStartPlanBalanceUrl`：环境 origin 的 balance 地址带 `app_version`。
    pub fn balance_url(&self) -> String {
        let endpoints = self.api.endpoints();
        let mut url =
            url::Url::parse(&endpoints.zcode_api_url("/api/v1/zcode-plan/billing/balance"))
                .expect("balance url");
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(key, _)| key != "app_version")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        url.query_pairs_mut()
            .clear()
            .extend_pairs(pairs)
            .append_pair("app_version", &endpoints.app_version());
        url.to_string()
    }

    pub async fn fetch(&self, authorization: &str, invalidate: bool) -> Result<Value, ApiError> {
        let url = self.balance_url();
        let key = json!({"authorization": authorization.trim(), "url": url}).to_string();
        let future = {
            let mut in_flight = self.in_flight.lock().unwrap_or_else(|p| p.into_inner());
            if invalidate {
                in_flight.remove(&key);
            }
            if let Some((_, existing)) = in_flight.get(&key) {
                existing.clone()
            } else {
                let id = self
                    .sequence
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let this = self.clone();
                let authorization = authorization.to_owned();
                let evict_key = key.clone();
                let future = async move {
                    let started = std::time::Instant::now();
                    let result = this.request(&url, &authorization).await;
                    let remaining = Duration::from_millis(1000).saturating_sub(started.elapsed());
                    let map = this.in_flight.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(remaining).await;
                        let mut map = map.lock().unwrap_or_else(|p| p.into_inner());
                        if map.get(&evict_key).is_some_and(|(entry, _)| *entry == id) {
                            map.remove(&evict_key);
                        }
                    });
                    result
                }
                .boxed()
                .shared();
                in_flight.insert(key, (id, future.clone()));
                future
            }
        };
        future.await
    }

    async fn request(&self, url: &str, authorization: &str) -> Result<Value, ApiError> {
        let response = self
            .api
            .request(
                ApiRequest::get(url)
                    .timeout_ms(REQUEST_TIMEOUT_MS)
                    .header("Authorization", authorization.to_owned()),
            )
            .await?;
        let response_time = response.header("date").and_then(httpdate_seconds);
        let status = response.status;
        let ok = response.ok();
        let method = response.method.clone();
        let response_url = url.to_owned();
        let body = response.text().await?;
        if !ok {
            return Err(ApiError {
                message: super::net::response_message(&body, status),
                url: response_url,
                method,
                status: Some(status),
                response_headers: None,
            });
        }
        let payload: Value = serde_json::from_str(&body).map_err(|error| ApiError {
            message: error.to_string(),
            url: response_url.clone(),
            method: method.clone(),
            status: Some(status),
            response_headers: None,
        })?;
        Ok(normalize_start_plan_expiry(payload, response_time))
    }
}

/// HTTP-date（RFC 7231 IMF-fixdate）→ Unix 秒。
fn httpdate_seconds(value: &str) -> Option<f64> {
    // "Sun, 06 Nov 1994 08:49:37 GMT"
    let parts: Vec<&str> = value.split_whitespace().collect();
    if parts.len() != 6 || parts[5] != "GMT" {
        return None;
    }
    let day: i64 = parts[1].parse().ok()?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| *m == parts[2])? as i64
        + 1;
    let year: i64 = parts[3].parse().ok()?;
    let time: Vec<i64> = parts[4]
        .split(':')
        .map(|part| part.parse().ok())
        .collect::<Option<_>>()?;
    if time.len() != 3 {
        return None;
    }
    // days_from_civil
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + time[0] * 3600 + time[1] * 60 + time[2]) as f64)
}

/// JS `Number(value)` 的子集：数字原样，字符串按十进制解析（空串为 0），其他为 NaN。
pub fn js_number(value: &Value) -> f64 {
    match value {
        Value::Number(number) => number.as_f64().unwrap_or(f64::NAN),
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                0.0
            } else {
                trimmed.parse().unwrap_or(f64::NAN)
            }
        }
        Value::Null => 0.0,
        Value::Bool(flag) => f64::from(u8::from(*flag)),
        _ => f64::NAN,
    }
}

fn status_lower(plan: &Value) -> String {
    plan["status"]
        .as_str()
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_default()
}

/// `normalizeStartPlanExpiry`：已过结束时间的 active 套餐改为 expired，并删除其余额桶。
fn normalize_start_plan_expiry(mut payload: Value, response_time: Option<f64>) -> Value {
    if !payload["data"].is_object() {
        return payload;
    }
    let server_time = payload["data"]["server_time"]
        .as_f64()
        .filter(|t| t.is_finite() && *t >= 0.0);
    let now = response_time
        .or(server_time)
        .unwrap_or_else(|| now_ms() as f64 / 1000.0);
    let plans: Vec<Value> = payload["data"]["plans"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|mut plan| {
            let end = js_number(&plan["ends_at"]);
            if status_lower(&plan) == "active" && end.is_finite() && end > 0.0 && end <= now {
                plan["status"] = json!("expired");
            }
            plan
        })
        .collect();
    let balances = payload["data"]["balances"].as_array().map(|balances| {
        balances
            .iter()
            .filter(|balance| {
                let owners: Vec<&Value> = plans
                    .iter()
                    .filter(|plan| {
                        match (
                            balance["user_plan_id"].as_str().filter(|s| !s.is_empty()),
                            plan["user_plan_id"].as_str().filter(|s| !s.is_empty()),
                        ) {
                            (Some(owner), Some(id)) => owner == id,
                            _ => plan.get("plan_id") == balance.get("plan_id"),
                        }
                    })
                    .collect();
                owners.is_empty() || owners.iter().any(|plan| status_lower(plan) != "expired")
            })
            .cloned()
            .collect::<Vec<_>>()
    });
    payload["data"]["plans"] = Value::Array(plans);
    if let Some(balances) = balances {
        payload["data"]["balances"] = Value::Array(balances);
    }
    payload
}

/// `resolveZaiStartPlanBalanceModelIds`。
pub fn start_plan_model_ids(payload: &Value) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut ids = Vec::new();
    for balance in payload["data"]["balances"].as_array().into_iter().flatten() {
        let from_capabilities: Vec<String> = balance["capabilities"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter_map(|capability| {
                let normalized = capability.trim();
                normalized
                    .to_lowercase()
                    .starts_with("model:")
                    .then(|| normalized["model:".len()..].trim().to_owned())
            })
            .filter(|id| !id.is_empty())
            .collect();
        let candidates = if from_capabilities.is_empty() {
            vec![balance["show_name"].as_str().unwrap_or("").to_owned()]
        } else {
            from_capabilities
        };
        for candidate in candidates {
            let model = normalize_official_glm_model_id(candidate.trim());
            if model.is_empty() || !seen.insert(model.to_lowercase()) {
                continue;
            }
            ids.push(model);
        }
    }
    ids
}

fn business_envelope_ok(payload: &Value) -> bool {
    payload["success"] != false
        && (payload["code"].is_null()
            || payload["code"]
                .as_f64()
                .is_some_and(|code| code == 0.0 || code == 200.0))
}

fn is_start_plan_identity(value: Option<String>) -> bool {
    value.is_some_and(|value| value.contains("start-plan") || value.contains("start plan"))
}

pub fn has_active_start_plan(plans: &Value) -> bool {
    plans.as_array().into_iter().flatten().any(|plan| {
        let lower = |key: &str| {
            plan[key]
                .as_str()
                .map(|value| value.trim().to_lowercase())
                .filter(|value| !value.is_empty())
        };
        let (plan_id, name) = (lower("plan_id"), lower("name"));
        let identity = if plan_id.is_none() && name.is_none() {
            true
        } else {
            is_start_plan_identity(plan_id) || is_start_plan_identity(name)
        };
        status_lower(plan) == "active" && identity
    })
}

fn start_plan_availability(payload: &Value) -> Availability {
    if !business_envelope_ok(payload) {
        return Availability::Unavailable("coding_plan_auth_failed");
    }
    if !has_active_start_plan(&payload["data"]["plans"]) {
        return Availability::Unavailable("coding_plan_not_entitled");
    }
    let models = start_plan_model_ids(payload);
    let now = payload["data"]["server_time"]
        .as_f64()
        .filter(|t| t.is_finite() && *t >= 0.0)
        .unwrap_or_else(|| now_ms() as f64 / 1000.0);
    let effective: Vec<f64> = payload["data"]["plans"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|plan| plan["status"].as_str().map(str::to_lowercase).as_deref() == Some("active"))
        .flat_map(|plan| {
            let entitlements = plan["entitlements"].as_array().filter(|e| !e.is_empty());
            match entitlements {
                Some(entries) => entries
                    .iter()
                    .map(|entry| entry["effective_at"].clone())
                    .collect(),
                None => vec![plan["starts_at"].clone()],
            }
        })
        .map(|value| match &value {
            Value::Null => f64::NAN,
            Value::String(text) if text.is_empty() => f64::NAN,
            _ => js_number(&value),
        })
        .collect();
    if models.is_empty()
        && !effective.is_empty()
        && effective
            .iter()
            .all(|value| value.is_finite() && *value > now)
    {
        return Availability::Pending {
            effective_at: effective.iter().cloned().fold(f64::INFINITY, f64::min),
            models: Vec::new(),
        };
    }
    Availability::Available {
        models: (!models.is_empty()).then_some(models),
    }
}

fn classify_error(error: &ApiError) -> Availability {
    match error.status {
        Some(401 | 403) => Availability::Unavailable("coding_plan_auth_failed"),
        _ => Availability::Unknown,
    }
}

/// 一个待查询的账号 provider（`CodingPlanAvailabilityProvider`）。
#[derive(Debug, Clone)]
pub struct AvailabilityProvider {
    pub provider_id: String,
    pub family: String,
    pub plan_kind: String,
    pub api_key: Option<String>,
}

/// `resolveBigModelStartPlanZcodeJwt`。
pub async fn bigmodel_start_plan_jwt(
    credentials: &CredentialStore,
    provider_api_key: Option<&str>,
    trust_cached: bool,
) -> String {
    let active = credentials
        .load(ACTIVE_PROVIDER_KEY)
        .await
        .ok()
        .flatten()
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();
    if trust_cached || active == BIGMODEL {
        let jwt = credentials
            .load(ZCODE_JWT_TOKEN_KEY)
            .await
            .ok()
            .flatten()
            .map(|value| value.trim().to_owned())
            .unwrap_or_default();
        if !jwt.is_empty() {
            return jwt;
        }
    }
    provider_api_key.map(str::trim).unwrap_or("").to_owned()
}

#[derive(Clone)]
pub struct AvailabilityResolver {
    api: ApiClient,
    credentials: CredentialStore,
    billing: StartPlanBilling,
}

impl AvailabilityResolver {
    pub fn new(api: ApiClient, credentials: CredentialStore, billing: StartPlanBilling) -> Self {
        Self {
            api,
            credentials,
            billing,
        }
    }

    pub fn billing(&self) -> &StartPlanBilling {
        &self.billing
    }

    async fn load_trimmed(&self, key: &str) -> String {
        self.credentials
            .load(key)
            .await
            .ok()
            .flatten()
            .map(|value| value.trim().to_owned())
            .unwrap_or_default()
    }

    async fn start_plan(&self, provider: &AvailabilityProvider) -> Availability {
        let authorization = if provider.family == "bigmodel" {
            let jwt =
                bigmodel_start_plan_jwt(&self.credentials, provider.api_key.as_deref(), false)
                    .await;
            if jwt.is_empty() {
                String::new()
            } else {
                format!("Bearer {jwt}")
            }
        } else {
            let credential_jwt = self.load_trimmed(ZCODE_JWT_TOKEN_KEY).await;
            let provider_jwt =
                normalize_api_key_for_header(provider.api_key.as_deref().unwrap_or(""));
            let jwt = if credential_jwt.is_empty() {
                provider_jwt
            } else {
                credential_jwt
            };
            if jwt.is_empty() {
                String::new()
            } else {
                format!("Bearer {jwt}")
            }
        };
        if authorization.is_empty() {
            return Availability::Unavailable("coding_plan_not_authenticated");
        }
        match self.billing.fetch(&authorization, false).await {
            Ok(payload) => {
                log::info(format!(
                    "[coding-plan-availability] billing/balance 请求完成 provider={} plans={} active={}",
                    provider.provider_id,
                    payload["data"]["plans"].as_array().map_or(0, Vec::len),
                    has_active_start_plan(&payload["data"]["plans"])
                ));
                start_plan_availability(&payload)
            }
            Err(error) => {
                log::warn(format!(
                    "[coding-plan-availability] billing/balance 请求失败 provider={} status={:?}: {}",
                    provider.provider_id, error.status, error.message
                ));
                classify_error(&error)
            }
        }
    }

    async fn individual(&self, provider: &AvailabilityProvider) -> Availability {
        let endpoints = self.api.endpoints();
        let url = if provider.family == "zai" {
            endpoints.zai_business_url("/api/biz/subscription/list")
        } else {
            endpoints.bigmodel_api_url("/api/biz/subscription/list")
        };
        let authorization = normalize_api_key_for_header(provider.api_key.as_deref().unwrap_or(""));
        // key 尚未准备好只说明权益未知，不能当成未订阅。
        if authorization.is_empty() {
            return Availability::Unknown;
        }
        match fetch_personal_entitlement(&self.api, &authorization, &url, REQUEST_TIMEOUT_MS).await
        {
            Ok(Entitlement::Unavailable(_)) => {
                Availability::Unavailable("coding_plan_not_entitled")
            }
            Ok(Entitlement::Available(_)) => Availability::Available { models: None },
            Ok(Entitlement::Unknown) => Availability::Unknown,
            Err(error) => classify_error(&error),
        }
    }

    async fn selected_team(&self, family: &str, selections: &Value) -> Availability {
        let selection = &selections[family];
        if selection["kind"] != "team-coding-plan" {
            return Availability::Unknown;
        }
        let selected_org = selection["organizationId"]
            .as_str()
            .filter(|s| !s.is_empty());
        let project_id = selection["projectId"].as_str().unwrap_or("");
        let provider = super::auth::oauth_provider_of(family);
        let token = self.load_trimmed(&access_token_key(provider)).await;
        if token.is_empty() {
            return Availability::Unavailable("coding_plan_not_connected");
        }
        let jwt = self.load_trimmed(ZCODE_JWT_TOKEN_KEY).await;
        if family == "bigmodel" && !jwt.is_empty() && token == jwt {
            return Availability::Unavailable("coding_plan_not_connected");
        }
        let endpoints = self.api.endpoints();
        let host = if family == "zai" {
            endpoints.zai_business_url("")
        } else {
            endpoints.bigmodel_api_origin()
        };
        let payload = match self
            .api
            .read_json(
                ApiRequest::get(format!("{host}/api/biz/customer/getCustomerInfo"))
                    .timeout_ms(REQUEST_TIMEOUT_MS)
                    .header("Authorization", token.clone())
                    .header("Content-Type", "application/json"),
            )
            .await
        {
            Ok(payload) => payload,
            Err(error) => {
                log::warn(format!(
                    "[coding-plan-availability] Team Plan 入口项目校验失败 family={family} status={:?}: {}",
                    error.status, error.message
                ));
                return classify_error(&error);
            }
        };
        if !business_envelope_ok(&payload) || !payload["data"]["organizations"].is_array() {
            return Availability::Unknown;
        }
        let mut resolved = None;
        'outer: for organization in payload["data"]["organizations"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let organization_id = organization["organizationId"]
                .as_str()
                .map(str::trim)
                .unwrap_or("");
            if selected_org.is_some_and(|selected| organization_id != selected) {
                continue;
            }
            for project in organization["projects"].as_array().into_iter().flatten() {
                if project["projectId"].as_str().map(str::trim) == Some(project_id)
                    && super::auth::project_type(project) == "2"
                    && !organization_id.is_empty()
                {
                    resolved = Some(organization_id.to_owned());
                    break 'outer;
                }
            }
        }
        log::info(format!(
            "[coding-plan-availability] Team Plan 入口项目校验完成 family={family} hasSelectedTeamProject={}",
            resolved.is_some()
        ));
        let Some(organization) = resolved else {
            return Availability::Unavailable("coding_plan_not_entitled");
        };
        let authorization = if family == "zai" {
            format!("Bearer {token}")
        } else {
            token
        };
        match fetch_team_entitlement(
            &self.api,
            &authorization,
            &host,
            (&organization, project_id),
            REQUEST_TIMEOUT_MS,
        )
        .await
        {
            Ok(Entitlement::Unavailable(_)) => {
                Availability::Unavailable("coding_plan_not_entitled")
            }
            Ok(Entitlement::Available(_)) => Availability::Available { models: None },
            Ok(Entitlement::Unknown) => Availability::Unknown,
            Err(error) => classify_error(&error),
        }
    }

    /// `validate{Zai,BigModel}AccountProviderAvailability`：Start 与个人订阅独立查询，Team 只在
    /// 当前选择是 Team 时查询，否则 unknown。
    pub async fn family(
        &self,
        family: &str,
        providers: &[AvailabilityProvider],
        selections: &Value,
    ) -> HashMap<String, Availability> {
        let find = |kind: &str| {
            providers
                .iter()
                .find(|provider| provider.family == family && provider.plan_kind == kind)
        };
        let mut result = HashMap::new();
        let (start, individual, team) = (
            find("start-plan"),
            find("individual-coding-plan"),
            find("team-coding-plan"),
        );
        if let Some(start) = start {
            result.insert(start.provider_id.clone(), self.start_plan(start).await);
        }
        if let Some(individual) = individual {
            result.insert(
                individual.provider_id.clone(),
                self.individual(individual).await,
            );
        }
        if let Some(team) = team {
            let availability = if selections[family]["kind"] == "team-coding-plan" {
                self.selected_team(family, selections).await
            } else {
                Availability::Unknown
            };
            result.insert(team.provider_id.clone(), availability);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_header_normalization_matches_node() {
        assert_eq!(
            normalize_api_key_for_header("Bearer abcdefgh.ijklmnop"),
            "abcdefgh.ijklmnop"
        );
        assert_eq!(normalize_api_key_for_header("  key-123 备注"), "key-123");
        assert_eq!(
            normalize_api_key_for_header("中文 abcdefgh.ijklmnop 尾巴"),
            "abcdefgh.ijklmnop"
        );
        assert_eq!(normalize_api_key_for_header("中文"), "");
        assert_eq!(
            normalize_official_glm_model_id("glm-5.3-flash"),
            "GLM-5.3-Flash"
        );
        assert_eq!(normalize_official_glm_model_id("custom"), "custom");
    }

    #[test]
    fn start_plan_balance_rules() {
        let payload = json!({"code": 0, "data": {
            "server_time": 100,
            "plans": [
                {"user_plan_id": "a", "plan_id": "zcode-v3-start-plan", "status": "active", "ends_at": 50},
                {"user_plan_id": "b", "plan_id": "zcode-v3-start-plan", "status": "ACTIVE", "ends_at": 500},
            ],
            "balances": [
                {"user_plan_id": "a", "capabilities": ["model:glm-5.2"]},
                {"user_plan_id": "b", "capabilities": ["Model: GLM-5.3-flash", "other"]},
                {"show_name": "glm-5-turbo"},
            ],
        }});
        let normalized = normalize_start_plan_expiry(payload, None);
        assert_eq!(normalized["data"]["plans"][0]["status"], "expired");
        assert_eq!(normalized["data"]["balances"].as_array().unwrap().len(), 2);
        assert_eq!(
            start_plan_model_ids(&normalized),
            vec!["GLM-5.3-Flash", "GLM-5-Turbo"]
        );
        assert_eq!(
            start_plan_availability(&normalized),
            Availability::Available {
                models: Some(vec!["GLM-5.3-Flash".into(), "GLM-5-Turbo".into()])
            }
        );
        let pending = json!({"data": {"server_time": 10, "plans": [{"status": "active", "name": "Start Plan", "entitlements": [{"effective_at": "20"}, {"effective_at": 30}]}]}});
        assert_eq!(
            start_plan_availability(&pending),
            Availability::Pending {
                effective_at: 20.0,
                models: vec![]
            }
        );
        assert_eq!(
            start_plan_availability(&json!({"code": 401})),
            Availability::Unavailable("coding_plan_auth_failed")
        );
        assert_eq!(
            start_plan_availability(
                &json!({"data": {"plans": [{"status": "active", "name": "Pro"}]}})
            ),
            Availability::Unavailable("coding_plan_not_entitled")
        );
        assert_eq!(
            httpdate_seconds("Thu, 01 Jan 1970 00:01:40 GMT"),
            Some(100.0)
        );
    }
}
