//! 官方 Server MCP 的凭证源（packages/services/src/official-mcp/officialMcpCredentials.ts、
//! packages/shared/src/official-mcp-auth.ts 的信任判定）。
//!
//! MCP 身份头与 MCP 额度查询共用这一份凭证解析：解析前后各取一次身份快照并比对，
//! 任何不一致整轮重来，绝不把两代凭证拼进同一请求。
use std::sync::{Arc, Mutex};

use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use serde_json::{Map, Value, json};
use url::Url;
use zcode_host_kit::log;

use super::auth::{AccountRequestAuth, oauth_provider_of};
use super::credential::CredentialStore;
use super::oauth_repo::{ACTIVE_PROVIDER_KEY, ZCODE_JWT_TOKEN_KEY, access_token_key};
use crate::providers::ProviderRuntime;

pub const DEV_TRUSTED_ORIGINS_ENV: &str = "ZCODE_OFFICIAL_MCP_DEV_TRUSTED_ORIGINS";

fn normalize_https_origin(candidate: &str) -> Option<String> {
    let url = Url::parse(candidate).ok()?;
    if !url.username().is_empty() || url.password().is_some() || url.scheme() != "https" {
        return None;
    }
    Some(url.origin().ascii_serialization())
}

fn normalize_loopback_origin(candidate: &str) -> Option<String> {
    let url = Url::parse(candidate).ok()?;
    if !url.username().is_empty() || url.password().is_some() || url.scheme() != "http" {
        return None;
    }
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    loopback.then(|| url.origin().ascii_serialization())
}

/// `isOfficialMcpOriginTrusted`：返回 (trusted, detail)。
pub fn is_origin_trusted(
    origin: &str,
    dev_trusted_raw: Option<&str>,
    zcode_api_origin: Option<&str>,
) -> (bool, &'static str) {
    let origin = origin.trim();
    if origin.is_empty() {
        return (false, "invalid_input");
    }
    if let Some(loopback) = normalize_loopback_origin(origin) {
        for candidate in dev_trusted_raw
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|candidate| !candidate.is_empty())
        {
            if normalize_loopback_origin(candidate).as_deref() == Some(loopback.as_str()) {
                return (true, "ok");
            }
        }
    }
    let Some(expected) = zcode_api_origin.and_then(normalize_https_origin) else {
        return (false, "zcode_origin_unresolved");
    };
    if normalize_https_origin(origin).as_deref() != Some(expected.as_str()) {
        return (false, "origin_mismatch");
    }
    (true, "ok")
}

/// 解析成功的凭证快照（只在 Host 内存活）。
#[derive(Debug, Clone, PartialEq)]
pub struct CredentialSnapshot {
    pub jwt: String,
    pub coding_plan_authorization: Option<String>,
    pub provider_family: String,
    /// 选中连接的产品 / 额度归属：`{targetType:"PERSONAL"}` 或带组织项目的 TEAM。
    pub plan_scope: Option<Value>,
    /// 实际发往 MCP 的 scope；ZAI Team 为 None。
    pub wire_scope: Option<Value>,
}

pub type Outcome = Result<CredentialSnapshot, &'static str>;

#[derive(Debug, Clone, PartialEq)]
struct Identity {
    active_provider: String,
    jwt: String,
    view: Value,
    fingerprint: String,
}

/// 进行中的凭据解析（并发调用共享同一结果）。
type InFlight = Shared<BoxFuture<'static, Result<Map<String, Value>, &'static str>>>;

#[derive(Clone)]
pub struct OfficialMcpCredentials {
    providers: ProviderRuntime,
    auth: AccountRequestAuth,
    credentials: CredentialStore,
    in_flight: Arc<Mutex<Option<InFlight>>>,
    last_log_key: Arc<Mutex<Option<String>>>,
}

fn jwt_expires_in_seconds(token: &str) -> Option<i64> {
    use base64::Engine;
    let payload = token.split('.').nth(1).filter(|p| !p.is_empty())?;
    let normalized = payload.replace('-', "+").replace('_', "/");
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    );
    let decoded: Value =
        serde_json::from_slice(&engine.decode(normalized.trim_end_matches('=')).ok()?).ok()?;
    let exp = decoded["exp"].as_f64().filter(|exp| exp.is_finite())?;
    Some((exp - super::oauth_adapters::now_ms() as f64 / 1000.0).round() as i64)
}

impl OfficialMcpCredentials {
    pub fn new(
        providers: ProviderRuntime,
        auth: AccountRequestAuth,
        credentials: CredentialStore,
    ) -> Self {
        Self {
            providers,
            auth,
            credentials,
            in_flight: Arc::new(Mutex::new(None)),
            last_log_key: Arc::new(Mutex::new(None)),
        }
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

    async fn identity(&self) -> Result<Identity, &'static str> {
        let (revision, resolved) = self.providers.snapshot().await;
        let view = resolved.model_selection_view(revision, None);
        let active = self.load_trimmed(ACTIVE_PROVIDER_KEY).await;
        let jwt = self.load_trimmed(ZCODE_JWT_TOKEN_KEY).await;
        if (active != "zai" && active != "bigmodel") || jwt.is_empty() {
            return Err("official_auth_unavailable");
        }
        let fingerprint =
            json!({"revision": view["revision"], "providers": view["providers"]}).to_string();
        Ok(Identity {
            active_provider: active,
            jwt,
            view,
            fingerprint,
        })
    }

    fn same(left: &Identity, right: &Identity) -> bool {
        left.active_provider == right.active_provider
            && left.jwt == right.jwt
            && left.fingerprint == right.fingerprint
    }

    /// Registry 里唯一的个人 / Team Coding Plan provider（`resolveSelectedProvider`）。
    fn selected_provider(view: &Value) -> Option<(String, Value)> {
        let candidates: Vec<(String, Value)> = view["providers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|provider| {
                let access = &provider["config"]["access"];
                // zcodeProviderAccountAccessSchema 是 strict：只有这四个键，entitled 必须是布尔。
                let strict = access.as_object().is_some_and(|object| {
                    object.keys().all(|key| {
                        ["type", "accountType", "mode", "entitled"].contains(&key.as_str())
                    })
                });
                let valid = strict
                    && access["type"] == "zhipu-account"
                    && access["entitled"].is_boolean()
                    && matches!(access["accountType"].as_str(), Some("zai" | "bigmodel"))
                    && matches!(
                        access["mode"].as_str(),
                        Some("individual-coding-plan" | "team-coding-plan")
                    );
                valid.then(|| {
                    (
                        provider["providerId"].as_str().unwrap_or("").to_owned(),
                        access.clone(),
                    )
                })
            })
            .collect();
        (candidates.len() == 1).then(|| candidates.into_iter().next().expect("one"))
    }

    /// `resolveOfficialMcpCredentials`。
    pub async fn resolve(&self) -> Outcome {
        for attempt in 0..2 {
            let identity = match self.identity().await {
                Ok(identity) => identity,
                Err(reason) => {
                    if attempt == 0 {
                        continue;
                    }
                    return Err(reason);
                }
            };
            let Some((provider_id, static_access)) = Self::selected_provider(&identity.view) else {
                match self.identity().await {
                    Ok(latest) if Self::same(&identity, &latest) => {
                        log::debug("[official-mcp] identity-only credentials resolved");
                        return Ok(CredentialSnapshot {
                            jwt: identity.jwt,
                            coding_plan_authorization: None,
                            provider_family: identity.active_provider,
                            plan_scope: None,
                            wire_scope: None,
                        });
                    }
                    _ => continue,
                }
            };
            let access = self.auth.resolve_access_current(&static_access).await;
            let family = static_access["accountType"].as_str().unwrap_or("");
            let mode = static_access["mode"].as_str().unwrap_or("");
            let expected_kind = if mode == "team-coding-plan" {
                "team-coding-plan"
            } else {
                "individual-coding-plan"
            };
            let Some(access_value) = access
                .clone()
                .filter(|access| access["family"] == family && access["planKind"] == expected_kind)
            else {
                return Err("official_auth_plan_required");
            };
            let (plan_scope, wire_scope) = if expected_kind == "team-coding-plan" {
                let team = json!({
                    "organizationId": access_value["organizationId"],
                    "projectId": access_value["projectId"],
                    "targetType": "TEAM",
                });
                let wire = (family == "bigmodel").then(|| team.clone());
                (Some(team), wire)
            } else {
                let personal = json!({"targetType": "PERSONAL"});
                (Some(personal.clone()), Some(personal))
            };
            if identity.active_provider != family {
                return Err("official_auth_unavailable");
            }
            let maas_key = access_token_key(oauth_provider_of(family));
            let coding_plan = self.load_trimmed(&maas_key).await;
            if coding_plan.is_empty() {
                log::warn(format!(
                    "[official-mcp] official mcp maas jwt missing providerFamily={family}"
                ));
                return Err("official_auth_unavailable");
            }
            let latest = self.identity().await;
            let latest_maas = self.load_trimmed(&maas_key).await;
            let latest_selected = latest
                .as_ref()
                .ok()
                .and_then(|latest| Self::selected_provider(&latest.view));
            let latest_access = match &latest_selected {
                Some((_, latest_static)) => self.auth.resolve_access_current(latest_static).await,
                None => None,
            };
            let stable = latest
                .as_ref()
                .is_ok_and(|latest| Self::same(&identity, latest))
                && latest_selected.is_some()
                && access == latest_access
                && coding_plan == latest_maas;
            if !stable {
                continue;
            }
            let exists = identity.view["providers"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|provider| provider["providerId"] == provider_id.as_str());
            if !exists {
                return Err("official_auth_plan_required");
            }
            let expires = jwt_expires_in_seconds(&coding_plan);
            let bucket = match expires {
                None => "unparsable".to_owned(),
                Some(seconds) if seconds <= 0 => "expired".to_owned(),
                Some(seconds) => format!("t{}", seconds / 3600),
            };
            let target = plan_scope
                .as_ref()
                .and_then(|scope| scope["targetType"].as_str())
                .unwrap_or("none");
            let log_key = format!("{family}|{target}|{bucket}");
            {
                let mut last = self.last_log_key.lock().unwrap_or_else(|p| p.into_inner());
                if last.as_deref() != Some(log_key.as_str()) {
                    *last = Some(log_key);
                    log::info(format!(
                        "[official-mcp] official mcp credentials resolved providerFamily={family} planTargetType={target} maasJwtExpiresInSeconds={expires:?}"
                    ));
                }
            }
            return Ok(CredentialSnapshot {
                jwt: identity.jwt,
                coding_plan_authorization: Some(coding_plan),
                provider_family: family.to_owned(),
                plan_scope,
                wire_scope,
            });
        }
        Err("official_auth_unavailable")
    }

    /// `buildOfficialMcpAuthHeaders`。
    pub fn headers(snapshot: &CredentialSnapshot) -> Map<String, Value> {
        let mut headers = Map::new();
        headers.insert(
            "Authorization".into(),
            json!(format!("Bearer {}", snapshot.jwt)),
        );
        if let Some(coding_plan) = snapshot
            .coding_plan_authorization
            .as_deref()
            .filter(|v| !v.is_empty())
        {
            headers.insert(
                "X-Bigmodel-Authorization".into(),
                json!(format!("Bearer {coding_plan}")),
            );
        }
        if let Some(scope) = &snapshot.wire_scope {
            headers.insert("Bigmodel-Target-Type".into(), scope["targetType"].clone());
            if scope["targetType"] == "TEAM" {
                headers.insert(
                    "Bigmodel-Organization".into(),
                    scope["organizationId"].clone(),
                );
                headers.insert("Bigmodel-Project".into(), scope["projectId"].clone());
            }
        }
        headers
    }

    /// `createOfficialMcpAuthHeadersResolver`：只合并并发请求，不做时间缓存。
    pub async fn resolve_headers(&self) -> Result<Map<String, Value>, &'static str> {
        let future = {
            let mut slot = self.in_flight.lock().unwrap_or_else(|p| p.into_inner());
            match slot.as_ref() {
                Some(existing) => existing.clone(),
                None => {
                    let this = self.clone();
                    let future = async move {
                        let outcome = this
                            .resolve()
                            .await
                            .map(|snapshot| Self::headers(&snapshot));
                        *this.in_flight.lock().unwrap_or_else(|p| p.into_inner()) = None;
                        outcome
                    }
                    .boxed()
                    .shared();
                    *slot = Some(future.clone());
                    future
                }
            }
        };
        future.await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_trust_matches_shared_rules() {
        assert_eq!(
            is_origin_trusted("https://zcode.z.ai/", None, Some("https://zcode.z.ai")),
            (true, "ok")
        );
        assert_eq!(
            is_origin_trusted("https://evil.example", None, Some("https://zcode.z.ai")),
            (false, "origin_mismatch")
        );
        assert_eq!(
            is_origin_trusted("http://127.0.0.1:9", Some("http://127.0.0.1:9 , x"), None),
            (true, "ok")
        );
        // 开关只放开 loopback 目标；远端 https 不能借空 dev 列表通过。
        assert_eq!(
            is_origin_trusted("https://evil.example", Some("http://127.0.0.1:9"), None),
            (false, "zcode_origin_unresolved")
        );
        assert_eq!(is_origin_trusted(" ", None, None), (false, "invalid_input"));
        let snapshot = CredentialSnapshot {
            jwt: "j".into(),
            coding_plan_authorization: Some("m".into()),
            provider_family: "bigmodel".into(),
            plan_scope: None,
            wire_scope: Some(
                json!({"targetType": "TEAM", "organizationId": "o", "projectId": "p"}),
            ),
        };
        assert_eq!(
            Value::Object(OfficialMcpCredentials::headers(&snapshot)),
            json!({"Authorization": "Bearer j", "X-Bigmodel-Authorization": "Bearer m", "Bigmodel-Target-Type": "TEAM", "Bigmodel-Organization": "o", "Bigmodel-Project": "p"})
        );
    }
}
