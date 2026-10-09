//! OAuth provider 适配器（packages/services/src/oauth/providers/*、runtimeConfig.ts、
//! providers/zaiBusinessTokenResolver.ts、oauthProfileSchema.ts）。
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};
use url::Url;
use zcode_host_kit::log;

use super::endpoint::Endpoints;
use super::net::{ApiClient, ApiError, ApiRequest};
use super::oauth_repo::{BIGMODEL, TokenSet, UserProfile, ZAI, normalize_avatar};

const USERINFO_TIMEOUT_MS: u64 = 60_000;
const ZAI_BUSINESS_TOKEN_TIMEOUT_MS: u64 = 10_000;
const ZAI_BUSINESS_TOKEN_REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;
const BIGMODEL_USERINFO_PATH: &str = "/api/biz/customer/getCustomerInfo";
const BIGMODEL_PROFILE_SCHEMA_VERSION: i64 = 2;
const BIGMODEL_PROFILE_MIGRATION_RETRY_DELAY_MS: i64 = 60 * 60 * 1000;

/// adapter 错误：`status` 供 401/403 判定（`ApiError.status`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterError {
    pub message: String,
    pub status: Option<u16>,
}

impl AdapterError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: None,
        }
    }
}

impl From<ApiError> for AdapterError {
    fn from(error: ApiError) -> Self {
        Self {
            message: error.message,
            status: error.status,
        }
    }
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// `OAuthProviderRuntimeConfig`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfig {
    pub id: &'static str,
    pub display_name: &'static str,
    pub enabled: bool,
    pub order: u32,
    pub authorize_url: String,
    pub token_url: String,
    pub userinfo_url: String,
    pub app_id: String,
    pub redirect_uri: String,
    pub business_login_url: Option<String>,
}

fn env_flag(endpoints: &Endpoints, key: &str, fallback: bool) -> bool {
    match endpoints.env(key) {
        None => fallback,
        Some(value) => value != "0" && !value.eq_ignore_ascii_case("false"),
    }
}

pub fn bigmodel_userinfo_url(endpoints: &Endpoints) -> String {
    endpoints
        .env("BIGMODEL_OAUTH_USERINFO_URL")
        .map(str::to_owned)
        .unwrap_or_else(|| endpoints.bigmodel_api_url(BIGMODEL_USERINFO_PATH))
}

pub fn zai_userinfo_url(endpoints: &Endpoints) -> String {
    endpoints
        .env("ZAI_OAUTH_USERINFO_URL")
        .map(str::to_owned)
        .unwrap_or_else(|| endpoints.zai_oauth_url("/api/oauth/userinfo"))
}

/// `createOAuthRuntimeConfig`：bigmodel、zai 两个 provider。
pub fn runtime_config(endpoints: &Endpoints) -> Vec<ProviderConfig> {
    let token_url = |key: &str| {
        endpoints
            .env(key)
            .map(str::to_owned)
            .unwrap_or_else(|| endpoints.zcode_api_url("/api/v1/oauth/token"))
    };
    vec![
        ProviderConfig {
            id: BIGMODEL,
            display_name: "BigModel",
            enabled: env_flag(endpoints, "BIGMODEL_OAUTH_ENABLED", true),
            order: 0,
            authorize_url: endpoints
                .env("BIGMODEL_OAUTH_AUTHORIZE_URL")
                .map(str::to_owned)
                .unwrap_or_else(|| endpoints.bigmodel_api_url("/login")),
            token_url: token_url("BIGMODEL_OAUTH_TOKEN_URL"),
            userinfo_url: bigmodel_userinfo_url(endpoints),
            app_id: endpoints
                .env("BIGMODEL_OAUTH_APP_ID")
                .unwrap_or("zcode")
                .to_owned(),
            redirect_uri: endpoints.desktop_oauth_redirect_uri(),
            business_login_url: None,
        },
        ProviderConfig {
            id: ZAI,
            display_name: "Z.ai",
            enabled: env_flag(endpoints, "ZAI_OAUTH_ENABLED", true),
            order: 1,
            authorize_url: endpoints
                .env("ZAI_OAUTH_AUTHORIZE_URL")
                .map(str::to_owned)
                .unwrap_or_else(|| endpoints.zai_oauth_url("/api/oauth/authorize")),
            token_url: token_url("ZAI_OAUTH_TOKEN_URL"),
            userinfo_url: zai_userinfo_url(endpoints),
            app_id: endpoints.zai_oauth_client_id(),
            redirect_uri: endpoints.desktop_oauth_redirect_uri(),
            business_login_url: Some(
                endpoints
                    .env("ZAI_BUSINESS_LOGIN_URL")
                    .map(str::to_owned)
                    .unwrap_or_else(|| endpoints.zai_business_url("/api/auth/z/login")),
            ),
        },
    ]
}

impl ProviderConfig {
    pub fn meta(&self) -> Value {
        json!({"id": self.id, "displayName": self.display_name, "enabled": self.enabled, "order": self.order})
    }
}

/// `parseOAuthLoginAttribution`。
pub fn parse_attribution(url: &Url) -> Option<Map<String, Value>> {
    let mut attribution = Map::new();
    for key in ["channel_id", "utm_source", "utm_campaign"] {
        if let Some(value) = query(url, key).map(|v| v.trim().to_owned())
            && !value.is_empty()
        {
            attribution.insert(key.to_owned(), json!(value));
        }
    }
    (!attribution.is_empty()).then_some(attribution)
}

/// `URLSearchParams.get`：第一个同名参数。
pub fn query(url: &Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
}

pub fn has_authorization_code(url: &Url) -> bool {
    query(url, "code").is_some_and(|v| !v.is_empty())
        || query(url, "authCode").is_some_and(|v| !v.is_empty())
}

#[derive(Debug, Clone)]
pub struct CallbackParams {
    pub code: String,
    pub state: String,
}

/// `ZaiBusinessTokenResolver`：OAuth access token → api.z.ai 业务 token（带缓存）。
#[derive(Clone)]
pub struct ZaiBusinessTokenResolver {
    api: ApiClient,
    login_url: String,
    cached: Arc<Mutex<Option<(String, String, i64)>>>,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

impl ZaiBusinessTokenResolver {
    pub fn new(api: ApiClient, login_url: String) -> Self {
        Self {
            api,
            login_url,
            cached: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn resolve(&self, oauth_access_token: &str) -> Result<String, AdapterError> {
        let token = oauth_access_token.trim();
        let required = || AdapterError::new("zai_oauth_required");
        if token.is_empty() {
            return Err(required());
        }
        {
            let mut cached = self
                .cached
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((source, business, expires_at)) = cached.as_ref()
                && source == token
                && expires_at - ZAI_BUSINESS_TOKEN_REFRESH_SKEW_MS > now_ms()
            {
                return Ok(business.clone());
            }
            *cached = None;
        }
        let payload = self
            .api
            .read_json(
                ApiRequest::post(&self.login_url)
                    .json(&json!({"token": token}))
                    .timeout_ms(ZAI_BUSINESS_TOKEN_TIMEOUT_MS),
            )
            .await
            .map_err(|_| required())?;
        let code_ok = match &payload["code"] {
            Value::Null => true,
            Value::Number(number) => number.as_f64().is_some_and(|n| n == 0.0 || n == 200.0),
            Value::String(text) => text == "0" || text == "200",
            _ => false,
        };
        if !code_ok || payload["success"] == false {
            return Err(required());
        }
        let access = first_string(&payload["data"], &["access_token", "accessToken"])
            .map(|value| value.trim().to_owned())
            .unwrap_or_default();
        if access.is_empty() {
            return Err(required());
        }
        if let Some(expires_in) = payload["data"]["expires_in"]
            .as_f64()
            .filter(|value| value.is_finite() && *value != 0.0)
        {
            *self
                .cached
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((
                token.to_owned(),
                access.clone(),
                now_ms() + (expires_in * 1000.0) as i64,
            ));
        }
        Ok(access)
    }
}

/// `a?.trim() ?? b?.trim() ?? ...`：第一个存在的字符串字段（空串也算存在）。
pub fn first_string<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| value[*key].as_str())
}

fn mask(code: &str) -> String {
    let chars: Vec<char> = code.chars().collect();
    if chars.len() <= 8 {
        return "*".repeat(chars.len());
    }
    format!(
        "{}...{}",
        chars[..4].iter().collect::<String>(),
        chars[chars.len() - 4..].iter().collect::<String>()
    )
}

/// 一个 provider 的协议适配（`OAuthProviderAdapter`）。
pub struct Adapter {
    pub config: ProviderConfig,
    api: ApiClient,
    business: Option<ZaiBusinessTokenResolver>,
    /// zai：token 响应里带的 user，按 state 缓存给紧接着的 fetchUserInfo。
    last_backend_user: Mutex<Option<(String, UserProfile)>>,
}

impl Adapter {
    pub fn new(config: ProviderConfig, api: ApiClient) -> Self {
        let business = (config.id == ZAI).then(|| {
            ZaiBusinessTokenResolver::new(
                api.clone(),
                config
                    .business_login_url
                    .clone()
                    .unwrap_or_else(|| "https://api.z.ai/api/auth/z/login".to_owned()),
            )
        });
        Self {
            config,
            api,
            business,
            last_backend_user: Mutex::new(None),
        }
    }

    pub fn id(&self) -> &'static str {
        self.config.id
    }

    pub fn parse_callback(&self, url: &Url) -> Result<CallbackParams, AdapterError> {
        let (first, second, message) = if self.config.id == BIGMODEL {
            (
                "authCode",
                "code",
                "OAuth 回调缺少 authCode/code 或 state 参数",
            )
        } else {
            (
                "code",
                "authCode",
                "OAuth 回调缺少 code/authCode 或 state 参数",
            )
        };
        let code = query(url, first).or_else(|| query(url, second));
        let state = query(url, "state");
        match (code, state) {
            (Some(code), Some(state)) if !code.is_empty() && !state.is_empty() => {
                Ok(CallbackParams { code, state })
            }
            _ => Err(AdapterError::new(message)),
        }
    }

    pub fn build_authorize_url(&self, state: &str) -> String {
        let mut url = form_urlencoded_pairs();
        if self.config.id == BIGMODEL {
            url.append_pair("redirect", &self.config.redirect_uri)
                .append_pair("appId", &self.config.app_id)
                .append_pair("state", state);
        } else {
            url.append_pair("redirect_uri", &self.config.redirect_uri)
                .append_pair("response_type", "code")
                .append_pair("client_id", &self.config.app_id)
                .append_pair("state", state);
        }
        format!("{}?{}", self.config.authorize_url, url.finish())
    }

    /// 轮询 ready 的 token 归一化：zai 换成业务 token，bigmodel 原样。
    pub async fn normalize_polled(&self, mut tokens: TokenSet) -> Result<TokenSet, AdapterError> {
        if let Some(business) = &self.business {
            tokens.access_token = business.resolve(&tokens.access_token).await?;
        }
        Ok(tokens)
    }

    pub async fn exchange_token(
        &self,
        params: &CallbackParams,
        state: &str,
    ) -> Result<TokenSet, AdapterError> {
        log::info(format!(
            "[{}OAuth] zcode token request POST {} code={} codeLength={}",
            self.config.id,
            self.config.token_url,
            mask(&params.code),
            params.code.chars().count()
        ));
        let body = json!({
            "provider": self.config.id,
            "code": params.code,
            "redirect_uri": self.config.redirect_uri,
            "state": state,
        });
        let payload = match self
            .api
            .read_json(ApiRequest::post(&self.config.token_url).json(&body))
            .await
        {
            Ok(payload) => payload,
            Err(error) => {
                log::warn(format!(
                    "[{}OAuth] zcode token request failed: {} {} {:?} {:?}",
                    self.config.id, error.method, error.url, error.status, error.response_headers
                ));
                return Err(error.into());
            }
        };
        if self.config.id == BIGMODEL {
            self.bigmodel_tokens(&payload)
        } else {
            self.zai_tokens(&payload, state).await
        }
    }

    fn bigmodel_tokens(&self, payload: &Value) -> Result<TokenSet, AdapterError> {
        let code = &payload["code"];
        if !code.is_null() && code.as_f64() != Some(0.0) {
            let message = payload["msg"]
                .as_str()
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("BigModel zcode token 交换失败（code: {code}）"));
            log::warn(format!(
                "[bigmodelOAuth] zcode token business response rejected code={code}"
            ));
            return Err(AdapterError::new(message));
        }
        let data = &payload["data"];
        let jwt = data["token"].as_str().map(str::trim).unwrap_or("");
        if jwt.is_empty() {
            return Err(AdapterError::new(
                "BigModel zcode token 交换失败：响应缺少 data.token",
            ));
        }
        let access = first_string(&data["bigmodel"], &["access_token", "accessToken"])
            .or_else(|| first_string(data, &["access_token", "accessToken"]))
            .map(str::trim)
            .unwrap_or("");
        if access.is_empty() {
            return Err(AdapterError::new(
                "BigModel zcode token 交换失败：响应缺少 data.bigmodel.access_token",
            ));
        }
        let refresh = first_string(&data["bigmodel"], &["refresh_token", "refreshToken"])
            .map(str::trim)
            .unwrap_or("");
        Ok(TokenSet {
            access_token: access.to_owned(),
            refresh_token: (!refresh.is_empty()).then(|| refresh.to_owned()),
            zcode_jwt_token: Some(jwt.to_owned()),
        })
    }

    async fn zai_tokens(&self, payload: &Value, state: &str) -> Result<TokenSet, AdapterError> {
        if payload["code"].as_f64() != Some(0.0) {
            return Err(AdapterError::new(
                payload["msg"]
                    .as_str()
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                    .unwrap_or("ZAI 后端 token 交换失败"),
            ));
        }
        let data = &payload["data"];
        let jwt = data["token"].as_str().filter(|t| !t.is_empty());
        let Some(jwt) = jwt else {
            return Err(AdapterError::new("Token 交换失败：响应缺少 data.token"));
        };
        let Some(access) = data["zai"]["access_token"]
            .as_str()
            .filter(|t| !t.is_empty())
        else {
            return Err(AdapterError::new(
                "Token 交换失败：响应缺少 data.zai.access_token",
            ));
        };
        let business = match &self.business {
            Some(business) => business.resolve(access).await?,
            None => access.to_owned(),
        };
        let user = &data["user"];
        let meaningful = ["user_id", "email", "name", "avatar"]
            .iter()
            .any(|key| user[*key].as_str().is_some_and(|v| !v.trim().is_empty()));
        *self
            .last_backend_user
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            meaningful.then(|| (state.to_owned(), backend_user_profile(user)));
        Ok(TokenSet {
            access_token: business,
            refresh_token: None,
            zcode_jwt_token: Some(jwt.to_owned()),
        })
    }

    pub fn can_fetch_user_info(&self) -> bool {
        true
    }

    pub async fn fetch_user_info(
        &self,
        tokens: &TokenSet,
        state: &str,
    ) -> Result<UserProfile, AdapterError> {
        if self.config.id == BIGMODEL {
            if tokens.zcode_jwt_token.as_deref() == Some(tokens.access_token.as_str()) {
                return Ok(UserProfile::fallback());
            }
            let payload = self
                .api
                .read_json(
                    ApiRequest::get(&self.config.userinfo_url)
                        .timeout_ms(USERINFO_TIMEOUT_MS)
                        // customer 接口要求 Authorization 直接传 token，不能带 Bearer。
                        .header("Authorization", tokens.access_token.clone())
                        .header("Content-Type", "application/json"),
                )
                .await?;
            let customer = &payload["data"];
            if !customer.is_object() {
                return Ok(UserProfile::fallback());
            }
            let trimmed = |key: &str| customer[key].as_str().map(str::trim).unwrap_or("");
            let username = [trimmed("customerName"), trimmed("nickName")]
                .into_iter()
                .find(|value| !value.is_empty())
                .unwrap_or("user")
                .to_owned();
            return Ok(UserProfile {
                id: customer["customerNumber"]
                    .as_str()
                    .unwrap_or("unknown")
                    .to_owned(),
                display_name: username.clone(),
                username,
                avatar_url: customer["avatar"].as_str().map(str::to_owned),
                raw_profile: None,
            });
        }
        {
            let mut cached = self
                .last_backend_user
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if cached
                .as_ref()
                .is_some_and(|(cached_state, _)| cached_state == state)
                && let Some((_, profile)) = cached.take()
            {
                return Ok(profile);
            }
        }
        let payload = self
            .api
            .read_json(
                ApiRequest::get(&self.config.userinfo_url)
                    .timeout_ms(USERINFO_TIMEOUT_MS)
                    .bearer(&tokens.access_token)
                    .header("Content-Type", "application/json"),
            )
            .await?;
        let user = if payload["data"].is_null() {
            &payload
        } else {
            &payload["data"]
        };
        let id = first_string(user, &["sub", "id"])
            .unwrap_or("unknown")
            .to_owned();
        let username = first_string(user, &["name", "preferred_username", "email"])
            .map(str::to_owned)
            .unwrap_or_else(|| id.clone());
        Ok(UserProfile {
            id,
            display_name: username.clone(),
            username,
            avatar_url: user["picture"].as_str().map(str::to_owned),
            raw_profile: None,
        })
    }

    /// 只有 bigmodel 有旧版 `auth_token` / `refresh_token` 键。
    pub fn has_legacy_tokens(&self) -> bool {
        self.config.id == BIGMODEL
    }
}

fn form_urlencoded_pairs() -> url::form_urlencoded::Serializer<'static, String> {
    url::form_urlencoded::Serializer::new(String::new())
}

/// zai token 响应里的 `user`（`toBackendUserProfile`）。
pub fn backend_user_profile(user: &Value) -> UserProfile {
    if !user.is_object() {
        return UserProfile::fallback();
    }
    let id = user["user_id"].as_str().unwrap_or("unknown").to_owned();
    let name = user["name"].as_str().map(str::trim).unwrap_or("");
    let username = if !name.is_empty() {
        name.to_owned()
    } else {
        user["email"]
            .as_str()
            .filter(|email| !email.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| id.clone())
    };
    UserProfile {
        id,
        display_name: username.clone(),
        username,
        avatar_url: normalize_avatar(user["avatar"].as_str()),
        raw_profile: Some(user.clone()),
    }
}

/// `withProviderProfileSchema`：bigmodel 写入 schema 版本，清掉重试标记。
pub fn with_profile_schema(provider: &str, profile: &UserProfile) -> UserProfile {
    if provider != BIGMODEL {
        return profile.clone();
    }
    let mut raw = match &profile.raw_profile {
        Some(Value::Object(object)) => object.clone(),
        _ => Map::new(),
    };
    raw.shift_remove("zcodeProfileMigrationRetryAfter");
    raw.insert(
        "zcodeProfileSchemaVersion".into(),
        json!(BIGMODEL_PROFILE_SCHEMA_VERSION),
    );
    UserProfile {
        raw_profile: Some(Value::Object(raw)),
        ..profile.clone()
    }
}

fn with_retry_after(profile: &UserProfile, now: i64) -> UserProfile {
    let mut raw = match &profile.raw_profile {
        Some(Value::Object(object)) => object.clone(),
        _ => Map::new(),
    };
    raw.insert(
        "zcodeProfileMigrationRetryAfter".into(),
        json!(now + BIGMODEL_PROFILE_MIGRATION_RETRY_DELAY_MS),
    );
    UserProfile {
        raw_profile: Some(Value::Object(raw)),
        ..profile.clone()
    }
}

/// 旧版 bigmodel 缓存 profile 需要迁移时，返回下一步动作。
pub fn needs_bigmodel_migration(profile: &UserProfile, now: i64) -> bool {
    let raw = profile.raw_profile.as_ref();
    let version = raw.and_then(|raw| raw["zcodeProfileSchemaVersion"].as_f64());
    let retry_after = raw.and_then(|raw| raw["zcodeProfileMigrationRetryAfter"].as_f64());
    version.unwrap_or(0.0) < BIGMODEL_PROFILE_SCHEMA_VERSION as f64
        && retry_after.unwrap_or(0.0) <= now as f64
}

/// `refreshLegacyBigModelCachedProfile` 的结果：要写回的 profile。
pub fn migrated_bigmodel_profile(
    cached: &UserProfile,
    refreshed: Result<UserProfile, AdapterError>,
    now: i64,
) -> UserProfile {
    match refreshed {
        Ok(profile) if profile.is_fallback() => with_profile_schema(BIGMODEL, cached),
        Ok(profile) => with_profile_schema(BIGMODEL, &profile),
        Err(error) if matches!(error.status, Some(401 | 403)) => {
            with_profile_schema(BIGMODEL, cached)
        }
        // 离线、超时、5xx 只写重试时间，保留之后成功迁移的机会。
        Err(_) => with_retry_after(cached, now),
    }
}

/// `isUnauthorizedError`：消息里出现 401 / 403 / unauthorized / forbidden。
pub fn is_unauthorized(error: &AdapterError) -> bool {
    let lower = error.message.to_lowercase();
    let has_word = |word: &str| {
        lower.match_indices(word).any(|(index, _)| {
            let before = lower[..index].chars().next_back();
            let after = lower[index + word.len()..].chars().next();
            let boundary = |c: Option<char>| c.is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
            boundary(before) && boundary(after)
        })
    };
    has_word("401")
        || has_word("403")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn endpoints() -> Endpoints {
        Endpoints::from_env(BTreeMap::from([(
            "ZCODE_APP_VERSION".to_owned(),
            "1.0.0".to_owned(),
        )]))
    }

    #[test]
    fn runtime_config_and_authorize_urls() {
        let configs = runtime_config(&endpoints());
        assert_eq!(
            configs[0].token_url,
            "https://zcode.z.ai/api/v1/oauth/token"
        );
        assert_eq!(configs[0].authorize_url, "https://bigmodel.cn/login");
        assert_eq!(
            configs[1].userinfo_url,
            "https://chat.z.ai/api/oauth/userinfo"
        );
        assert_eq!(
            configs[1].business_login_url.as_deref(),
            Some("https://api.z.ai/api/auth/z/login")
        );
        let dir = tempfile::tempdir().unwrap();
        let api = ApiClient::with_network(endpoints(), dir.path().into(), Default::default());
        let zai = Adapter::new(configs[1].clone(), api);
        assert_eq!(
            zai.build_authorize_url("s1"),
            "https://chat.z.ai/api/oauth/authorize?redirect_uri=https%3A%2F%2Fzcode.z.ai%2Fapp%2Foauth%2Flogin%3Fredirect%3Dzcode%253A%252F%252Foauth%252Fcallback%26app_version%3D1.0.0&response_type=code&client_id=client_P8X5CMWmlaRO9gyO-KSqtg&state=s1"
        );
        let url =
            Url::parse("zcode://oauth/callback?authCode=c&code=d&state=s&utm_source=x").unwrap();
        assert_eq!(zai.parse_callback(&url).unwrap().code, "d");
        assert_eq!(
            parse_attribution(&url).unwrap(),
            json!({"utm_source": "x"}).as_object().unwrap().clone()
        );
    }

    #[test]
    fn unauthorized_and_migration_rules() {
        assert!(is_unauthorized(&AdapterError::new("HTTP 401")));
        assert!(is_unauthorized(&AdapterError::new("Forbidden")));
        assert!(!is_unauthorized(&AdapterError::new("code 4011")));
        let cached = UserProfile {
            id: "1".into(),
            username: "n".into(),
            display_name: "n".into(),
            avatar_url: None,
            raw_profile: None,
        };
        assert!(needs_bigmodel_migration(&cached, 0));
        let migrated = migrated_bigmodel_profile(&cached, Err(AdapterError::new("offline")), 10);
        assert_eq!(
            migrated.raw_profile.as_ref().unwrap()["zcodeProfileMigrationRetryAfter"],
            json!(10 + BIGMODEL_PROFILE_MIGRATION_RETRY_DELAY_MS)
        );
        assert!(!needs_bigmodel_migration(&migrated, 11));
        let done = migrated_bigmodel_profile(&migrated, Ok(UserProfile::fallback()), 11);
        assert_eq!(
            done.raw_profile.unwrap(),
            json!({"zcodeProfileSchemaVersion": 2})
        );
    }
}
