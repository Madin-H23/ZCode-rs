//! 账号请求鉴权与账号派生凭据（packages/services/src/model-provider/accountProvider*、
//! accountRequestAuthService.ts、bigmodel/teamPlanApiKey.ts、legacyTeamOrganizationResolver.ts 的
//! 读取部分）。
//!
//! 个人 Coding Plan key 按账号身份缓存在 credentials.json（`account-provider:coding-plan:...`），
//! Start Plan 用登录时落盘的 zcode JWT，Team Plan 每次按当前选择的项目远端解析。
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use serde_json::{Value, json};
use zcode_host_kit::log;

use super::credential::CredentialStore;
use super::net::{ApiClient, ApiError, ApiRequest};
use super::oauth_repo::{
    BIGMODEL, OAuthCredentialRepo, ZAI, ZCODE_JWT_TOKEN_KEY, access_token_key, encode_uri_component,
};
use crate::settings::SettingStore;

pub const ZAI_INDIVIDUAL_CODING_PLAN: &str = "account:zai-individual-coding-plan";
pub const BIGMODEL_INDIVIDUAL_CODING_PLAN: &str = "account:bigmodel-individual-coding-plan";
const TEAM_PLAN_RUNTIME_KEY_REQUEST_TIMEOUT_MS: u64 = 15_000;
const DEFAULT_ORG_NAME: &str = "默认机构";
const DEFAULT_PROJECT_NAME: &str = "默认项目";
const ZCODE_API_KEY_NAME: &str = "zcode-api-key";
const TEAM_PLAN_API_KEY_NAME: &str = "zcode-team-api-key";
const TEAM_PLAN_API_KEY_TYPE: i64 = 2;

pub fn oauth_provider_of(family: &str) -> &'static str {
    if family == "zai" { ZAI } else { BIGMODEL }
}

/// `accountProviderCredentialKey`。
pub fn credential_key(
    provider_id: &str,
    plan_kind: &str,
    account_identity: &str,
    team: Option<(&str, &str, &str)>,
) -> Result<String, String> {
    let required = |value: &str, label: &str| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            Err(format!("Account Provider 缺少{label}"))
        } else {
            Ok(trimmed.to_owned())
        }
    };
    let provider = required(provider_id, "Provider ID")?;
    let identity = required(account_identity, "账号身份")?;
    let scope = match (plan_kind, team) {
        ("team-coding-plan", Some((product, organization, project))) => [
            "team".to_owned(),
            provider,
            required(product, "Team Product ID")?,
            required(organization, "Team Organization ID")?,
            required(project, "Team Project ID")?,
        ]
        .iter()
        .map(|part| encode_uri_component(part))
        .collect::<Vec<_>>()
        .join(":"),
        ("start-plan", _) => format!("start-plan:{provider}"),
        _ => format!("coding-plan:{provider}"),
    };
    Ok(format!(
        "account-provider:{scope}:account:{}:api-key",
        encode_uri_component(&identity)
    ))
}

/// BigModel 业务接口成功码 0 或 200（字符串同样接受），缺失视为成功。
pub fn remote_code_ok(code: &Value) -> bool {
    match code {
        Value::Null => true,
        Value::Number(number) => number.as_f64().is_some_and(|n| n == 0.0 || n == 200.0),
        Value::String(text) => text == "0" || text == "200",
        _ => false,
    }
}

/// `isSuccessfulBigModelBizEnvelope`。
fn biz_envelope_ok(envelope: &Value) -> bool {
    if envelope["success"] == false {
        return false;
    }
    if let Some(code) = envelope["code"].as_f64() {
        return code == 0.0 || code == 200.0;
    }
    envelope["success"] == true || envelope.get("data").is_some()
}

fn biz_headers(request: ApiRequest, authorization: &str) -> ApiRequest {
    request
        .header("Authorization", authorization.to_owned())
        .header("Content-Type", "application/json")
}

fn team_headers(request: ApiRequest, authorization: &str, team: (&str, &str)) -> ApiRequest {
    biz_headers(request, authorization)
        .header("bigmodel-organization", team.0.to_owned())
        .header("bigmodel-project", team.1.to_owned())
}

/// `pickOrgAndProject`：个人 key 只从非团队项目解析。
pub fn pick_org_and_project(customer: &Value) -> Option<(String, String)> {
    let personal: Vec<(&Value, Vec<&Value>)> = customer["organizations"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|organization| {
            let projects: Vec<&Value> = organization["projects"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|project| project_type(project) != "2")
                .collect();
            (organization, projects)
        })
        .filter(|(organization, projects)| {
            organization["organizationId"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
                && !projects.is_empty()
        })
        .collect();
    let (organization, projects) = personal
        .iter()
        .find(|(organization, _)| {
            organization["organizationName"]
                .as_str()
                .unwrap_or("")
                .contains(DEFAULT_ORG_NAME)
        })
        .or_else(|| personal.first())?;
    let organization_id = organization["organizationId"].as_str()?.to_owned();
    let project = projects
        .iter()
        .find(|project| {
            project["projectName"]
                .as_str()
                .unwrap_or("")
                .contains(DEFAULT_PROJECT_NAME)
        })
        .or_else(|| projects.first())?;
    let project_id = project["projectId"].as_str().filter(|id| !id.is_empty())?;
    Some((organization_id, project_id.to_owned()))
}

/// `String(project.projectType ?? "").trim()`。
pub fn project_type(project: &Value) -> String {
    match &project["projectType"] {
        Value::String(text) => text.trim().to_owned(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        _ => String::new(),
    }
}

/// 账号派生凭据的远端解析（AccountProviderApiKeyResolver、teamPlanApiKey.ts）。
#[derive(Clone)]
pub struct AccountApi {
    api: ApiClient,
}

impl AccountApi {
    pub fn new(api: ApiClient) -> Self {
        Self { api }
    }

    pub fn client(&self) -> &ApiClient {
        &self.api
    }

    /// `AccountProviderApiClient.fetchRemoteData`：业务码失败返回 None。
    async fn fetch_remote_data(&self, request: ApiRequest) -> Result<Option<Value>, ApiError> {
        let payload = self.api.read_json(request).await?;
        if !remote_code_ok(&payload["code"]) {
            return Ok(None);
        }
        Ok(payload.get("data").filter(|data| !data.is_null()).cloned())
    }

    /// `resolveProviderApiKey`：个人 Coding Plan 的模型 API key；任何失败都返回 None。
    pub async fn resolve_provider_api_key(
        &self,
        provider: &str,
        access_token: &str,
    ) -> Option<String> {
        let endpoints = self.api.endpoints();
        let result = match provider {
            BIGMODEL => {
                self.resolve_biz_api_key(&endpoints.bigmodel_api_origin(), access_token, false)
                    .await
            }
            // Provider Connection 已把 Z.AI access token 持久化为业务 token。
            ZAI => {
                self.resolve_biz_api_key(
                    &endpoints.zai_business_base_url(),
                    &format!("Bearer {access_token}"),
                    true,
                )
                .await
            }
            _ => return None,
        };
        result.ok().flatten()
    }

    async fn resolve_biz_api_key(
        &self,
        host: &str,
        authorization: &str,
        require_secret: bool,
    ) -> Result<Option<String>, ApiError> {
        let Some(customer) = self
            .fetch_remote_data(biz_headers(
                ApiRequest::get(format!("{host}/api/biz/customer/getCustomerInfo")),
                authorization,
            ))
            .await?
        else {
            return Ok(None);
        };
        let Some((organization, project)) = pick_org_and_project(&customer) else {
            return Ok(None);
        };
        let list_url =
            format!("{host}/api/biz/v1/organization/{organization}/projects/{project}/api_keys");
        let keys = self
            .fetch_remote_data(biz_headers(ApiRequest::get(&list_url), authorization))
            .await?
            .unwrap_or(Value::Null);
        let mut entry = keys
            .as_array()
            .into_iter()
            .flatten()
            .find(|item| item["name"] == ZCODE_API_KEY_NAME)
            .cloned();
        if entry.is_none() {
            entry = self
                .fetch_remote_data(
                    biz_headers(ApiRequest::post(&list_url), authorization)
                        .json(&json!({"name": ZCODE_API_KEY_NAME})),
                )
                .await?;
        }
        let api_key = entry
            .as_ref()
            .and_then(|entry| entry["apiKey"].as_str())
            .map(str::trim)
            .unwrap_or("")
            .to_owned();
        if api_key.is_empty() {
            return Ok(None);
        }
        let copy_url = format!("{list_url}/copy/{}", encode_uri_component(&api_key));
        let secret = self
            .fetch_remote_data(biz_headers(ApiRequest::get(copy_url), authorization))
            .await?
            .and_then(|data| data["secretKey"].as_str().map(|s| s.trim().to_owned()))
            .unwrap_or_default();
        if secret.is_empty() {
            // Z.AI 必须用 copy 接口返回的 secretKey，裸 apiKey 不能用于模型鉴权。
            return Ok((!require_secret).then_some(api_key));
        }
        Ok(Some(format!("{api_key}.{secret}")))
    }

    fn team_keys_url(host: &str, organization: &str, project: &str) -> String {
        format!(
            "{host}/api/biz/v1/organization/{}/projects/{}/api_keys",
            encode_uri_component(organization),
            encode_uri_component(project)
        )
    }

    /// `ensureBigModelTeamPlanProjectApiKey`：已有 keyType=2 的项目 key 优先，否则创建。
    pub async fn ensure_team_project_api_key(
        &self,
        authorization: &str,
        host: &str,
        team: (&str, &str),
        timeout_ms: u64,
    ) -> Result<Option<Value>, ApiError> {
        let usable = |item: &Value| {
            item["name"] == TEAM_PLAN_API_KEY_NAME
                && item["keyType"].as_i64() == Some(TEAM_PLAN_API_KEY_TYPE)
                && item["apiKey"]
                    .as_str()
                    .is_some_and(|key| !key.trim().is_empty())
        };
        let url = Self::team_keys_url(host, team.0, team.1);
        let list = self
            .api
            .read_json(
                team_headers(ApiRequest::get(&url), authorization, team).timeout_ms(timeout_ms),
            )
            .await?;
        if biz_envelope_ok(&list)
            && let Some(existing) = list["data"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|item| usable(item))
        {
            return Ok(Some(existing.clone()));
        }
        let created = self
            .api
            .read_json(
                team_headers(ApiRequest::post(&url), authorization, team)
                    .timeout_ms(timeout_ms)
                    .json(
                        &json!({"name": TEAM_PLAN_API_KEY_NAME, "keyType": TEAM_PLAN_API_KEY_TYPE}),
                    ),
            )
            .await?;
        Ok(biz_envelope_ok(&created)
            .then(|| created["data"].clone())
            .filter(|data| usable(data)))
    }

    pub async fn copy_team_project_api_key_secret(
        &self,
        authorization: &str,
        api_key: &str,
        host: &str,
        team: (&str, &str),
        timeout_ms: u64,
    ) -> Result<Option<String>, ApiError> {
        let url = format!(
            "{}/copy/{}",
            Self::team_keys_url(host, team.0, team.1),
            encode_uri_component(api_key)
        );
        let payload = self
            .api
            .read_json(
                team_headers(ApiRequest::get(url), authorization, team).timeout_ms(timeout_ms),
            )
            .await?;
        Ok(biz_envelope_ok(&payload)
            .then(|| {
                payload["data"]["secretKey"]
                    .as_str()
                    .map(|s| s.trim().to_owned())
            })
            .flatten()
            .filter(|secret| !secret.is_empty()))
    }

    /// `resolveAccountTeamPlanRuntimeApiKey`。
    pub async fn resolve_team_plan_runtime_api_key(
        &self,
        credentials: &CredentialStore,
        family: &str,
        organization: &str,
        project: &str,
    ) -> Result<Option<String>, ApiError> {
        let provider = oauth_provider_of(family);
        let token = credentials
            .load(&access_token_key(provider))
            .await
            .ok()
            .flatten()
            .map(|token| token.trim().to_owned())
            .unwrap_or_default();
        if token.is_empty() {
            log::warn(format!(
                "[account-provider-team-plan-request-key] Team Plan runtime key projection skipped: OAuth token missing family={family}"
            ));
            return Ok(None);
        }
        let jwt = credentials
            .load(ZCODE_JWT_TOKEN_KEY)
            .await
            .ok()
            .flatten()
            .map(|jwt| jwt.trim().to_owned())
            .unwrap_or_default();
        if family == "bigmodel" && !jwt.is_empty() && token == jwt {
            log::warn(
                "[account-provider-team-plan-request-key] Team Plan runtime key projection skipped: stale zcode JWT token",
            );
            return Ok(None);
        }
        let endpoints = self.api.endpoints();
        let host = if family == "zai" {
            endpoints.zai_business_base_url()
        } else {
            endpoints.bigmodel_api_origin()
        };
        // 两个业务域都直接传登录 token（不带 Bearer）。
        let customer = self
            .api
            .read_json(
                biz_headers(
                    ApiRequest::get(format!("{host}/api/biz/customer/getCustomerInfo")),
                    &token,
                )
                .timeout_ms(TEAM_PLAN_RUNTIME_KEY_REQUEST_TIMEOUT_MS),
            )
            .await?;
        let has_project = customer["data"]["organizations"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|item| item["organizationId"].as_str().map(str::trim) == Some(organization))
            .is_some_and(|org| {
                org["projects"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|item| item["projectId"].as_str().map(str::trim) == Some(project))
            });
        if !has_project {
            log::warn(format!(
                "[account-provider-team-plan-request-key] Team Plan runtime key projection skipped: project not found in customer info family={family}"
            ));
            return Ok(None);
        }
        let team = (organization, project);
        let entry = self
            .ensure_team_project_api_key(
                &token,
                &host,
                team,
                TEAM_PLAN_RUNTIME_KEY_REQUEST_TIMEOUT_MS,
            )
            .await?;
        let api_key = entry
            .as_ref()
            .and_then(|entry| entry["apiKey"].as_str())
            .map(str::trim)
            .unwrap_or("")
            .to_owned();
        if api_key.is_empty() {
            return Ok(None);
        }
        let secret = self
            .copy_team_project_api_key_secret(
                &token,
                &api_key,
                &host,
                team,
                TEAM_PLAN_RUNTIME_KEY_REQUEST_TIMEOUT_MS,
            )
            .await?;
        Ok(Some(match secret {
            Some(secret) => format!("{api_key}.{secret}"),
            None => api_key,
        }))
    }
}

type KeyFuture = Shared<BoxFuture<'static, Option<String>>>;

/// 个人 Coding Plan key：缓存在凭据库，缺失或强制刷新时远端解析；同 key 并发合并。
#[derive(Clone)]
pub struct CodingPlanKeys {
    credentials: CredentialStore,
    repo: OAuthCredentialRepo,
    api: AccountApi,
    in_flight: Arc<Mutex<HashMap<String, KeyFuture>>>,
}

fn normalize_secret(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

impl CodingPlanKeys {
    pub fn new(credentials: CredentialStore, repo: OAuthCredentialRepo, api: AccountApi) -> Self {
        Self {
            credentials,
            repo,
            api,
            in_flight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn load(
        &self,
        provider_id: &str,
        family: &str,
        identity: &str,
        force_refresh: bool,
    ) -> Option<String> {
        let key = credential_key(provider_id, "individual-coding-plan", identity, None).ok()?;
        let operation = format!("{}:{key}", if force_refresh { "refresh" } else { "load" });
        let future = {
            let mut in_flight = self
                .in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(existing) = in_flight.get(&operation) {
                existing.clone()
            } else {
                let this = self.clone();
                let family = family.to_owned();
                let credential_key = key.clone();
                let operation_key = operation.clone();
                let future = async move {
                    let result = this
                        .load_uncached(&credential_key, &family, force_refresh)
                        .await;
                    this.in_flight
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&operation_key);
                    result
                }
                .boxed()
                .shared();
                in_flight.insert(operation, future.clone());
                future
            }
        };
        future.await
    }

    async fn load_uncached(&self, key: &str, family: &str, force_refresh: bool) -> Option<String> {
        if !force_refresh
            && let Some(cached) = normalize_secret(self.credentials.load(key).await.ok().flatten())
        {
            return Some(cached);
        }
        let access = normalize_secret(
            self.repo
                .token_set(oauth_provider_of(family))
                .await
                .ok()
                .flatten()
                .map(|tokens| tokens.access_token),
        )?;
        let api_key = normalize_secret(
            self.api
                .resolve_provider_api_key(oauth_provider_of(family), &access)
                .await,
        )?;
        let _ = self.credentials.save(key, &api_key).await;
        Some(api_key)
    }

    /// 退出时删除该账号的个人 Coding Plan key（`createOAuthProviderLogoutHandler`）。
    pub async fn delete_for_logout(&self, provider: &str, identity: &str) {
        let coding_plan = match provider {
            ZAI => ZAI_INDIVIDUAL_CODING_PLAN,
            BIGMODEL => BIGMODEL_INDIVIDUAL_CODING_PLAN,
            _ => return,
        };
        if identity.trim().is_empty() {
            return;
        }
        if let Ok(key) = credential_key(coding_plan, "individual-coding-plan", identity, None)
            && let Err(error) = self.credentials.delete(&key).await
        {
            log::warn(format!(
                "[oauthProviderLogout] delete coding plan key failed: {error}"
            ));
        }
    }
}

/// 账号连接设置（`AccountProviderConnectionSettings`）。
#[derive(Debug, Clone, PartialEq)]
pub struct AccountSettings {
    pub domain: Option<String>,
    pub selections: Value,
    pub unresolved_families: Vec<String>,
}

impl AccountSettings {
    pub fn selection(&self, family: &str) -> &Value {
        &self.selections[family]
    }
}

/// 读取账号连接设置；旧版只有两段的 Team 连接还缺组织 ID，对应 family 标为未解析。
pub async fn read_account_settings(settings: &SettingStore) -> AccountSettings {
    let current = settings.get().await;
    let raw = tokio::fs::read_to_string(settings.file())
        .await
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or(Value::Null);
    let unresolved_families = ["zai", "bigmodel"]
        .into_iter()
        .filter(|family| {
            let mut probe = serde_json::Map::new();
            for key in [
                "modelProviderFamilyModes",
                "modelProviderFamilySelectedKeys",
            ] {
                if let Some(value) = raw[key].get(*family) {
                    probe.insert(key.to_owned(), json!({ *family: value }));
                }
            }
            raw.get("providerFamilyConnectionSelections").is_none()
                && crate::settings::schema::has_incomplete_legacy_team_connections(&Value::Object(
                    probe,
                ))
        })
        .map(str::to_owned)
        .collect();
    AccountSettings {
        domain: current["providerFamilyDomain"].as_str().map(str::to_owned),
        selections: if current["providerFamilyConnectionSelections"].is_object() {
            current["providerFamilyConnectionSelections"].clone()
        } else {
            json!({})
        },
        unresolved_families,
    }
}

/// `resolveCurrentAccountAccess`：静态 access（family + mode）→ 当前账号的动态访问事实。
pub fn resolve_current_access(
    access: &Value,
    settings: &AccountSettings,
    identity: Option<&str>,
) -> Option<Value> {
    let family = access["accountType"].as_str()?;
    let mode = access["mode"].as_str()?;
    if settings.domain.as_deref() != Some(family) {
        return None;
    }
    let has_identity = identity.is_some_and(|id| !id.trim().is_empty());
    if mode == "start-plan" {
        return has_identity
            .then(|| json!({"type": "zhipu-account", "family": family, "planKind": "start-plan"}));
    }
    let selection = settings.selection(family);
    let kind = selection["kind"].as_str()?;
    if mode == "off-peak" {
        if kind != "individual-coding-plan" && kind != "team-coding-plan" {
            return None;
        }
    } else if kind != mode {
        return None;
    }
    if !has_identity {
        return None;
    }
    if kind == "team-coding-plan" {
        return Some(json!({
            "type": "zhipu-account",
            "family": family,
            "planKind": kind,
            "productId": selection["productId"],
            "organizationId": selection["organizationId"],
            "projectId": selection["projectId"],
        }));
    }
    Some(json!({"type": "zhipu-account", "family": family, "planKind": kind}))
}

/// `AccountRequestCredentialUnavailableError`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialUnavailable(pub String);

impl std::fmt::Display for CredentialUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Account request credential is unavailable: {}", self.0)
    }
}

/// 请求期账号鉴权（`AccountProviderRequestAuthService`）。
#[derive(Clone)]
pub struct AccountRequestAuth {
    settings: SettingStore,
    repo: OAuthCredentialRepo,
    keys: CodingPlanKeys,
    api: AccountApi,
}

impl AccountRequestAuth {
    pub fn new(
        settings: SettingStore,
        repo: OAuthCredentialRepo,
        keys: CodingPlanKeys,
        api: AccountApi,
    ) -> Self {
        Self {
            settings,
            repo,
            keys,
            api,
        }
    }

    pub fn repo(&self) -> &OAuthCredentialRepo {
        &self.repo
    }

    pub async fn account_identity(&self, family: &str) -> Option<String> {
        self.repo
            .user_profile(oauth_provider_of(family))
            .await
            .ok()
            .flatten()
            .map(|profile| profile.id)
            .filter(|id| !id.trim().is_empty())
    }

    pub async fn resolve_access_current(&self, access: &Value) -> Option<Value> {
        let settings = read_account_settings(&self.settings).await;
        let family = access["accountType"].as_str().unwrap_or("");
        let identity = self.account_identity(family).await;
        resolve_current_access(access, &settings, identity.as_deref())
    }

    async fn resolve_access(&self, access: &Value) -> Option<Value> {
        if access.get("mode").is_some() {
            self.resolve_access_current(access).await
        } else {
            Some(access.clone())
        }
    }

    /// `resolveCurrent`：返回 `{apiKey}`；没有可用凭据时报 CredentialUnavailable。
    pub async fn resolve_current(
        &self,
        provider_id: &str,
        account_access: &Value,
    ) -> Result<String, CredentialUnavailable> {
        let provider_id = provider_id.trim().to_owned();
        let unavailable = || CredentialUnavailable(provider_id.clone());
        let access = self
            .resolve_access(account_access)
            .await
            .ok_or_else(unavailable)?;
        let family = access["family"].as_str().unwrap_or("");
        let key = match access["planKind"].as_str() {
            Some("start-plan") => self
                .repo
                .token_set(oauth_provider_of(family))
                .await
                .ok()
                .flatten()
                .and_then(|tokens| tokens.zcode_jwt_token),
            Some("individual-coding-plan") => match self.account_identity(family).await {
                Some(identity) => self.keys.load(&provider_id, family, &identity, false).await,
                None => None,
            },
            _ => self
                .api
                .resolve_team_plan_runtime_api_key(
                    self.repo.credentials(),
                    family,
                    access["organizationId"].as_str().unwrap_or(""),
                    access["projectId"].as_str().unwrap_or(""),
                )
                .await
                .ok()
                .flatten(),
        };
        normalize_secret(key).ok_or_else(unavailable)
    }

    pub async fn assert_current(
        &self,
        provider_id: &str,
        account_access: &Value,
    ) -> Result<(), CredentialUnavailable> {
        self.resolve_access(account_access)
            .await
            .map(|_| ())
            .ok_or_else(|| CredentialUnavailable(provider_id.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_keys_match_node() {
        assert_eq!(
            credential_key(
                "account:zai-individual-coding-plan",
                "individual-coding-plan",
                "u 1",
                None
            )
            .unwrap(),
            "account-provider:coding-plan:account:zai-individual-coding-plan:account:u%201:api-key"
        );
        assert_eq!(
            credential_key("p", "team-coding-plan", "u", Some(("pr", "o:1", "x"))).unwrap(),
            "account-provider:team:p:pr:o%3A1:x:account:u:api-key"
        );
        assert!(credential_key("p", "start-plan", " ", None).is_err());
    }

    #[test]
    fn org_and_project_prefer_defaults_and_skip_team_projects() {
        let customer = json!({"organizations": [
            {"organizationId": "o1", "organizationName": "Other", "projects": [{"projectId": "p0", "projectType": 2}]},
            {"organizationId": "o2", "organizationName": "Acme", "projects": [{"projectId": "p1"}, {"projectId": "p2", "projectName": "默认项目"}]},
            {"organizationId": "o3", "organizationName": "我的默认机构", "projects": [{"projectId": "p3", "projectType": "1"}]},
        ]});
        assert_eq!(
            pick_org_and_project(&customer),
            Some(("o3".into(), "p3".into()))
        );
        let no_default = json!({"organizations": [{"organizationId": "o2", "projects": [{"projectId": "p1"}, {"projectId": "p2", "projectName": "默认项目"}]}]});
        assert_eq!(
            pick_org_and_project(&no_default),
            Some(("o2".into(), "p2".into()))
        );
    }

    #[test]
    fn current_access_follows_domain_and_selection() {
        let settings = AccountSettings {
            domain: Some("zai".into()),
            selections: json!({"zai": {"kind": "team-coding-plan", "productId": "pr", "organizationId": "o", "projectId": "p"}}),
            unresolved_families: vec![],
        };
        let team =
            json!({"type": "zhipu-account", "accountType": "zai", "mode": "team-coding-plan"});
        assert_eq!(
            resolve_current_access(&team, &settings, Some("u")).unwrap()["projectId"],
            "p"
        );
        assert!(resolve_current_access(&team, &settings, None).is_none());
        let offpeak = json!({"type": "zhipu-account", "accountType": "zai", "mode": "off-peak"});
        assert_eq!(
            resolve_current_access(&offpeak, &settings, Some("u")).unwrap()["planKind"],
            "team-coding-plan"
        );
        let start = json!({"type": "zhipu-account", "accountType": "zai", "mode": "start-plan"});
        assert_eq!(
            resolve_current_access(&start, &settings, Some("u")).unwrap(),
            json!({"type": "zhipu-account", "family": "zai", "planKind": "start-plan"})
        );
        let bigmodel =
            json!({"type": "zhipu-account", "accountType": "bigmodel", "mode": "start-plan"});
        assert!(resolve_current_access(&bigmodel, &settings, Some("u")).is_none());
    }
}
