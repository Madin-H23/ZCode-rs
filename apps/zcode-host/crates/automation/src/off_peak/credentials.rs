//! 闲时双凭证解析（offPeakRuntimeModel.ts 的 `resolveOffPeakCredentials` 与 node.ts 的
//! `resolveAccountProvider`；docs/specs/rust-host-integration.md §2.1）。
//!
//! 唯一入口 [`CredentialSource::resolve`]：取号客户端、`getCodingPlanSupport`、模型选择与
//! `off-peak-run` 派发的逐请求鉴权都调它；不缓存，每次按当前账号状态解析。账号事实
//! （OAuth 凭据、Coding Plan key、账号访问）的所有者是 account 区域。
use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::models::{BIGMODEL_INDIVIDUAL_CODING_PLAN, mock_enabled, mock_no_plan};
use crate::account::auth::AccountRequestAuth;
use crate::account::credential::CredentialStore;
use crate::account::oauth_repo::{ACTIVE_PROVIDER_KEY, ZCODE_JWT_TOKEN_KEY};
use crate::providers::ProviderRuntime;

/// 一次凭据解析的结果（`OffPeakCredentialSnapshot`）。
#[derive(Debug, Clone, PartialEq)]
pub struct Credentials {
    pub jwt: String,
    pub coding_plan_api_key: String,
    /// `OffPeakCodingPlanKind`：`zai-personal` / `bigmodel-personal` / `zai-team` / `bigmodel-team`。
    pub kind: &'static str,
    pub provider_family: &'static str,
    pub provider_id: String,
    /// BigModel Team 的组织 / 项目；两者都有才进入请求头。
    pub organization_id: Option<String>,
    pub project_id: Option<String>,
    /// 选中 Coding Plan 的上游 baseURL（只供 mock 网关代理用；本 Host 的 mock 不代理）。
    pub provider_base_url: Option<String>,
}

/// `OffPeakCredentialsUnavailableError.missing`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    Jwt,
    CodingPlanApiKey,
}

/// 凭据不可用。前两类是 Node 的 `OffPeakPermanentDispatchError` 子类（派发时为 permanent）。
#[derive(Debug, Clone, PartialEq)]
pub enum Unavailable {
    /// `OffPeakCodingPlanUnavailableError(reason)`。
    CodingPlan(&'static str),
    /// `OffPeakCredentialsUnavailableError(missing)`。
    Credentials(Missing),
    /// 读取凭据库失败等非分类错误（Node 原样抛出）。
    Failed(String),
}

impl Unavailable {
    /// `resolveOffPeakCodingPlanSupport` 的 `reason`；非分类错误返回 None（Node 重新抛出）。
    pub fn support_reason(&self) -> Option<&'static str> {
        match self {
            Unavailable::CodingPlan(reason) => Some(reason),
            Unavailable::Credentials(Missing::Jwt) => Some("jwt_missing"),
            Unavailable::Credentials(Missing::CodingPlanApiKey) => Some("connection_unavailable"),
            Unavailable::Failed(_) => None,
        }
    }

    /// 派发时是否为 permanent（`OffPeakPermanentDispatchError`）。
    pub fn permanent(&self) -> bool {
        !matches!(self, Unavailable::Failed(_))
    }
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unavailable::CodingPlan(reason) => {
                write!(
                    formatter,
                    "off-peak selected coding plan unavailable: {reason}"
                )
            }
            Unavailable::Credentials(Missing::Jwt) => {
                formatter.write_str("off-peak requires zcode login (jwt missing)")
            }
            Unavailable::Credentials(Missing::CodingPlanApiKey) => {
                formatter.write_str("off-peak requires a coding plan provider api key")
            }
            Unavailable::Failed(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for Unavailable {}

/// 真实凭据链的依赖（account 区域所有）。
#[derive(Clone)]
pub struct AccountDeps {
    pub credentials: CredentialStore,
    pub auth: AccountRequestAuth,
    pub providers: ProviderRuntime,
}

/// `resolveAccountProvider` 的结果。
#[derive(Debug, Clone, PartialEq)]
struct SelectedProvider {
    provider_id: String,
    /// `accountRequestAuthService.resolveAccessCurrent` 给出的当前访问事实。
    access: Value,
    base_url: Option<String>,
}

impl SelectedProvider {
    /// `createOffPeakSelectionFingerprint`。
    fn fingerprint(provider: Option<&Self>) -> String {
        match provider {
            None => "null".to_owned(),
            Some(provider) => json!({
                "providerId": provider.provider_id,
                "access": provider.access,
                "baseURL": provider.base_url,
            })
            .to_string(),
        }
    }
}

/// `resolveSelectedOffPeakCodingPlan` 的结果。
struct Selection {
    kind: &'static str,
    family: &'static str,
    team: bool,
}

/// `zcodeProviderAccountAccessSchema`（strict）的个人 / Team Coding Plan 静态 access。
fn coding_plan_access(access: &Value) -> bool {
    let strict = access.as_object().is_some_and(|object| {
        object
            .keys()
            .all(|key| ["type", "accountType", "mode", "entitled"].contains(&key.as_str()))
    });
    strict
        && access["type"] == "zhipu-account"
        && access["entitled"].is_boolean()
        && matches!(access["accountType"].as_str(), Some("zai" | "bigmodel"))
        && matches!(
            access["mode"].as_str(),
            Some("individual-coding-plan" | "team-coding-plan")
        )
}

fn family_of(access: &Value) -> Option<&'static str> {
    match access["family"].as_str() {
        Some("zai") => Some("zai"),
        Some("bigmodel") => Some("bigmodel"),
        _ => None,
    }
}

/// `resolveSelectedOffPeakCodingPlan`。
fn selection_of(provider: Option<&SelectedProvider>) -> Result<Selection, Unavailable> {
    let Some(provider) = provider else {
        return Err(Unavailable::CodingPlan("connection_unavailable"));
    };
    let access = &provider.access;
    let Some(family) = family_of(access) else {
        return Err(Unavailable::CodingPlan("connection_unavailable"));
    };
    match access["planKind"].as_str() {
        Some("start-plan") => Err(Unavailable::CodingPlan("start_plan_not_supported")),
        Some("individual-coding-plan") => Ok(Selection {
            kind: if family == "zai" {
                "zai-personal"
            } else {
                "bigmodel-personal"
            },
            family,
            team: false,
        }),
        Some("team-coding-plan") => Ok(Selection {
            kind: if family == "zai" {
                "zai-team"
            } else {
                "bigmodel-team"
            },
            family,
            team: true,
        }),
        _ => Err(Unavailable::CodingPlan("connection_unavailable")),
    }
}

fn optional_text(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned)
}

#[derive(Clone)]
pub struct CredentialSource {
    vars: BTreeMap<String, String>,
    account: Option<AccountDeps>,
}

impl CredentialSource {
    /// `account` 为空时（单元测试、未接账号栈）真实链按“没有选中的 Coding Plan”处理。
    pub fn new(vars: BTreeMap<String, String>, account: Option<AccountDeps>) -> Self {
        Self { vars, account }
    }

    /// `resolveOffPeakCredentials`。
    pub async fn resolve(&self) -> Result<Credentials, Unavailable> {
        if mock_enabled(&self.vars) {
            if mock_no_plan(&self.vars) {
                return Err(Unavailable::CodingPlan("connection_unavailable"));
            }
            // mock 网关不校验凭证；确定性 metadata 让 UI 与 ticket/runtime 共享同一 support 形状。
            return Ok(Credentials {
                jwt: "offpeak-mock-jwt".into(),
                coding_plan_api_key: "offpeak-mock-key".into(),
                kind: "bigmodel-personal",
                provider_family: "bigmodel",
                provider_id: BIGMODEL_INDIVIDUAL_CODING_PLAN.into(),
                organization_id: None,
                project_id: None,
                provider_base_url: None,
            });
        }
        let Some(account) = &self.account else {
            return Err(Unavailable::CodingPlan("connection_unavailable"));
        };
        // settings 可能在解析期间切换：前后指纹不一致时重读一次，禁止拼接两代凭证。
        for _ in 0..2 {
            let provider = account.selected_provider().await;
            let selection = selection_of(provider.as_ref())?;
            let active = account.load_trimmed(ACTIVE_PROVIDER_KEY).await?;
            if active != selection.family {
                // zcode JWT 是当前 App 登录身份的全局镜像；不同 family 的 JWT 与 key 不能拼在一起。
                return Err(Unavailable::CodingPlan("provider_identity_mismatch"));
            }
            let jwt = account.load_trimmed(ZCODE_JWT_TOKEN_KEY).await?;
            if jwt.is_empty() {
                return Err(Unavailable::Credentials(Missing::Jwt));
            }
            let latest = account.selected_provider().await;
            let latest_active = account.load_trimmed(ACTIVE_PROVIDER_KEY).await?;
            if SelectedProvider::fingerprint(provider.as_ref())
                != SelectedProvider::fingerprint(latest.as_ref())
                || active != latest_active
            {
                continue;
            }
            let Some(provider) = provider else {
                return Err(Unavailable::CodingPlan("connection_unavailable"));
            };
            let key = account
                .auth
                .resolve_current(&provider.provider_id, &provider.access)
                .await
                .map(|key| key.trim().to_owned())
                .unwrap_or_default();
            if key.is_empty() {
                return Err(Unavailable::Credentials(Missing::CodingPlanApiKey));
            }
            let (organization_id, project_id) = if selection.team {
                (
                    optional_text(&provider.access["organizationId"]),
                    optional_text(&provider.access["projectId"]),
                )
            } else {
                (None, None)
            };
            return Ok(Credentials {
                jwt,
                coding_plan_api_key: key,
                kind: selection.kind,
                provider_family: selection.family,
                provider_id: provider.provider_id,
                organization_id,
                project_id,
                provider_base_url: provider.base_url,
            });
        }
        Err(Unavailable::CodingPlan("selection_changed"))
    }

    /// `resolveOffPeakCodingPlanSupport`：renderer 可见的脱敏快照；非分类错误原样返回。
    pub async fn support(&self) -> Result<Value, String> {
        match self.resolve().await {
            Ok(credentials) => Ok(json!({
                "supported": true,
                "kind": credentials.kind,
                "providerFamily": credentials.provider_family,
                "providerId": credentials.provider_id,
            })),
            Err(error) => match error.support_reason() {
                Some(reason) => Ok(json!({"supported": false, "reason": reason})),
                None => Err(error.to_string()),
            },
        }
    }
}

impl AccountDeps {
    async fn load_trimmed(&self, key: &str) -> Result<String, Unavailable> {
        self.credentials
            .load(key)
            .await
            .map(|value| {
                value
                    .map(|value| value.trim().to_owned())
                    .unwrap_or_default()
            })
            .map_err(|error| Unavailable::Failed(error.to_string()))
    }

    /// `resolveAccountProvider`：Registry 里唯一的个人 / Team Coding Plan 账号 provider，
    /// 静态 access 换成当前账号的访问事实；任一步不满足即为没有。
    async fn selected_provider(&self) -> Option<SelectedProvider> {
        let (_, resolved) = self.providers.snapshot().await;
        let candidates: Vec<_> = resolved
            .providers
            .iter()
            .filter(|provider| {
                let access = &provider.effective_config["access"];
                provider.in_registry()
                    && access["type"] == "zhipu-account"
                    && matches!(
                        access["mode"].as_str(),
                        Some("individual-coding-plan" | "team-coding-plan")
                    )
            })
            .collect();
        let [provider] = candidates.as_slice() else {
            return None;
        };
        let static_access = &provider.effective_config["access"];
        if !coding_plan_access(static_access) {
            return None;
        }
        let access = self.auth.resolve_access_current(static_access).await?;
        Some(SelectedProvider {
            provider_id: provider.provider_id.clone(),
            access,
            base_url: provider.effective_config["api"]["baseUrl"]
                .as_str()
                .filter(|url| !url.is_empty())
                .map(str::to_owned),
        })
    }
}

/// `buildOffPeakPlanIdentityHeaders`：只有 BigModel Team 且组织与项目都有时才发。
pub fn plan_identity_headers(credentials: &Credentials) -> Vec<(&'static str, String)> {
    if credentials.kind != "bigmodel-team" {
        return Vec::new();
    }
    let organization = credentials
        .organization_id
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();
    let project = credentials
        .project_id
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();
    if organization.is_empty() || project.is_empty() {
        return Vec::new();
    }
    vec![
        ("bigmodel-organization", organization.to_owned()),
        ("bigmodel-project", project.to_owned()),
    ]
}

#[cfg(test)]
mod tests;
