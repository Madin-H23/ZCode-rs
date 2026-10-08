//! `usage-stats` channel（packages/services/src/usage-stats/**）。
//!
//! 额度 / 套餐快照（`getEntitlementSnapshot`，Composer 工具栏与设置页）、Coding Plan 重置
//! （状态、领取资格、使用、已读）、App 用量（经 CLI `v4/usage/stats`）、BigModel monitor 用量
//! （`getSnapshot`、`getCodingPlanUsageSnapshot`，映射见 `usage_monitor.rs`）。
use std::sync::Arc;

use chrono::{Local, NaiveDateTime, TimeZone};
use serde_json::{Map, Value, json};
use zcode_host_kit::{Reply, RpcError, ServiceBuilder, log};

use super::auth::{AccountRequestAuth, oauth_provider_of};
use super::availability::{
    Entitlement, StartPlanBilling, fetch_personal_entitlement, fetch_team_entitlement, js_number,
};
use super::credential::CredentialStore;
use super::net::{ApiClient, ApiError, ApiRequest};
use super::oauth_adapters::now_ms;
use super::oauth_repo::ZCODE_JWT_TOKEN_KEY;
use super::official_mcp::OfficialMcpCredentials;

const REQUEST_TIMEOUT_MS: u64 = 15_000;
const BIGMODEL_QUOTA_PATH: &str = "/api/monitor/usage/quota/limit";
const CODING_PLAN_RESET_BASE_PATH: &str = "/api/v1/coding-plan/reset";
const MCP_USAGE_PATH: &str = "/api/v1/mcp/usage";
const MCP_USAGE_QUOTA_LIMIT_TYPE: &str = "MCP_USAGE_LIMIT";

pub fn is_zai_coding_plan_provider(id: &str) -> bool {
    matches!(
        id,
        "account:zai-individual-coding-plan"
            | "account:zai-team-coding-plan"
            | "account:zai-start-plan"
    )
}

pub fn is_start_plan_provider(id: &str) -> bool {
    matches!(id, "account:zai-start-plan" | "account:bigmodel-start-plan")
}

pub fn is_coding_plan_provider(id: &str) -> bool {
    is_zai_coding_plan_provider(id)
        || matches!(
            id,
            "account:bigmodel-individual-coding-plan"
                | "account:bigmodel-team-coding-plan"
                | "account:bigmodel-start-plan"
        )
}

fn provider_label(id: &str) -> &'static str {
    if id == "account:zai-individual-coding-plan" || id == "account:zai-start-plan" {
        "Z.ai - Coding Plan"
    } else {
        "BigModel - Coding Plan"
    }
}

fn error(message: impl Into<String>) -> RpcError {
    RpcError::new("Error", message)
}

fn api_error(failure: ApiError) -> RpcError {
    let mut rpc = RpcError::new("ApiError", failure.message);
    if let Some(status) = failure.status {
        rpc = rpc.with_data(json!({"status": status}));
    }
    rpc
}

fn non_empty(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// `parseNumber`：数字或非空数字字符串，非有限值为 None。
fn parse_number(value: &Value) -> Option<f64> {
    let number = match value {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) if !text.trim().is_empty() => js_number(value),
        _ => return None,
    };
    number.is_finite().then_some(number)
}

fn parse_unix_seconds(value: &Value) -> Option<f64> {
    parse_number(value).filter(|number| *number > 0.0)
}

fn iso_from_seconds(value: &Value) -> Option<String> {
    let seconds = parse_unix_seconds(value)?;
    Some(super::onboarding::iso_from_ms((seconds * 1000.0) as i64))
}

/// `json!` 对 f64 整数值输出 `1.0`；与 JS 一致地把整数写成整数。
fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9.0e15 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

/// `parseBigModelLocalDateTime`：无时区的 `YYYY-MM-DD HH:mm:ss` 按本机时区解析。
fn parse_local_date_time(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = if trimmed.contains('T') || !trimmed.contains(' ') {
        trimmed.to_owned()
    } else {
        trimmed.replacen(' ', "T", 1)
    };
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(&normalized) {
        return Some(super::onboarding::iso_from_ms(parsed.timestamp_millis()));
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(&normalized, format) {
            let local = Local.from_local_datetime(&naive).earliest()?;
            return Some(super::onboarding::iso_from_ms(local.timestamp_millis()));
        }
    }
    // 只有日期：JS 按 UTC 午夜解析。
    if let Ok(date) = chrono::NaiveDate::parse_from_str(&normalized, "%Y-%m-%d") {
        let utc = date.and_hms_opt(0, 0, 0)?.and_utc();
        return Some(super::onboarding::iso_from_ms(utc.timestamp_millis()));
    }
    None
}

/// `pickCurrentSubscriptionFromList` 的单条映射。
fn subscription_summary(item: &Value) -> Option<Value> {
    let product_id = item["productId"].as_str().filter(|id| !id.is_empty())?;
    let auto_renew = item["autoRenew"] == true || item["autoRenew"] == 1;
    let next_renew = parse_local_date_time(item["nextRenewTime"].as_str());
    let renew_time = next_renew.clone().filter(|_| auto_renew);
    let expire_time = match next_renew.filter(|_| !auto_renew) {
        Some(time) => Some(time),
        None => {
            // `valid` 里最后一个日期（时间）片段是有效期结束。
            let valid = item["valid"].as_str().unwrap_or("");
            let pattern = regex::Regex::new(r"\d{4}-\d{2}-\d{2}(?:[ T]\d{2}:\d{2}:\d{2})?")
                .expect("static regex");
            parse_local_date_time(pattern.find_iter(valid).last().map(|m| m.as_str()))
        }
    };
    Some(json!({
        "productId": product_id,
        "productName": item["productName"].as_str().unwrap_or(product_id),
        "billingCycle": non_empty(&item["billingCycle"]),
        "renewTime": renew_time,
        "expireTime": expire_time,
    }))
}

/// `normalizeLimits`。
pub fn normalize_limits(limits: &Value) -> Vec<Value> {
    limits
        .as_array()
        .into_iter()
        .flatten()
        .filter(|limit| limit["type"].as_str().is_some_and(|t| !t.is_empty()))
        .map(|limit| {
            let mut normalized = Map::new();
            normalized.insert("type".into(), limit["type"].clone());
            for key in [
                "unit",
                "number",
                "usage",
                "currentValue",
                "remaining",
                "percentage",
                "nextResetTime",
            ] {
                if limit[key].is_number() {
                    normalized.insert(key.into(), limit[key].clone());
                }
            }
            let details: Vec<Value> = limit["usageDetails"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|detail| detail["modelCode"].is_string())
                .map(|detail| {
                    let mut item = Map::new();
                    item.insert("modelCode".into(), detail["modelCode"].clone());
                    if let Some(name) = non_empty(&detail["displayName"]) {
                        item.insert("displayName".into(), json!(name));
                    }
                    item.insert(
                        "usage".into(),
                        if detail["usage"].is_number() {
                            detail["usage"].clone()
                        } else {
                            json!(0)
                        },
                    );
                    Value::Object(item)
                })
                .collect();
            normalized.insert("usageDetails".into(), Value::Array(details));
            Value::Object(normalized)
        })
        .collect()
}

pub fn pick_primary_limit(limits: &[Value]) -> Option<&Value> {
    limits
        .iter()
        .find(|limit| limit["type"] == "TIME_LIMIT")
        .or_else(|| limits.iter().find(|limit| limit["remaining"].is_number()))
        .or_else(|| limits.first())
}

fn bigmodel_envelope_ok(payload: &Value) -> bool {
    payload["success"] != false
        && (payload["code"].is_null()
            || payload["code"]
                .as_f64()
                .is_some_and(|code| code == 0.0 || code == 200.0))
}

/// 套餐额度查询的鉴权（`ResolvedQuotaAuthorization`）。
#[derive(Debug, Clone)]
struct QuotaAuthorization {
    authorization: String,
    quota_url: String,
    team: Option<(String, String, String)>,
    provider: Value,
}

fn team_context(access: &Value) -> Option<(String, String, String)> {
    (access["planKind"] == "team-coding-plan").then(|| {
        (
            access["organizationId"].as_str().unwrap_or("").to_owned(),
            access["projectId"].as_str().unwrap_or("").to_owned(),
            access["family"].as_str().unwrap_or("").to_owned(),
        )
    })
}

fn with_query(url: &str, pairs: &[(&str, &str)]) -> String {
    let Ok(mut parsed) = url::Url::parse(url) else {
        return url.to_owned();
    };
    let existing: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(key, _)| !pairs.iter().any(|(name, _)| name == key))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let mut serializer = parsed.query_pairs_mut();
    serializer.clear().extend_pairs(existing);
    for (key, value) in pairs {
        serializer.append_pair(key, value);
    }
    drop(serializer);
    parsed.to_string()
}

#[derive(Clone)]
pub struct UsageStats {
    api: ApiClient,
    auth: AccountRequestAuth,
    credentials: CredentialStore,
    billing: StartPlanBilling,
    official: OfficialMcpCredentials,
    /// App Usage 读取任意已运行 workspace 的 CLI（usage 表在全局会话库）。
    app_usage: Arc<
        dyn Fn(Value) -> futures_util::future::BoxFuture<'static, Result<Value, RpcError>>
            + Send
            + Sync,
    >,
}

impl UsageStats {
    pub fn new(
        api: ApiClient,
        auth: AccountRequestAuth,
        credentials: CredentialStore,
        billing: StartPlanBilling,
        official: OfficialMcpCredentials,
        app_usage: Arc<
            dyn Fn(Value) -> futures_util::future::BoxFuture<'static, Result<Value, RpcError>>
                + Send
                + Sync,
        >,
    ) -> Self {
        Self {
            api,
            auth,
            credentials,
            billing,
            official,
            app_usage,
        }
    }

    fn env(&self, key: &str) -> Option<String> {
        self.api.endpoints().env(key).map(str::to_owned)
    }

    fn quota_url_from_env(&self) -> Option<String> {
        self.env("ZCODE_BIGMODEL_USAGE_QUOTA_URL")
            .or_else(|| self.env("BIGMODEL_USAGE_QUOTA_URL"))
    }

    fn bigmodel_quota_url(&self) -> String {
        self.api.endpoints().bigmodel_api_url(BIGMODEL_QUOTA_PATH)
    }

    fn zai_quota_url(&self) -> String {
        self.api.endpoints().zai_business_url(BIGMODEL_QUOTA_PATH)
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

    async fn resolve_request_access(&self, access: &Value) -> Option<Value> {
        if !access.is_object() {
            return None;
        }
        if access.get("mode").is_none() {
            return Some(access.clone());
        }
        self.auth.resolve_access_current(access).await
    }

    async fn resolve_authorization(
        &self,
        preferred: &str,
        access: &Value,
        require_preferred: bool,
        allow_env: Option<bool>,
    ) -> Result<Option<QuotaAuthorization>, RpcError> {
        if allow_env != Some(false) && !require_preferred {
            let env_key = self
                .env("ZCODE_BIGMODEL_USAGE_API_KEY")
                .or_else(|| self.env("BIGMODEL_USAGE_API_KEY"));
            if let Some(key) = env_key {
                return Ok(Some(QuotaAuthorization {
                    authorization: key,
                    quota_url: self
                        .quota_url_from_env()
                        .unwrap_or_else(|| self.bigmodel_quota_url()),
                    team: None,
                    provider: json!({"id": "env:bigmodel-usage", "name": "BigModel"}),
                }));
            }
        }
        let provider_id = preferred.trim();
        let access = self.resolve_request_access(access).await;
        if !provider_id.is_empty() && is_coding_plan_provider(provider_id) {
            let Some(access) = access else {
                return Ok(None);
            };
            let key = self
                .auth
                .resolve_current(provider_id, &access)
                .await
                .map_err(|failure| error(failure.to_string()))?;
            return Ok(Some(QuotaAuthorization {
                authorization: key,
                quota_url: self.quota_url_from_env().unwrap_or_else(|| {
                    if is_zai_coding_plan_provider(provider_id) {
                        self.zai_quota_url()
                    } else {
                        self.bigmodel_quota_url()
                    }
                }),
                team: team_context(&access),
                provider: json!({"id": provider_id, "name": provider_label(provider_id)}),
            }));
        }
        Ok(None)
    }

    /// `fetchBigModelSubscriptionSummary`。
    async fn subscription_summary(
        &self,
        authorization: &str,
        quota_url: &str,
        team: Option<&(String, String, String)>,
    ) -> Entitlement {
        let Ok(mut url) = url::Url::parse(quota_url) else {
            return Entitlement::Unknown;
        };
        url.set_path("/api/biz/subscription/list");
        url.set_query(None);
        if let Some((organization, project, family)) = team {
            let token = self
                .load_trimmed(&format!("oauth:{family}:access_token"))
                .await;
            if token.is_empty() {
                return Entitlement::Unknown;
            }
            let authorization = if family == "zai" {
                format!("Bearer {token}")
            } else {
                token
            };
            let origin = url.origin().ascii_serialization();
            return match fetch_team_entitlement(
                &self.api,
                &authorization,
                &origin,
                (organization, project),
                REQUEST_TIMEOUT_MS,
            )
            .await
            {
                Ok(Entitlement::Available(detail)) => Entitlement::Available(json!({
                    "productId": detail["productId"].as_str().unwrap_or(""),
                    "productName": detail["productName"].as_str().unwrap_or(""),
                    "billingCycle": detail["subscribePeriod"].as_str().map(str::to_lowercase),
                    "expireTime": detail["subscribeEndTime"],
                    "renewTime": null,
                })),
                Ok(other) => other,
                Err(_) => Entitlement::Unknown,
            };
        }
        match fetch_personal_entitlement(&self.api, authorization, url.as_str(), REQUEST_TIMEOUT_MS)
            .await
        {
            Ok(Entitlement::Available(subscription)) => match subscription_summary(&subscription) {
                Some(summary) => Entitlement::Available(summary),
                None => Entitlement::Unknown,
            },
            Ok(other) => other,
            Err(_) => Entitlement::Unknown,
        }
    }

    fn quota_limit_url(resolved: &QuotaAuthorization) -> String {
        match resolved.team {
            Some(_) => with_query(&resolved.quota_url, &[("type", "2")]),
            None => resolved.quota_url.clone(),
        }
    }

    fn usage_headers(request: ApiRequest, resolved: &QuotaAuthorization) -> ApiRequest {
        let mut request = request.header("authorization", resolved.authorization.clone());
        if let Some((organization, project, _)) = &resolved.team {
            request = request
                .header("bigmodel-organization", organization.clone())
                .header("bigmodel-project", project.clone());
        }
        request
    }

    async fn fetch_quota(&self, resolved: &QuotaAuthorization) -> Result<Value, ApiError> {
        self.api
            .read_json(Self::usage_headers(
                ApiRequest::get(Self::quota_limit_url(resolved)).timeout_ms(REQUEST_TIMEOUT_MS),
                resolved,
            ))
            .await
    }

    /// `fetchMcpQuotaSnapshot`：只有 Coding Plan provider 会查询，任何失败都静默降级为 null。
    async fn fetch_mcp_quota(&self, resolved: &QuotaAuthorization) -> Value {
        let provider_id = resolved.provider["id"].as_str().unwrap_or("");
        if !is_coding_plan_provider(provider_id) {
            return Value::Null;
        }
        let family = resolved
            .team
            .as_ref()
            .map(|(_, _, family)| family.clone())
            .unwrap_or_else(|| {
                if is_zai_coding_plan_provider(provider_id) {
                    "zai"
                } else {
                    "bigmodel"
                }
                .to_owned()
            });
        let snapshot = match self.official.resolve().await {
            Ok(snapshot) => snapshot,
            Err(reason) => {
                log::info(format!(
                    "[usage-stats] 官方 MCP 额度不可用，跳过请求 reason={reason}"
                ));
                return Value::Null;
            }
        };
        let Some(plan_scope) = &snapshot.plan_scope else {
            return Value::Null;
        };
        let scope = if plan_scope["targetType"] == "TEAM" {
            json!({"organizationId": plan_scope["organizationId"], "projectId": plan_scope["projectId"], "providerFamily": snapshot.provider_family, "targetType": "TEAM"})
        } else {
            json!({"providerFamily": snapshot.provider_family, "targetType": "PERSONAL"})
        };
        let (request_org, request_project) = resolved
            .team
            .as_ref()
            .map(|(org, project, _)| (org.trim().to_owned(), project.trim().to_owned()))
            .unwrap_or_default();
        let matches = scope["providerFamily"] == family.as_str()
            && if request_org.is_empty() || request_project.is_empty() {
                scope["targetType"] == "PERSONAL"
            } else {
                scope["targetType"] == "TEAM"
                    && scope["organizationId"].as_str().map(str::trim) == Some(request_org.as_str())
                    && scope["projectId"].as_str().map(str::trim) == Some(request_project.as_str())
            };
        if !matches {
            log::info("[usage-stats] 官方 MCP 额度归属与本次查询不一致，跳过请求");
            return Value::Null;
        }
        let url = self.api.endpoints().zcode_api_url(MCP_USAGE_PATH);
        let mut request = ApiRequest::get(&url).timeout_ms(REQUEST_TIMEOUT_MS);
        for (name, value) in OfficialMcpCredentials::headers(&snapshot) {
            request = request.header(&name, value.as_str().unwrap_or("").to_owned());
        }
        let response = match self.api.request(request).await {
            Ok(response) => response,
            Err(failure) => {
                log::warn(format!(
                    "[usage-stats] 官方 MCP 额度请求异常: {}",
                    failure.message
                ));
                return Value::Null;
            }
        };
        if !response.ok() {
            log::warn(format!(
                "[usage-stats] 官方 MCP 额度请求失败 status={}",
                response.status
            ));
            return Value::Null;
        }
        let Ok(payload) = response
            .text()
            .await
            .map_err(|_| ())
            .and_then(|text| serde_json::from_str::<Value>(&text).map_err(|_| ()))
        else {
            return Value::Null;
        };
        let data = &payload["data"];
        let total = &data["total_usage"];
        let finite = |value: &Value| value.as_f64().filter(|v| v.is_finite());
        if payload["code"].as_f64() != Some(0.0)
            || !data.is_object()
            || finite(&data["server_time"]).is_none()
        {
            return Value::Null;
        }
        let (Some(used), Some(limit), Some(remaining)) = (
            finite(&total["used"]),
            finite(&total["limit"]),
            finite(&total["remaining"]),
        ) else {
            log::warn("[usage-stats] 官方 MCP 额度响应缺少 total_usage");
            return Value::Null;
        };
        let limit = limit.max(0.0);
        if limit <= 0.0 {
            return Value::Null;
        }
        let remaining = remaining.max(0.0).min(limit);
        let used = used.max(0.0);
        let percentage = (100.0 - (remaining / limit) * 100.0).clamp(0.0, 100.0);
        let mut aggregate = json!({
            "type": MCP_USAGE_QUOTA_LIMIT_TYPE,
            "currentValue": number(used),
            "usage": number(used),
            "remaining": number(remaining),
            "percentage": number(percentage),
        });
        if let Some(next) = finite(&data["next_refresh_at"]) {
            aggregate["nextResetTime"] = number(next * 1000.0);
        }
        aggregate["usageDetails"] = json!([]);
        json!({
            "aggregate": aggregate,
            "level": non_empty(&data["level"]),
            "scope": scope,
            "serverTime": number(data["server_time"].as_f64().unwrap_or(0.0) * 1000.0),
        })
    }

    /// monitor 接口 URL：把 quota/limit 路径换成目标接口并带上查询参数（`build*MonitorUrl`）。
    fn monitor_url(
        resolved: &QuotaAuthorization,
        from: &str,
        to: &str,
        pairs: &[(&str, &str)],
    ) -> String {
        let Ok(mut url) = url::Url::parse(&resolved.quota_url) else {
            return resolved.quota_url.clone();
        };
        let path = url.path().to_owned();
        if let Some(prefix) = path.strip_suffix(from) {
            url.set_path(&format!("{prefix}{to}"));
        }
        with_query(url.as_str(), pairs)
    }

    async fn fetch_monitor(
        &self,
        resolved: &QuotaAuthorization,
        url: String,
    ) -> Result<Value, ApiError> {
        self.api
            .read_json(Self::usage_headers(
                ApiRequest::get(url).timeout_ms(REQUEST_TIMEOUT_MS),
                resolved,
            ))
            .await
    }

    /// `getUsageStatsSnapshot`：设置页的 monitor 用量（model-usage / tool-usage）。
    pub async fn usage_snapshot(&self, request: &Value) -> Result<Value, RpcError> {
        let resolved = self
            .resolve_authorization(
                request["preferredProviderId"].as_str().unwrap_or(""),
                &request["accountAccess"],
                request["requirePreferredProvider"] == true,
                request["allowEnvApiKey"].as_bool(),
            )
            .await?
            .ok_or_else(|| error("no_bigmodel_api_key"))?;
        let (start, end) =
            super::usage_monitor::usage_time_range(request["range"].as_str().unwrap_or(""));
        let team = if resolved.team.is_some() {
            vec![("type", "2")]
        } else {
            Vec::new()
        };
        let query = |extra: &[(&'static str, &str)]| -> Vec<(&'static str, String)> {
            team.iter()
                .map(|(k, v)| (*k, (*v).to_owned()))
                .chain(extra.iter().map(|(k, v)| (*k, (*v).to_owned())))
                .collect()
        };
        let pairs = query(&[("startTime", &start), ("endTime", &end)]);
        let pairs: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (model, tool) = tokio::join!(
            self.fetch_monitor(
                &resolved,
                Self::monitor_url(&resolved, "/quota/limit", "/model-usage", &pairs)
            ),
            self.fetch_monitor(
                &resolved,
                Self::monitor_url(&resolved, "/quota/limit", "/tool-usage", &pairs)
            ),
        );
        let model = monitor_data(
            &model.map_err(api_error)?,
            "BigModel model usage failed",
            false,
        )?;
        let tool = monitor_data(
            &tool.map_err(api_error)?,
            "BigModel tool usage failed",
            true,
        )?;
        let mut snapshot =
            super::usage_monitor::usage_stats_snapshot(request, &model, &tool, now_ms());
        snapshot["sourceProvider"] = resolved.provider.clone();
        Ok(snapshot)
    }

    /// `getCodingPlanUsageSnapshot`：quota 必须成功；activity / detail / health 传输失败只清空该区域。
    pub async fn coding_plan_usage(&self, request: &Value) -> Result<Value, RpcError> {
        let preferred = request["preferredProviderId"].as_str().unwrap_or("");
        let resolved = self
            .resolve_authorization(preferred, &request["accountAccess"], true, Some(false))
            .await?
            .ok_or_else(|| {
                error(if is_zai_coding_plan_provider(preferred) {
                    "zai_coding_plan_api_key_required"
                } else {
                    "bigmodel_coding_plan_api_key_required"
                })
            })?;
        let range = super::usage_monitor::coding_plan_range(request)?;
        let time_zone = request["timeZone"].as_str();
        let (activity_start, activity_end) = super::usage_monitor::trailing_range(time_zone, 365)?;
        let (health_start, health_end) = super::usage_monitor::trailing_range(time_zone, 6)?;
        let credit_type = if resolved.team.is_some() { "3" } else { "1" };
        let credit_url = |endpoint: &str, start: &str, end: &str, usage_type: Option<&str>| {
            let mut pairs = vec![
                ("type", credit_type),
                ("startTime", start),
                ("endTime", end),
            ];
            if let Some(usage_type) = usage_type {
                pairs.push(("usageType", usage_type));
            }
            Self::monitor_url(
                &resolved,
                "/usage/quota/limit",
                &format!("/credit-usage/{endpoint}"),
                &pairs,
            )
        };
        let (quota, activity, model_detail, tool_detail, health) = tokio::join!(
            self.fetch_quota(&resolved),
            self.fetch_monitor(
                &resolved,
                credit_url("activity", &activity_start, &activity_end, None)
            ),
            self.fetch_monitor(
                &resolved,
                credit_url(
                    "usage-detail",
                    &range.start_time,
                    &range.end_time,
                    Some("MODEL")
                )
            ),
            self.fetch_monitor(
                &resolved,
                credit_url(
                    "usage-detail",
                    &range.start_time,
                    &range.end_time,
                    Some("MCP")
                )
            ),
            self.fetch_monitor(
                &resolved,
                Self::monitor_url(
                    &resolved,
                    "/quota/limit",
                    "/model-performance-day",
                    &[("startTime", &health_start), ("endTime", &health_end)]
                ),
            ),
        );
        let quota = quota.map_err(api_error)?;
        let activity = best_effort(activity, "BigModel activity usage failed")?;
        let model_detail = best_effort(model_detail, "BigModel model usage detail failed")?;
        let tool_detail = best_effort(tool_detail, "BigModel tool usage detail failed")?;
        let health = best_effort(health, "BigModel model performance failed")?;
        let quota = if bigmodel_envelope_ok(&quota) && quota["data"].is_object() {
            json!({
                "level": quota["data"]["level"].as_str().map(str::trim).filter(|l| !l.is_empty()),
                "limits": normalize_limits(&quota["data"]["limits"]),
            })
        } else {
            Value::Null
        };
        Ok(super::usage_monitor::coding_plan_snapshot(
            super::usage_monitor::CodingPlanInputs {
                request,
                provider: &resolved.provider,
                quota,
                range: &range,
                activity: &activity,
                model_detail: &model_detail,
                tool_detail: &tool_detail,
                health: &health,
                now_ms: now_ms(),
            },
        ))
    }

    /// `getSnapshotForRequest`。
    pub async fn entitlement_snapshot(&self, request: &Value) -> Result<Value, RpcError> {
        let access = self.resolve_request_access(&request["accountAccess"]).await;
        let preferred = request["preferredProviderId"].as_str().unwrap_or("");
        if !preferred.is_empty() && is_start_plan_provider(preferred) {
            return self
                .start_plan_snapshot(
                    preferred,
                    access.as_ref(),
                    request["invalidateBalanceCache"] == true,
                )
                .await;
        }
        let generated_at = now_ms();
        let team = access.as_ref().and_then(team_context);
        let provider_id = preferred.trim().to_owned();
        let team_entitlement = match &team {
            Some(context) => {
                let quota_url = self.quota_url_from_env().unwrap_or_else(|| {
                    if context.2 == "zai" {
                        self.zai_quota_url()
                    } else {
                        self.bigmodel_quota_url()
                    }
                });
                Some(
                    self.subscription_summary("", &quota_url, Some(context))
                        .await,
                )
            }
            None => None,
        };
        let resolved = match self
            .resolve_authorization(
                preferred,
                access.as_ref().unwrap_or(&Value::Null),
                request["requirePreferredProvider"] == true,
                request["allowEnvApiKey"].as_bool(),
            )
            .await
        {
            Ok(resolved) => resolved,
            Err(failure) => {
                log::warn(format!(
                    "[usage-stats] 读取用量凭据失败，保留独立订阅判定 provider={provider_id}: {}",
                    failure.message
                ));
                None
            }
        };
        let entitlement = match (&team_entitlement, &resolved) {
            (Some(entitlement), _) => entitlement.clone(),
            (None, Some(resolved)) => {
                self.subscription_summary(&resolved.authorization, &resolved.quota_url, None)
                    .await
            }
            (None, None) => Entitlement::Unknown,
        };
        if matches!(entitlement, Entitlement::Unknown) && (resolved.is_some() || team.is_some()) {
            return Err(error("Coding Plan entitlement refresh failed"));
        }
        let subscription = match &entitlement {
            Entitlement::Available(summary) => {
                let mut detail = summary.clone();
                detail["purchaseTime"] = Value::Null;
                detail["beginTime"] = Value::Null;
                json!({"identityType": "unknown", "identityMasked": null, "details": [detail]})
            }
            _ => Value::Null,
        };
        let (quota_payload, mcp_quota) = match &resolved {
            Some(resolved) => {
                let (quota, mcp) =
                    tokio::join!(self.fetch_quota(resolved), self.fetch_mcp_quota(resolved));
                (quota.ok(), mcp)
            }
            None => (None, Value::Null),
        };
        let quota_data = quota_payload
            .filter(bigmodel_envelope_ok)
            .map(|payload| payload["data"].clone())
            .filter(|data| data.is_object());
        let limits = normalize_limits(
            quota_data
                .as_ref()
                .map(|data| &data["limits"])
                .unwrap_or(&Value::Null),
        );
        let primary = pick_primary_limit(&limits);
        if let Some(access) = &access
            && !provider_id.is_empty()
        {
            self.auth
                .assert_current(&provider_id, access)
                .await
                .map_err(|failure| error(failure.to_string()))?;
        }
        let mut snapshot = Map::new();
        snapshot.insert("generatedAt".into(), json!(generated_at));
        snapshot.insert("authenticated".into(), json!(true));
        if !matches!(entitlement, Entitlement::Available(_)) {
            let reason = match entitlement {
                Entitlement::Unavailable(_) => "no_plan",
                _ if resolved.is_some() || team.is_some() => "unavailable",
                _ => "not_configured",
            };
            snapshot.insert("unavailableReason".into(), json!(reason));
        }
        if let Some(Entitlement::Unavailable(Some(reason))) = &team_entitlement {
            snapshot.insert("teamPlanUnavailableReason".into(), json!(reason));
        }
        let detail = &subscription["details"][0];
        let product = |key: &str| {
            non_empty(&detail[key])
                .map(Value::String)
                .unwrap_or(Value::Null)
        };
        let context = match &resolved {
            Some(resolved) => match &resolved.team {
                None => {
                    json!({"scope": "personal", "productId": product("productId"), "displayName": product("productName")})
                }
                Some((organization, project, _)) => {
                    json!({"scope": "team", "organizationId": organization, "projectId": project, "productId": product("productId"), "displayName": product("productName")})
                }
            },
            None => match access
                .as_ref()
                .filter(|access| access["planKind"] == "team-coding-plan")
            {
                Some(access) => {
                    json!({"scope": "team", "organizationId": access["organizationId"], "projectId": access["projectId"]})
                }
                None => json!({"scope": "personal"}),
            },
        };
        snapshot.insert("context".into(), context);
        snapshot.insert(
            "provider".into(),
            resolved
                .as_ref()
                .map(|r| r.provider.clone())
                .unwrap_or(Value::Null),
        );
        snapshot.insert(
            "remaining".into(),
            match primary {
                Some(limit) => {
                    let mut remaining = json!({"count": limit["remaining"].as_f64().map(number).unwrap_or(json!(0)), "isShow": true});
                    if limit.get("percentage").is_some() {
                        remaining["percentage"] = limit["percentage"].clone();
                    }
                    remaining["nextResetTime"] = limit.get("nextResetTime").cloned().unwrap_or(Value::Null);
                    remaining
                }
                None => Value::Null,
            },
        );
        snapshot.insert("subscription".into(), subscription);
        snapshot.insert(
            "quota".into(),
            match &quota_data {
                Some(data) => json!({"level": non_empty(&data["level"]), "limits": limits}),
                None => Value::Null,
            },
        );
        snapshot.insert("mcpQuota".into(), mcp_quota);
        Ok(Value::Object(snapshot))
    }

    async fn start_plan_snapshot(
        &self,
        provider_id: &str,
        access: Option<&Value>,
        invalidate: bool,
    ) -> Result<Value, RpcError> {
        let generated_at = now_ms();
        let authorization = match access.filter(|access| access["planKind"] == "start-plan") {
            Some(access) => self
                .auth
                .resolve_current(provider_id, access)
                .await
                .ok()
                .map(|key| key.trim().to_owned())
                .filter(|key| !key.is_empty())
                .map(|key| {
                    if key.len() > 7 && key[..7].eq_ignore_ascii_case("bearer ") {
                        key
                    } else {
                        format!("Bearer {key}")
                    }
                }),
            None => None,
        };
        let Some(authorization) = authorization else {
            return Ok(json!({
                "generatedAt": generated_at,
                "authenticated": true,
                "unavailableReason": "not_configured",
                "context": {"scope": "personal"},
                "provider": null,
                "remaining": null,
                "subscription": null,
                "quota": null,
            }));
        };
        let provider = json!({"id": provider_id, "name": provider_label(provider_id)});
        let payload = match self.billing.fetch(&authorization, invalidate).await {
            Ok(payload) => payload,
            Err(failure) => {
                log::warn(format!(
                    "[usage-stats] billing/balance 请求失败 provider={provider_id} status={:?}: {}",
                    failure.status, failure.message
                ));
                return Err(api_error(failure));
            }
        };
        if payload["code"].as_f64() != Some(0.0) {
            return Err(error(
                payload["msg"]
                    .as_str()
                    .filter(|msg| !msg.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("Start Plan balance failed: {}", payload["code"])),
            ));
        }
        let plans = &payload["data"]["plans"];
        let current = plans.as_array().into_iter().flatten().find(|plan| {
            let lower = |key: &str| non_empty(&plan[key]).map(|v| v.to_lowercase());
            lower("status").as_deref() == Some("active")
                && [lower("plan_id"), lower("name")]
                    .iter()
                    .flatten()
                    .any(|value| value.contains("start-plan") || value.contains("start plan"))
        });
        if current.is_none() {
            let expired = plans.as_array().into_iter().flatten().any(|plan| {
                plan["status"].as_str().map(str::to_lowercase).as_deref() == Some("expired")
            });
            return Ok(json!({
                "generatedAt": generated_at,
                "authenticated": true,
                "unavailableReason": "no_plan",
                "startPlanExpired": expired,
                "context": {"scope": "personal"},
                "provider": provider,
                "remaining": null,
                "subscription": null,
                "quota": null,
            }));
        }
        let balances = &payload["data"]["balances"];
        let limits = start_plan_limits(balances, Some(plans));
        let remaining_limits = start_plan_limits(balances, None);
        let mut snapshot = Map::new();
        snapshot.insert("generatedAt".into(), json!(generated_at));
        snapshot.insert("authenticated".into(), json!(true));
        snapshot.insert("context".into(), json!({"scope": "personal"}));
        snapshot.insert("provider".into(), provider);
        if let Some(server_time) = payload["data"]["server_time"]
            .as_f64()
            .filter(|t| t.is_finite() && *t >= 0.0)
        {
            snapshot.insert("serverTime".into(), number(server_time * 1000.0));
        }
        snapshot.insert("remaining".into(), start_plan_remaining(&remaining_limits));
        snapshot.insert("subscription".into(), start_plan_subscription(plans));
        snapshot.insert(
            "quota".into(),
            if limits.is_empty() {
                Value::Null
            } else {
                json!({"level": "Start", "limits": limits})
            },
        );
        Ok(Value::Object(snapshot))
    }

    async fn reset_authorization(
        &self,
        request: &Value,
    ) -> Result<(String, String, Option<(String, String, String)>), RpcError> {
        let access = self
            .resolve_request_access(&request["accountAccess"])
            .await
            .ok_or_else(|| error("coding_plan_reset_account_access_required"))?;
        let provider = request["preferredProviderId"].as_str().unwrap_or("");
        self.auth
            .assert_current(provider, &access)
            .await
            .map_err(|failure| error(failure.to_string()))?;
        let jwt = self.load_trimmed(ZCODE_JWT_TOKEN_KEY).await;
        if jwt.is_empty() {
            return Err(error("coding_plan_reset_zcode_jwt_required"));
        }
        let family = access["family"].as_str().unwrap_or("");
        let maas = self
            .load_trimmed(&super::oauth_repo::access_token_key(oauth_provider_of(
                family,
            )))
            .await;
        if maas.is_empty() {
            return Err(error("coding_plan_reset_maas_jwt_required"));
        }
        let zcode = if jwt.len() > 7 && jwt[..7].eq_ignore_ascii_case("bearer ") {
            jwt
        } else {
            format!("Bearer {jwt}")
        };
        Ok((zcode, maas, team_context(&access)))
    }

    async fn reset_request(
        &self,
        request: &Value,
        path: &str,
        body: Option<Value>,
        include_scope: bool,
        accepted: &[i64],
    ) -> Result<Value, RpcError> {
        let preferred = request["preferredProviderId"].as_str().unwrap_or("");
        if !is_coding_plan_provider(preferred) {
            return Err(error("no_bigmodel_api_key"));
        }
        let (zcode, maas, team) = self.reset_authorization(request).await?;
        let url = self
            .api
            .endpoints()
            .zcode_api_url(&format!("{CODING_PLAN_RESET_BASE_PATH}/{path}"));
        let mut api_request = match &body {
            Some(_) => ApiRequest::post(&url),
            None if path == "history/read" => ApiRequest::post(&url),
            None => ApiRequest::get(&url),
        }
        .timeout_ms(REQUEST_TIMEOUT_MS)
        .header("Authorization", zcode)
        .header("X-Bigmodel-Authorization", maas);
        if include_scope {
            api_request = api_request.header(
                "Bigmodel-Target-Type",
                if team.is_some() { "TEAM" } else { "PERSONAL" },
            );
            if let Some((organization, project, _)) = &team {
                api_request = api_request
                    .header("Bigmodel-Organization", organization.clone())
                    .header("Bigmodel-Project", project.clone());
            }
        }
        if let Some(body) = &body {
            api_request = api_request
                .json(body)
                .header("content-type", "application/json");
        }
        let response = self.api.request(api_request).await.map_err(api_error)?;
        let status = response.status;
        let ok = response.ok();
        let request_id = response
            .header("x-request-id")
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned);
        let text = response.text().await.map_err(api_error)?;
        let payload = serde_json::from_str::<Value>(&text).ok();
        if let Some(payload) = &payload
            && let Some(code) = payload["code"].as_i64()
            && payload["code"].is_number()
            && (payload["msg"].is_null() || payload["msg"].is_string())
            && code != 0
        {
            if accepted.contains(&code) {
                return Ok(payload.clone());
            }
            let suffix = request_id
                .map(|id| format!(" (x-request-id:{id})"))
                .unwrap_or_default();
            return Err(error(format!("coding_plan_reset_api_error:{code}{suffix}")));
        }
        if !ok {
            let message = payload
                .as_ref()
                .and_then(|payload| {
                    ["error", "message", "msg", "detail"]
                        .iter()
                        .find_map(|key| non_empty(&payload[*key]))
                })
                .unwrap_or_else(|| format!("HTTP {status}"));
            if status == 429 && path == "opportunity" {
                return Err(error("coding_plan_reset_opportunity_throttled"));
            }
            return Err(RpcError::new("ApiError", message).with_data(json!({"status": status})));
        }
        payload.ok_or_else(|| error("coding_plan_reset_invalid_response"))
    }

    fn reset_data(payload: &Value) -> Result<&Value, RpcError> {
        if !payload["code"].is_number() {
            return Err(error("coding_plan_reset_invalid_response"));
        }
        if payload["code"].as_f64() != Some(0.0) {
            return Err(error(format!(
                "coding_plan_reset_api_error:{}",
                payload["code"]
            )));
        }
        Ok(&payload["data"])
    }

    pub async fn reset_status(&self, request: &Value) -> Result<Value, RpcError> {
        let payload = self
            .reset_request(request, "status", None, true, &[])
            .await?;
        let data = Self::reset_data(&payload)?;
        let invalid = || error("coding_plan_reset_invalid_response");
        let positive = |value: &Value| value.as_f64().filter(|v| v.is_finite() && *v > 0.0);
        let list = |key: &str| -> Result<Vec<Value>, RpcError> {
            data[key]
                .as_array()
                .ok_or_else(invalid)?
                .iter()
                .map(|item| {
                    positive(&item["expire_at"])
                        .map(|v| json!({"expireAt": number(v)}))
                        .ok_or_else(invalid)
                })
                .collect()
        };
        let history = |key: &str| -> Result<Value, RpcError> {
            match &data[key] {
                Value::Null if data.get(key).is_some() => Ok(Value::Null),
                item => positive(&item["used_at"])
                    .map(|v| json!({"usedAt": number(v)}))
                    .ok_or_else(invalid),
            }
        };
        Ok(json!({
            "availableFiveHourResets": list("available_five_hour_resets")?,
            "availableWeekResets": list("available_week_resets")?,
            "latestFiveHourResetHistory": history("latest_five_hour_reset_history")?,
            "latestWeekResetHistory": history("latest_week_reset_history")?,
            "hasUnreadHistory": data["has_unread_history"].as_bool().ok_or_else(invalid)?,
        }))
    }

    fn idempotency_key(request: &Value) -> Result<String, RpcError> {
        let key = request["idempotencyKey"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_owned();
        if key.is_empty() || key.chars().count() > 64 {
            return Err(error("coding_plan_reset_invalid_idempotency_key"));
        }
        Ok(key)
    }

    pub async fn use_reset(&self, request: &Value) -> Result<Value, RpcError> {
        let key = Self::idempotency_key(request)?;
        let body = json!({"idempotency_key": key, "reset_type": request["resetType"]});
        let payload = self
            .reset_request(request, "use", Some(body), true, &[])
            .await?;
        let data = Self::reset_data(&payload)?;
        if data["used"] != true {
            return Err(error("coding_plan_reset_invalid_response"));
        }
        Ok(json!({"used": true}))
    }

    pub async fn request_opportunity(&self, request: &Value) -> Result<Value, RpcError> {
        let key = Self::idempotency_key(request)?;
        let payload = self
            .reset_request(
                request,
                "opportunity",
                Some(json!({"idempotency_key": key})),
                true,
                &[3301],
            )
            .await?;
        if payload["code"].as_i64() == Some(3301) {
            let next = payload["data"]["next_try_at"]
                .as_i64()
                .filter(|v| *v > 0 && payload["data"]["granted"] == false)
                .ok_or_else(|| error("coding_plan_reset_invalid_response"))?;
            return Ok(json!({"granted": false, "nextTryAt": next}));
        }
        let data = Self::reset_data(&payload)?;
        if data["granted"] != true {
            return Err(error("coding_plan_reset_invalid_response"));
        }
        Ok(json!({"granted": true, "nextTryAt": null}))
    }

    pub async fn mark_history_read(&self, request: &Value) -> Result<(), RpcError> {
        let payload = self
            .reset_request(request, "history/read", None, false, &[])
            .await?;
        Self::reset_data(&payload).map(|_| ())
    }
}

/// `normalizeZaiStartPlanBalanceLimits`（带 plans 时附加 period）。
fn start_plan_limits(balances: &Value, plans: Option<&Value>) -> Vec<Value> {
    balances
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|balance| {
            let total = parse_number(&balance["total_units"]);
            let used = parse_number(&balance["used_units"]);
            let remaining = parse_number(&balance["remaining_units"]);
            if total.is_none() && used.is_none() && remaining.is_none() {
                return None;
            }
            let next_reset = parse_unix_seconds(&balance["expires_at"]);
            let capabilities: Vec<String> = balance["capabilities"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|c| c.strip_prefix("model:").unwrap_or(c).to_owned())
                .filter(|c| !c.trim().is_empty())
                .collect();
            let capability_type = non_empty(&json!(capabilities.join(", ")));
            let display_name = non_empty(&balance["show_name"]);
            let plan_id = non_empty(&balance["plan_id"]);
            let user_plan_id = non_empty(&balance["user_plan_id"]);
            let plan = plans.and_then(|plans| {
                plans.as_array()?.iter().find(|candidate| {
                    match (
                        &user_plan_id,
                        candidate["user_plan_id"].as_str().filter(|s| !s.is_empty()),
                    ) {
                        (Some(owner), Some(id)) => owner == id,
                        _ => {
                            candidate
                                .get("plan_id")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                                == plan_id
                        }
                    }
                })
            });
            let period = plan.and_then(|plan| {
                plan["entitlements"]
                    .as_array()?
                    .iter()
                    .find(|entry| entry["entitlement_id"] == balance["entitlement_id"])
                    .and_then(|entry| non_empty(&entry["period"]))
            });
            let mut limit = Map::new();
            let mut put = |key: &str, value: Option<Value>| {
                if let Some(value) = value {
                    limit.insert(key.to_owned(), value);
                }
            };
            put(
                "bucketId",
                non_empty(&balance["bucket_id"]).map(Value::String),
            );
            put("userPlanId", user_plan_id.clone().map(Value::String));
            put(
                "periodStart",
                parse_unix_seconds(&balance["period_start"]).map(|v| number(v * 1000.0)),
            );
            put(
                "periodEnd",
                parse_unix_seconds(&balance["period_end"]).map(|v| number(v * 1000.0)),
            );
            put("period", period.map(Value::String));
            put("meter", non_empty(&balance["meter"]).map(Value::String));
            put(
                "unitType",
                non_empty(&balance["unit_type"]).map(Value::String),
            );
            let kind = non_empty(&balance["entitlement_id"])
                .or(capability_type)
                .or_else(|| non_empty(&balance["meter"]))
                .unwrap_or_else(|| "model_usage".to_owned());
            put("type", Some(json!(kind)));
            put("planId", plan_id.map(Value::String));
            put("unit", total.map(number));
            put("number", total.map(number));
            put("usage", used.map(number));
            put("currentValue", used.map(number));
            put("remaining", remaining.map(number));
            put(
                "percentage",
                match (total, remaining) {
                    (Some(total), Some(remaining)) if total > 0.0 => Some(json!(remaining / total)),
                    _ => None,
                },
            );
            put("nextResetTime", next_reset.map(|v| number(v * 1000.0)));
            let details: Vec<Value> = capabilities
                .iter()
                .map(|code| {
                    let mut detail = json!({"modelCode": code});
                    if let Some(name) = &display_name {
                        detail["displayName"] = json!(name);
                    }
                    detail["usage"] = used.map(number).unwrap_or(json!(0));
                    detail
                })
                .collect();
            limit.insert("usageDetails".into(), Value::Array(details));
            Some(Value::Object(limit))
        })
        .collect()
}

fn start_plan_remaining(limits: &[Value]) -> Value {
    if limits.is_empty() {
        return Value::Null;
    }
    let total: f64 = limits
        .iter()
        .map(|limit| limit["number"].as_f64().unwrap_or(0.0))
        .sum();
    let remaining: f64 = limits
        .iter()
        .map(|limit| limit["remaining"].as_f64().unwrap_or(0.0))
        .sum();
    let mut resets: Vec<f64> = limits
        .iter()
        .filter_map(|limit| limit["nextResetTime"].as_f64())
        .collect();
    resets.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut value = json!({"count": number(remaining), "isShow": true});
    if total > 0.0 {
        value["percentage"] = json!(remaining / total);
    }
    value["nextResetTime"] = resets.first().map(|v| number(*v)).unwrap_or(Value::Null);
    value
}

fn start_plan_subscription(plans: &Value) -> Value {
    let details: Vec<Value> = plans
        .as_array()
        .into_iter()
        .flatten()
        .filter(|plan| non_empty(&plan["status"]).map(|s| s.to_lowercase()).as_deref() == Some("active"))
        .map(|plan| {
            let entitlements: Vec<Value> = plan["entitlements"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|entry| {
                    let id = non_empty(&entry["entitlement_id"])?;
                    Some(json!({
                        "entitlementId": id,
                        "showName": non_empty(&entry["show_name"]),
                        "effectiveTime": iso_from_seconds(&entry["effective_at"]),
                    }))
                })
                .collect();
            json!({
                "productId": non_empty(&plan["plan_id"]).unwrap_or_default(),
                "productName": non_empty(&plan["name"]).unwrap_or_else(|| "编程套餐".to_owned()),
                "purchaseTime": null,
                "beginTime": iso_from_seconds(&plan["starts_at"]),
                "billingCycle": plan["entitlements"].as_array().into_iter().flatten().find_map(|e| non_empty(&e["period"])),
                "renewTime": null,
                "expireTime": iso_from_seconds(&plan["ends_at"]),
                "entitlements": entitlements,
            })
        })
        .collect();
    json!({"identityType": "unknown", "identityMasked": null, "details": details})
}

/// `readSuccessfulBigModelMonitorData`。
fn monitor_data(payload: &Value, fallback: &str, allow_missing: bool) -> Result<Value, RpcError> {
    let message = || {
        let message = non_empty(&payload["msg"]).or_else(|| non_empty(&payload["message"]));
        error(message.unwrap_or_else(|| fallback.to_owned()))
    };
    if !bigmodel_envelope_ok(payload) {
        return Err(message());
    }
    let data = &payload["data"];
    // JS 真值：对象 / 数组即有数据。
    if data.is_object() || data.is_array() {
        return Ok(data.clone());
    }
    let explicit = payload["success"] == true
        || payload["code"]
            .as_f64()
            .is_some_and(|c| c == 0.0 || c == 200.0);
    if allow_missing && explicit {
        // 团队项目无工具调用时 monitor 返回 code=200 但省略 data：空统计，不中断页面。
        return Ok(json!({}));
    }
    Err(message())
}

/// `readBestEffortBigModelMonitorData`：传输失败降级为空，业务信封失败仍抛出。
fn best_effort(result: Result<Value, ApiError>, fallback: &str) -> Result<Value, RpcError> {
    match result {
        Ok(payload) => monitor_data(&payload, fallback, true),
        Err(failure) => {
            log::warn(format!(
                "[usage-stats] {fallback}，该区域降级为空统计 error={} status={:?}",
                failure.message, failure.status
            ));
            Ok(json!({}))
        }
    }
}

fn require_coding_plan(request: &Value) -> Result<(), RpcError> {
    let preferred = request["preferredProviderId"].as_str().unwrap_or("");
    if is_coding_plan_provider(preferred) {
        Ok(())
    } else {
        Err(error("no_bigmodel_api_key"))
    }
}

pub fn service(usage: UsageStats) -> ServiceBuilder {
    macro_rules! handler {
        (|$u:ident, $request:ident| $body:expr) => {{
            let shared = usage.clone();
            move |_: zcode_host_kit::CallContext, args: zcode_host_kit::Args| {
                let $u = shared.clone();
                async move {
                    let $request: Value = args.opt(0, "request")?.unwrap_or_else(|| json!({}));
                    $body
                }
            }
        }};
    }
    ServiceBuilder::new("usage-stats")
        .method(
            "getEntitlementSnapshot",
            handler!(|u, request| u.entitlement_snapshot(&request).await.map(Reply::Value)),
        )
        .method(
            "getAppUsageSnapshot",
            handler!(|u, request| {
                (u.app_usage)(json!({"range": request["range"], "timeZone": request["timeZone"]}))
                    .await
                    .map(Reply::Value)
            }),
        )
        .method(
            "getSnapshot",
            handler!(|u, request| u.usage_snapshot(&request).await.map(Reply::Value)),
        )
        .method(
            "getCodingPlanUsageSnapshot",
            handler!(|u, request| {
                // Coding Plan 页面只允许预置的 Coding Plan 账号，普通 provider 不进入 monitor 链路。
                require_coding_plan(&request)?;
                u.coding_plan_usage(&request).await.map(Reply::Value)
            }),
        )
        .method(
            "getCodingPlanResetStatus",
            handler!(|u, request| {
                require_coding_plan(&request)?;
                u.reset_status(&request).await.map(Reply::Value)
            }),
        )
        .method(
            "requestCodingPlanResetOpportunity",
            handler!(|u, request| {
                require_coding_plan(&request)?;
                u.request_opportunity(&request).await.map(Reply::Value)
            }),
        )
        .method(
            "useCodingPlanReset",
            handler!(|u, request| {
                require_coding_plan(&request)?;
                u.use_reset(&request).await.map(Reply::Value)
            }),
        )
        .method(
            "markCodingPlanResetHistoryRead",
            handler!(|u, request| {
                require_coding_plan(&request)?;
                u.mark_history_read(&request)
                    .await
                    .map(|()| Reply::Undefined)
            }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_limits_and_start_plan_mapping() {
        let limits = normalize_limits(&json!([
            {"type": "TOKENS_LIMIT", "remaining": 5, "usageDetails": [{"modelCode": "m", "displayName": " M "}, {"x": 1}]},
            {"type": "TIME_LIMIT", "percentage": 10, "nextResetTime": 99},
            {"type": ""},
        ]));
        assert_eq!(limits.len(), 2);
        assert_eq!(
            limits[0]["usageDetails"],
            json!([{"modelCode": "m", "displayName": "M", "usage": 0}])
        );
        assert_eq!(pick_primary_limit(&limits).unwrap()["type"], "TIME_LIMIT");
        let balances = json!([{"bucket_id": "b", "user_plan_id": "u", "plan_id": "p", "entitlement_id": "e1",
            "total_units": "100", "used_units": 40, "remaining_units": 60, "expires_at": 1000,
            "capabilities": ["model:GLM-5.3-Flash"], "show_name": "GLM-5.3-Flash"}]);
        let plans = json!([{"user_plan_id": "u", "plan_id": "p", "status": "active", "entitlements": [{"entitlement_id": "e1", "period": "daily"}]}]);
        let mapped = start_plan_limits(&balances, Some(&plans));
        assert_eq!(mapped[0]["period"], "daily");
        assert_eq!(mapped[0]["percentage"], json!(0.6));
        assert_eq!(mapped[0]["nextResetTime"], json!(1_000_000));
        assert_eq!(mapped[0]["number"], json!(100));
        let remaining = start_plan_remaining(&start_plan_limits(&balances, None));
        assert_eq!(
            remaining,
            json!({"count": 60, "isShow": true, "percentage": 0.6, "nextResetTime": 1_000_000})
        );
        assert_eq!(
            subscription_summary(&json!({"productId": "p", "productName": "GLM Coding Pro", "autoRenew": 0, "valid": "2026-01-01 00:00:00-2026-02-01"}))
                .unwrap()["expireTime"],
            "2026-02-01T00:00:00.000Z"
        );
    }
}
