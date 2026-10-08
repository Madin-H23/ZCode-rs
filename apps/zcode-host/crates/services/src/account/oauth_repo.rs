//! OAuth 凭据仓储（packages/services/src/oauth/repo/oauthCredentialRepo.ts）。
//!
//! 键：`oauth:active_provider`、`oauth:<p>:access_token`、`oauth:<p>:refresh_token`、
//! `oauth:<p>:user_info`、共享 `zcodejwttoken`、`oauth:login_attribution`。任一读取遇到解密失败，
//! 等价于强制退出全部已知 provider，并通知上层清理派生的 Coding Plan key。
use std::sync::{Arc, RwLock};

use base64::Engine;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use zcode_host_kit::log;

use super::credential::{CredentialError, CredentialStore};

pub const BIGMODEL: &str = "bigmodel";
pub const ZAI: &str = "zai";
pub const KNOWN_PROVIDERS: [&str; 2] = [BIGMODEL, ZAI];
pub const ACTIVE_PROVIDER_KEY: &str = "oauth:active_provider";
pub const LOGIN_ATTRIBUTION_KEY: &str = "oauth:login_attribution";
pub const ZCODE_JWT_TOKEN_KEY: &str = "zcodejwttoken";
const ATTRIBUTION_KEYS: [&str; 3] = ["channel_id", "utm_source", "utm_campaign"];

pub fn access_token_key(provider: &str) -> String {
    format!("oauth:{provider}:access_token")
}

pub fn refresh_token_key(provider: &str) -> String {
    format!("oauth:{provider}:refresh_token")
}

pub fn user_info_key(provider: &str) -> String {
    format!("oauth:{provider}:user_info")
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub zcode_jwt_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UserProfile {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub raw_profile: Option<Value>,
}

impl UserProfile {
    /// adapter 拿不到用户信息时的哨兵值。
    pub fn fallback() -> Self {
        Self {
            id: "unknown".into(),
            username: "user".into(),
            display_name: "User".into(),
            avatar_url: None,
            raw_profile: None,
        }
    }

    pub fn is_fallback(&self) -> bool {
        self.id == "unknown" && self.username == "user" && self.display_name == "User"
    }

    /// `JSON.stringify(profile)` 的键序：id、username、displayName、avatarUrl、rawProfile。
    pub fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert("id".into(), json!(self.id));
        object.insert("username".into(), json!(self.username));
        object.insert("displayName".into(), json!(self.display_name));
        if let Some(avatar) = &self.avatar_url {
            object.insert("avatarUrl".into(), json!(avatar));
        }
        if let Some(raw) = &self.raw_profile {
            object.insert("rawProfile".into(), raw.clone());
        }
        Value::Object(object)
    }

    /// `UserInfo`（`toUserInfo`）：avatarUrl 为空串时省略。
    pub fn user_info(&self) -> Value {
        let mut object = Map::new();
        object.insert("id".into(), json!(self.id));
        object.insert("username".into(), json!(self.username));
        object.insert("displayName".into(), json!(self.display_name));
        if let Some(avatar) = self.avatar_url.as_deref().filter(|value| !value.is_empty()) {
            object.insert("avatarUrl".into(), json!(avatar));
        }
        Value::Object(object)
    }

    /// `isSameOAuthProfile`：rawProfile 按 `JSON.stringify` 文本比较（键序敏感）。
    pub fn same_as(&self, other: &UserProfile) -> bool {
        let raw = |profile: &UserProfile| {
            profile
                .raw_profile
                .clone()
                .unwrap_or(Value::Null)
                .to_string()
        };
        self.id == other.id
            && self.username == other.username
            && self.display_name == other.display_name
            && self.avatar_url == other.avatar_url
            && raw(self) == raw(other)
    }
}

fn infer_image_mime(decoded: &[u8]) -> &'static str {
    if decoded.len() >= 8 && decoded[..4] == [0x89, 0x50, 0x4e, 0x47] {
        return "image/png";
    }
    if decoded.len() >= 3 && decoded[..3] == [0xff, 0xd8, 0xff] {
        return "image/jpeg";
    }
    if decoded.len() >= 6 && &decoded[..3] == b"GIF" {
        return "image/gif";
    }
    if decoded.len() >= 12 && &decoded[..4] == b"RIFF" && &decoded[8..12] == b"WEBP" {
        return "image/webp";
    }
    "image/png"
}

fn base64_image_data_url(raw: &str) -> Option<String> {
    let normalized: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    let body = normalized.trim_end_matches('=');
    let padding = normalized.len() - body.len();
    if normalized.len() < 16
        || padding > 2
        || body.is_empty()
        || !body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
    {
        return None;
    }
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    );
    let decoded = engine.decode(body).ok()?;
    if decoded.is_empty() {
        return None;
    }
    let reencoded = base64::engine::general_purpose::STANDARD_NO_PAD.encode(&decoded);
    if reencoded != body {
        return None;
    }
    Some(format!(
        "data:{};base64,{normalized}",
        infer_image_mime(&decoded)
    ))
}

/// zai 头像：data URL 或 http(s) 原样；裸 base64 补成 data URL；其余原样保留。
pub fn normalize_avatar(avatar: Option<&str>) -> Option<String> {
    let trimmed = avatar?.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    let is_data_url = lower.starts_with("data:image/")
        && lower["data:image/".len()..]
            .split_once(";base64,")
            .is_some_and(|(subtype, _)| !subtype.is_empty() && !subtype.contains(';'));
    if is_data_url || lower.starts_with("http://") || lower.starts_with("https://") {
        return Some(trimmed.to_owned());
    }
    Some(base64_image_data_url(trimmed).unwrap_or_else(|| trimmed.to_owned()))
}

/// zai 后端原始 `user` 对象 → 展示 profile（`toOAuthUserProfileFromRawZaiUser`）。
fn profile_from_raw_zai_user(raw: &Value) -> Option<UserProfile> {
    let id = raw["user_id"].as_str().unwrap_or("unknown").to_owned();
    let name = raw["name"].as_str().map(str::trim).unwrap_or("");
    let email = raw["email"].as_str().unwrap_or("");
    if name.is_empty() && email.is_empty() && id == "unknown" {
        return None;
    }
    let username = if !name.is_empty() {
        name.to_owned()
    } else if !email.is_empty() {
        email.to_owned()
    } else {
        id.clone()
    };
    Some(UserProfile {
        id,
        display_name: username.clone(),
        username,
        avatar_url: normalize_avatar(raw["avatar"].as_str()),
        raw_profile: Some(raw.clone()),
    })
}

pub type CorruptClearedHandler = Arc<dyn Fn(Vec<String>) -> BoxFuture<'static, ()> + Send + Sync>;

#[derive(Clone)]
pub struct OAuthCredentialRepo {
    credentials: CredentialStore,
    known: Vec<String>,
    on_corrupt_cleared: Arc<RwLock<Option<CorruptClearedHandler>>>,
}

impl OAuthCredentialRepo {
    pub fn new(credentials: CredentialStore) -> Self {
        Self {
            credentials,
            known: KNOWN_PROVIDERS.iter().map(|p| (*p).to_owned()).collect(),
            on_corrupt_cleared: Arc::new(RwLock::new(None)),
        }
    }

    pub fn credentials(&self) -> &CredentialStore {
        &self.credentials
    }

    pub fn set_corrupt_cleared_handler(&self, handler: CorruptClearedHandler) {
        *self
            .on_corrupt_cleared
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handler);
    }

    /// 解密失败统一按损坏登录态处理；其他错误原样上抛。
    async fn guarded<T>(
        &self,
        result: Result<T, CredentialError>,
        fallback: T,
    ) -> Result<T, CredentialError> {
        match result {
            Err(error) if error.is_decrypt() => {
                self.clear_corrupt_session().await;
                Ok(fallback)
            }
            other => other,
        }
    }

    pub async fn active_provider(&self) -> Result<Option<String>, CredentialError> {
        let result = self.credentials.load(ACTIVE_PROVIDER_KEY).await;
        self.guarded(result, None).await
    }

    pub async fn set_active_provider(&self, provider: Option<&str>) -> Result<(), CredentialError> {
        match provider {
            None | Some("") => self.credentials.delete(ACTIVE_PROVIDER_KEY).await,
            Some(provider) => self.credentials.save(ACTIVE_PROVIDER_KEY, provider).await,
        }
    }

    pub async fn active_token_set(&self) -> Result<Option<TokenSet>, CredentialError> {
        match self.active_provider().await? {
            Some(provider) => self.token_set(&provider).await,
            None => Ok(None),
        }
    }

    pub async fn save_active_token_set(&self, tokens: &TokenSet) -> Result<(), CredentialError> {
        let Some(provider) = self.active_provider().await? else {
            return Err(CredentialError::Io(
                "保存当前登录 token 前缺少 active provider".into(),
            ));
        };
        self.save_token_set(&provider, tokens).await
    }

    pub async fn active_user_profile(&self) -> Result<Option<UserProfile>, CredentialError> {
        match self.active_provider().await? {
            Some(provider) => self.user_profile(&provider).await,
            None => Ok(None),
        }
    }

    pub async fn save_active_user_profile(
        &self,
        profile: &UserProfile,
    ) -> Result<(), CredentialError> {
        let Some(provider) = self.active_provider().await? else {
            return Err(CredentialError::Io(
                "保存当前登录 user 前缺少 active provider".into(),
            ));
        };
        self.save_user_profile(&provider, profile).await
    }

    pub async fn save_login_attribution(
        &self,
        attribution: &Map<String, Value>,
    ) -> Result<(), CredentialError> {
        let persist: Map<String, Value> = attribution
            .iter()
            .filter_map(|(key, value)| {
                let trimmed = value.as_str().map(str::trim).unwrap_or("");
                (!trimmed.is_empty()).then(|| (key.clone(), json!(trimmed)))
            })
            .collect();
        if persist.is_empty() {
            return Ok(());
        }
        self.credentials
            .save(LOGIN_ATTRIBUTION_KEY, &Value::Object(persist).to_string())
            .await
    }

    pub async fn login_attribution(&self) -> Result<Option<Map<String, Value>>, CredentialError> {
        let raw = match self.credentials.load(LOGIN_ATTRIBUTION_KEY).await {
            Ok(raw) => raw,
            Err(error) if error.is_decrypt() => return Ok(None),
            Err(error) => return Err(error),
        };
        let Some(raw) = raw.filter(|raw| !raw.is_empty()) else {
            return Ok(None);
        };
        let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
            self.credentials.delete(LOGIN_ATTRIBUTION_KEY).await?;
            return Ok(None);
        };
        // 兼容早期 `{params, expiresAt}` 结构。
        let stored = if parsed["params"].is_object() {
            &parsed["params"]
        } else {
            &parsed
        };
        let params: Map<String, Value> = stored
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(key, _)| ATTRIBUTION_KEYS.contains(&key.as_str()))
            .filter_map(|(key, value)| {
                let trimmed = value.as_str().map(str::trim).unwrap_or("");
                (!trimmed.is_empty()).then(|| (key.clone(), json!(trimmed)))
            })
            .collect();
        if params.is_empty() {
            self.credentials.delete(LOGIN_ATTRIBUTION_KEY).await?;
            return Ok(None);
        }
        Ok(Some(params))
    }

    pub async fn clear_active_session(&self) -> Result<(), CredentialError> {
        if let Some(provider) = self.active_provider().await? {
            self.clear_provider(&provider).await?;
        }
        self.credentials.delete(ACTIVE_PROVIDER_KEY).await
    }

    async fn load_token_set_raw(&self, provider: &str) -> Result<Option<TokenSet>, CredentialError> {
        let Some(access_token) = self
            .credentials
            .load(&access_token_key(provider))
            .await?
            .filter(|token| !token.is_empty())
        else {
            return Ok(None);
        };
        let refresh_token = self
            .credentials
            .load(&refresh_token_key(provider))
            .await?
            .filter(|token| !token.is_empty());
        let zcode_jwt_token = if KNOWN_PROVIDERS.contains(&provider) {
            self.credentials
                .load(ZCODE_JWT_TOKEN_KEY)
                .await?
                .filter(|token| !token.is_empty())
        } else {
            None
        };
        Ok(Some(TokenSet {
            access_token,
            refresh_token,
            zcode_jwt_token,
        }))
    }

    pub async fn token_set(&self, provider: &str) -> Result<Option<TokenSet>, CredentialError> {
        let result = self.load_token_set_raw(provider).await;
        self.guarded(result, None).await
    }

    pub async fn save_token_set(
        &self,
        provider: &str,
        tokens: &TokenSet,
    ) -> Result<(), CredentialError> {
        self.credentials
            .save(&access_token_key(provider), &tokens.access_token)
            .await?;
        match tokens.refresh_token.as_deref().filter(|t| !t.is_empty()) {
            Some(refresh) => {
                self.credentials
                    .save(&refresh_token_key(provider), refresh)
                    .await?
            }
            None => self.credentials.delete(&refresh_token_key(provider)).await?,
        }
        if KNOWN_PROVIDERS.contains(&provider) {
            match tokens.zcode_jwt_token.as_deref().filter(|t| !t.is_empty()) {
                Some(jwt) => self.credentials.save(ZCODE_JWT_TOKEN_KEY, jwt).await?,
                None => self.credentials.delete(ZCODE_JWT_TOKEN_KEY).await?,
            }
        }
        Ok(())
    }

    pub async fn user_profile(&self, provider: &str) -> Result<Option<UserProfile>, CredentialError> {
        let raw = match self.credentials.load(&user_info_key(provider)).await {
            Ok(raw) => raw,
            Err(error) if error.is_decrypt() => {
                self.clear_corrupt_session().await;
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let Some(raw) = raw.filter(|raw| !raw.is_empty()) else {
            return Ok(None);
        };
        let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
            return Ok(None);
        };
        if let (Some(id), Some(username), Some(display_name)) = (
            parsed["id"].as_str(),
            parsed["username"].as_str(),
            parsed["displayName"].as_str(),
        ) {
            return Ok(Some(UserProfile {
                id: id.to_owned(),
                username: username.to_owned(),
                display_name: display_name.to_owned(),
                avatar_url: parsed["avatarUrl"]
                    .as_str()
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned),
                raw_profile: parsed
                    .get("rawProfile")
                    .filter(|value| value.is_object() || value.is_array())
                    .cloned(),
            }));
        }
        if provider == ZAI && parsed.is_object() {
            return Ok(profile_from_raw_zai_user(&parsed));
        }
        Ok(None)
    }

    /// zai 存后端原始 user 对象，其他 provider 存归一化 profile。
    pub async fn save_user_profile(
        &self,
        provider: &str,
        profile: &UserProfile,
    ) -> Result<(), CredentialError> {
        let persist = match (&profile.raw_profile, provider) {
            (Some(raw), ZAI) => raw.clone(),
            _ => profile.to_json(),
        };
        self.credentials
            .save(&user_info_key(provider), &persist.to_string())
            .await
    }

    pub async fn clear_user_profile(&self, provider: &str) -> Result<(), CredentialError> {
        self.credentials.delete(&user_info_key(provider)).await
    }

    pub async fn clear_provider(&self, provider: &str) -> Result<(), CredentialError> {
        self.credentials.delete(&access_token_key(provider)).await?;
        self.credentials.delete(&refresh_token_key(provider)).await?;
        self.credentials.delete(&user_info_key(provider)).await?;
        if KNOWN_PROVIDERS.contains(&provider) {
            self.credentials.delete(ZCODE_JWT_TOKEN_KEY).await?;
        }
        Ok(())
    }

    pub async fn clear_all(&self, providers: &[String]) -> Result<(), CredentialError> {
        for provider in providers {
            self.clear_provider(provider).await?;
        }
        self.set_active_provider(None).await
    }

    /// 解密失败：清空已知 provider 与共享 JWT，再通知清理派生凭据；不碰 SSH 等其他凭据。
    async fn clear_corrupt_session(&self) {
        for provider in &self.known {
            if let Err(error) = self.clear_provider(provider).await {
                log::warn(format!(
                    "[oauthCredentialRepo] clear corrupt provider {provider} failed: {error}"
                ));
            }
        }
        if let Err(error) = self.credentials.delete(ACTIVE_PROVIDER_KEY).await {
            log::warn(format!(
                "[oauthCredentialRepo] clear corrupt active provider failed: {error}"
            ));
        }
        let handler = self
            .on_corrupt_cleared
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(handler) = handler {
            handler(self.known.clone()).await;
        }
    }
}

/// `encodeURIComponent`。
pub fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')')
        {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `resolveJwtExpiration`：`expired` / `valid` 带毫秒时间戳，解析不了为 `unknown`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JwtExpiration {
    Expired(i64),
    Valid(i64),
    Unknown,
}

pub fn jwt_expiration(token: &str, now_ms: i64) -> JwtExpiration {
    let Some(segment) = token.split('.').nth(1).filter(|s| !s.is_empty()) else {
        return JwtExpiration::Unknown;
    };
    let normalized = segment.replace('-', "+").replace('_', "/");
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    );
    let Ok(bytes) = engine.decode(normalized.trim_end_matches('=')) else {
        return JwtExpiration::Unknown;
    };
    let Ok(payload) = serde_json::from_slice::<Value>(&bytes) else {
        return JwtExpiration::Unknown;
    };
    let Some(exp) = payload["exp"].as_f64().filter(|exp| exp.is_finite() && *exp > 0.0) else {
        return JwtExpiration::Unknown;
    };
    let expires_at = (exp * 1000.0) as i64;
    if now_ms + 30_000 >= expires_at {
        JwtExpiration::Expired(expires_at)
    } else {
        JwtExpiration::Valid(expires_at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn token_and_profile_roundtrip_with_node_layout() {
        let dir = tempfile::tempdir().unwrap();
        let repo = OAuthCredentialRepo::new(CredentialStore::new(dir.path(), "s"));
        repo.save_token_set(
            ZAI,
            &TokenSet {
                access_token: "a".into(),
                refresh_token: None,
                zcode_jwt_token: Some("jwt".into()),
            },
        )
        .await
        .unwrap();
        repo.set_active_provider(Some(ZAI)).await.unwrap();
        let raw_user = json!({"user_id": "u1", "name": " Alice ", "email": "a@x", "avatar": "https://img"});
        let profile = profile_from_raw_zai_user(&raw_user).unwrap();
        repo.save_user_profile(ZAI, &profile).await.unwrap();
        // zai 存原始 user 对象。
        let stored = repo.credentials().load(&user_info_key(ZAI)).await.unwrap().unwrap();
        assert_eq!(serde_json::from_str::<Value>(&stored).unwrap(), raw_user);
        let loaded = repo.active_user_profile().await.unwrap().unwrap();
        assert_eq!(loaded.username, "Alice");
        assert_eq!(loaded.user_info(), json!({"id": "u1", "username": "Alice", "displayName": "Alice", "avatarUrl": "https://img"}));
        let tokens = repo.active_token_set().await.unwrap().unwrap();
        assert_eq!(tokens.zcode_jwt_token.as_deref(), Some("jwt"));
        repo.clear_active_session().await.unwrap();
        assert_eq!(repo.active_provider().await.unwrap(), None);
        assert_eq!(repo.credentials().load(ZCODE_JWT_TOKEN_KEY).await.unwrap(), None);
    }

    #[tokio::test]
    async fn decrypt_failure_clears_all_oauth_keys_but_keeps_others() {
        let dir = tempfile::tempdir().unwrap();
        let good = CredentialStore::new(dir.path(), "right");
        good.save("ssh:host", "keep").await.unwrap();
        good.save(&access_token_key(BIGMODEL), "t").await.unwrap();
        good.save(ACTIVE_PROVIDER_KEY, BIGMODEL).await.unwrap();
        let repo = OAuthCredentialRepo::new(CredentialStore::new(dir.path(), "wrong"));
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        repo.set_corrupt_cleared_handler(Arc::new(move |providers| {
            let sender = sender.clone();
            Box::pin(async move {
                let _ = sender.send(providers);
            })
        }));
        assert_eq!(repo.active_provider().await.unwrap(), None);
        assert_eq!(receiver.recv().await.unwrap(), vec!["bigmodel", "zai"]);
        assert_eq!(good.load(ACTIVE_PROVIDER_KEY).await.unwrap(), None);
        assert_eq!(good.load(&access_token_key(BIGMODEL)).await.unwrap(), None);
        assert_eq!(good.load("ssh:host").await.unwrap().as_deref(), Some("keep"));
    }

    #[test]
    fn helpers_match_node() {
        assert_eq!(encode_uri_component("a b/c:é~"), "a%20b%2Fc%3A%C3%A9~");
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"exp":100}"#);
        assert_eq!(
            jwt_expiration(&format!("h.{payload}.s"), 100_000 - 30_001),
            JwtExpiration::Valid(100_000)
        );
        assert_eq!(
            jwt_expiration(&format!("h.{payload}.s"), 100_000 - 30_000),
            JwtExpiration::Expired(100_000)
        );
        assert_eq!(jwt_expiration("opaque", 0), JwtExpiration::Unknown);
        let png = base64::engine::general_purpose::STANDARD.encode([0x89, 0x50, 0x4e, 0x47, 0, 0, 0, 0, 1, 2, 3, 4]);
        assert_eq!(
            normalize_avatar(Some(&png)).unwrap(),
            format!("data:image/png;base64,{png}")
        );
        assert_eq!(normalize_avatar(Some("  ")), None);
        assert_eq!(normalize_avatar(Some("short")).unwrap(), "short");
    }
}
