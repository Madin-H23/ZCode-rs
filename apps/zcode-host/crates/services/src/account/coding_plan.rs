//! `coding-plan-subscription` channel（packages/services/src/coding-plan-subscription/**）。
//!
//! 套餐购买、试算、支付（支付宝 / Stripe / PayPal）、企业团队套餐与平台级 `client/configs`
//! （静态商品、Start Plan 预览、强制更新、闲时与动态工作流灰度）。都是对业务后端的透传，
//! 鉴权、域名路由、信封解包与错误归一化与 TS 相同。
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use serde_json::{Map, Value, json};
use zcode_host_kit::{Reply, RpcError, ServiceBuilder, log};

use super::auth::AccountApi;
use super::credential::CredentialStore;
use super::credential::node_platform;
use super::net::{ApiClient, ApiError, ApiRequest, node_arch};
use super::oauth_repo::{ZCODE_JWT_TOKEN_KEY, encode_uri_component};
use super::usage::is_zai_coding_plan_provider;
use crate::providers::ProviderRuntime;

const BIZ_PREFIX: &str = "/api/biz";
const ZAI_PAY_PREFIX: &str = "/api/pay";
const CLIENT_CONFIG_PATH: &str = "/api/v1/client/configs";
const REQUEST_TIMEOUT_MS: u64 = 15_000;
const CLIENT_CONFIG_TTL: Duration = Duration::from_secs(60 * 60);
const ZAI_OVERSEAS_PAYMENT_REQUIRED: &str = "coding_plan_zai_overseas_payment_required";
const SYSTEM_BUSY: &str = "coding_plan_system_busy";
const BIGMODEL_CODING_PLAN: &str = "account:bigmodel-individual-coding-plan";
const ZAI_CODING_PLAN: &str = "account:zai-individual-coding-plan";
const DYNAMIC_WORKFLOW_MODES: [&str; 3] = ["disabled", "onDemand", "alwaysOn"];

fn error(message: impl Into<String>) -> RpcError {
    RpcError::new("Error", message)
}

fn provider_name(provider_id: &str) -> &'static str {
    if is_zai_coding_plan_provider(provider_id) {
        "Z.ai"
    } else {
        "BigModel"
    }
}

/// `isUnrenderableRemoteErrorMessage`：HTML / WAF / 非 JSON 响应收敛为“系统繁忙”。
fn is_unrenderable(message: &str) -> bool {
    let normalized = message.trim().to_lowercase();
    if normalized.is_empty() {
        return false;
    }
    let html = regex::Regex::new(r"<\s*(html|head|body|script|style|title|meta)\b").expect("static regex");
    normalized.starts_with("<!doctype")
        || html.is_match(&normalized)
        || normalized.contains("errors.aliyun.com")
        || normalized.contains("request has been blocked")
        || normalized.contains("unexpected token '<'")
        || normalized.contains("unexpected end of json input")
        || normalized.contains("invalid json response")
}

fn remote_error(message: Option<&str>, provider_id: &str, code: Option<&Value>) -> String {
    let message = message.map(str::trim).filter(|m| !m.is_empty());
    if let Some(message) = message
        && is_unrenderable(message)
    {
        return SYSTEM_BUSY.to_owned();
    }
    match message {
        Some(message) => message.to_owned(),
        None => match code.filter(|code| !code.is_null() && code.as_f64() != Some(0.0)) {
            Some(code) => format!("{} request failed: {code}", provider_name(provider_id)),
            None => format!("{} request failed", provider_name(provider_id)),
        },
    }
}

/// `unwrapEnvelope`：code 只认 200（缺失也行），success=false 与缺 data 都是错误。
fn unwrap_envelope(payload: Value, provider_id: &str) -> Result<Value, RpcError> {
    let code = &payload["code"];
    if !code.is_null() && code.as_f64() != Some(200.0) {
        return Err(error(remote_error(payload["msg"].as_str(), provider_id, Some(code))));
    }
    if payload["success"] == false {
        return Err(error(remote_error(payload["msg"].as_str(), provider_id, None)));
    }
    match payload.get("data") {
        Some(data) if !data.is_null() => Ok(data.clone()),
        _ => Err(error(
            payload["msg"]
                .as_str()
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{} response missing data", provider_name(provider_id))),
        )),
    }
}

fn api_failure(failure: ApiError) -> RpcError {
    if is_unrenderable(&failure.message) {
        return error(SYSTEM_BUSY);
    }
    let mut rpc = RpcError::new("ApiError", failure.message);
    if let Some(status) = failure.status {
        rpc = rpc.with_data(json!({"status": status}));
    }
    rpc
}

/// `dropUndefined`：入参里没提供的字段（缺失或 null 作 undefined 的不算）不发送。
fn body(entries: &[(&str, Option<Value>)]) -> Value {
    let mut object = Map::new();
    for (key, value) in entries {
        if let Some(value) = value {
            object.insert((*key).to_owned(), value.clone());
        }
    }
    Value::Object(object)
}

/// 显式传入的字段（含 null）原样发送；缺失的不发送。
fn field(request: &Value, key: &str) -> Option<Value> {
    request.get(key).cloned()
}

/// `request.x ?? default`。
fn or(request: &Value, key: &str, default: Value) -> Option<Value> {
    Some(request.get(key).filter(|value| !value.is_null()).cloned().unwrap_or(default))
}

type ConfigFuture = Shared<BoxFuture<'static, Result<Value, RpcError>>>;

#[derive(Default)]
struct ClientConfigCache {
    snapshot: Option<(Value, Instant)>,
    request: Option<ConfigFuture>,
}

#[derive(Clone)]
pub struct CodingPlanSubscription {
    api: ApiClient,
    account_api: AccountApi,
    credentials: CredentialStore,
    providers: ProviderRuntime,
    cache: Arc<Mutex<ClientConfigCache>>,
}

struct Endpoint {
    provider_id: String,
    host: String,
    token: String,
}

impl CodingPlanSubscription {
    pub fn new(api: ApiClient, account_api: AccountApi, credentials: CredentialStore, providers: ProviderRuntime) -> Self {
        Self {
            api,
            account_api,
            credentials,
            providers,
            cache: Arc::new(Mutex::new(ClientConfigCache::default())),
        }
    }

    fn env(&self, key: &str) -> Option<String> {
        self.api.endpoints().env(key).map(str::to_owned)
    }

    fn bigmodel_host(&self) -> String {
        self.api.endpoints().bigmodel_api_origin()
    }

    fn zai_host(&self) -> String {
        self.api.endpoints().zai_business_base_url()
    }

    fn host_for(&self, provider_id: Option<&str>) -> String {
        if provider_id.is_some_and(is_zai_coding_plan_provider) {
            self.zai_host()
        } else {
            self.bigmodel_host()
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

    async fn bigmodel_token(&self) -> Result<String, RpcError> {
        let token = self.load_trimmed("oauth:bigmodel:access_token").await;
        if token.is_empty() {
            return Err(error("bigmodel_oauth_required"));
        }
        let jwt = self.load_trimmed(ZCODE_JWT_TOKEN_KEY).await;
        if !jwt.is_empty() && token == jwt {
            log::warn("[codingPlanSubscription] BigModel access token is stale zcode JWT; login required");
            return Err(error("bigmodel_oauth_required"));
        }
        Ok(token)
    }

    async fn zai_token(&self) -> Result<String, RpcError> {
        let token = self.load_trimmed("oauth:zai:access_token").await;
        if token.is_empty() {
            return Err(error("zai_oauth_required"));
        }
        Ok(token)
    }

    async fn endpoint(&self, provider_id: Option<&str>) -> Result<Endpoint, RpcError> {
        let provider_id = provider_id.unwrap_or(BIGMODEL_CODING_PLAN).to_owned();
        if is_zai_coding_plan_provider(&provider_id) {
            return Ok(Endpoint {
                token: self.zai_token().await?,
                host: self.zai_host(),
                provider_id,
            });
        }
        Ok(Endpoint {
            token: self.bigmodel_token().await?,
            host: self.bigmodel_host(),
            provider_id,
        })
    }

    async fn read(&self, request: ApiRequest) -> Result<Value, RpcError> {
        self.api.read_json(request).await.map_err(api_failure)
    }

    fn login_headers(request: ApiRequest, token: &str) -> ApiRequest {
        // 业务接口直接传登录 token，不加 Bearer。
        request
            .header("Authorization", token.to_owned())
            .header("Content-Type", "application/json")
            .timeout_ms(REQUEST_TIMEOUT_MS)
    }

    async fn post(&self, provider_id: Option<&str>, path: &str, payload: Value) -> Result<Value, RpcError> {
        let endpoint = self.endpoint(provider_id).await?;
        let request = Self::login_headers(
            ApiRequest::post(format!("{}{BIZ_PREFIX}{path}", endpoint.host)),
            &endpoint.token,
        )
        .json(&payload);
        unwrap_envelope(self.read(request).await?, &endpoint.provider_id)
    }

    async fn get(&self, provider_id: Option<&str>, url: String) -> Result<Value, RpcError> {
        let endpoint = self.endpoint(provider_id).await?;
        let request = Self::login_headers(ApiRequest::get(url), &endpoint.token);
        unwrap_envelope(self.read(request).await?, &endpoint.provider_id)
    }

    async fn zai_pay(&self, provider_id: Option<&str>, path: &str, payload: Option<Value>) -> Result<Value, RpcError> {
        let endpoint = self.endpoint(provider_id).await?;
        if !is_zai_coding_plan_provider(&endpoint.provider_id) {
            return Err(error(ZAI_OVERSEAS_PAYMENT_REQUIRED));
        }
        let url = format!("{}{ZAI_PAY_PREFIX}{path}", endpoint.host);
        let request = match payload {
            Some(payload) => Self::login_headers(ApiRequest::post(url), &endpoint.token).json(&payload),
            None => Self::login_headers(ApiRequest::get(url), &endpoint.token),
        };
        unwrap_envelope(self.read(request).await?, &endpoint.provider_id)
    }

    fn url_with(&self, base: &str, pairs: &[(&str, &str)]) -> String {
        let mut url = url::Url::parse(base).expect("coding plan base url");
        {
            let mut query = url.query_pairs_mut();
            for (key, value) in pairs {
                query.append_pair(key, value);
            }
        }
        url.to_string()
    }

    /// `client/configs`：1 小时快照，并发读取合并成一次请求。
    async fn client_configs(&self, force_refresh: bool) -> Result<Value, RpcError> {
        let future = {
            let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            if force_refresh {
                cache.snapshot = None;
            }
            if let Some((snapshot, expires)) = &cache.snapshot
                && *expires > Instant::now()
            {
                return Ok(snapshot.clone());
            }
            match &cache.request {
                Some(request) => request.clone(),
                None => {
                    let this = self.clone();
                    let future = async move {
                        let endpoints = this.api.endpoints();
                        let base = endpoints.zcode_api_url(CLIENT_CONFIG_PATH);
                        let url = this.url_with(
                            &base,
                            &[
                                ("app_version", &endpoints.app_version()),
                                ("platform", &format!("{}-{}", node_platform(), node_arch())),
                            ],
                        );
                        let result = this.read(ApiRequest::get(url).timeout_ms(REQUEST_TIMEOUT_MS)).await;
                        let mut cache = this.cache.lock().unwrap_or_else(|p| p.into_inner());
                        cache.request = None;
                        if let Ok(payload) = &result {
                            cache.snapshot = Some((payload.clone(), Instant::now() + CLIENT_CONFIG_TTL));
                        }
                        result
                    }
                    .boxed()
                    .shared();
                    cache.request = Some(future.clone());
                    future
                }
            }
        };
        future.await
    }

    fn config_failed(payload: &Value) -> Option<RpcError> {
        let code = &payload["code"];
        (!code.is_null() && code.as_f64() != Some(0.0)).then(|| {
            error(
                payload["msg"]
                    .as_str()
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                    .unwrap_or("ZCode client config request failed"),
            )
        })
    }

    /// 旧服务端目录仍用 builtin 键：只在配置边界转换。
    fn normalize_product_ids(products: &Value) -> Value {
        let mut normalized = products.as_object().cloned().unwrap_or_default();
        for (legacy, current) in [
            ("builtin:bigmodel-coding-plan", BIGMODEL_CODING_PLAN),
            ("builtin:zai-coding-plan", ZAI_CODING_PLAN),
        ] {
            if let Some(value) = normalized.shift_remove(legacy) {
                normalized.insert(current.to_owned(), value);
            }
        }
        Value::Object(normalized)
    }

    pub async fn static_products(&self) -> Result<Value, RpcError> {
        let payload = self.client_configs(false).await?;
        if let Some(failure) = Self::config_failed(&payload) {
            return Err(failure);
        }
        let products = &payload["data"]["configs"]["codingPlanStaticProducts"];
        if !products.is_object() {
            return Err(error("ZCode client config missing Coding Plan products"));
        }
        Ok(Self::normalize_product_ids(products))
    }

    pub async fn static_team_products(&self) -> Result<Value, RpcError> {
        let payload = self.client_configs(false).await?;
        if let Some(failure) = Self::config_failed(&payload) {
            return Err(failure);
        }
        let products = &payload["data"]["configs"]["codingPlanStaticTeamProducts"];
        let Some(map) = products.as_object() else {
            return Err(error("ZCode client config missing Coding Plan team products"));
        };
        let non_empty = |value: &Value| value.as_str().is_some_and(|s| !s.trim().is_empty());
        let copy_ok = |value: &Value| {
            value.is_null()
                || value.as_array().is_some_and(|items| {
                    items.iter().all(|item| {
                        non_empty(item)
                            || (non_empty(&item["text"]) && (item.get("tooltip").is_none() || item["tooltip"].is_string()))
                    })
                })
        };
        for list in map.values() {
            let valid = list.as_array().is_some_and(|items| {
                items.iter().all(|product| {
                    non_empty(&product["productId"])
                        && non_empty(&product["productName"])
                        && matches!(product["tier"].as_str(), Some("LITE" | "PRO" | "MAX"))
                        && matches!(product["subscribeMode"].as_str(), Some("CONTINUOUS" | "ONE_TIME"))
                        && matches!(product["subscribePeriod"].as_str(), Some("MONTHLY" | "QUARTERLY" | "YEARLY"))
                        && non_empty(&product["purchaseMethodName"])
                        && product["priceCurrency"] == "CNY"
                        && (product.get("equity").is_none() || copy_ok(&product["equity"]))
                        && (product.get("description").is_none() || copy_ok(&product["description"]))
                })
            });
            if !valid {
                return Err(error("ZCode client config has invalid Coding Plan team products"));
            }
        }
        Ok(Self::normalize_product_ids(products))
    }

    pub async fn start_plan_preview(&self) -> Result<Value, RpcError> {
        let payload = self.client_configs(false).await?;
        if let Some(failure) = Self::config_failed(&payload) {
            return Err(failure);
        }
        let preview = &payload["data"]["configs"]["startPlanPreview"];
        if preview.is_null() || preview == &json!(false) {
            return Ok(Value::Null);
        }
        if !preview["planId"].is_string() || !preview["name"].is_string() || !preview["entitlements"].is_array() {
            return Err(error("ZCode client config invalid Start Plan preview"));
        }
        let entitlements: Vec<Value> = preview["entitlements"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|entry| {
                entry["grantUnits"].is_number()
                    && ["meter", "period", "showName", "unitType"].iter().all(|key| entry[*key].is_string())
            })
            .cloned()
            .collect();
        Ok(json!({"planId": preview["planId"], "name": preview["name"], "entitlements": entitlements}))
    }

    pub async fn force_update(&self) -> Result<Value, RpcError> {
        let payload = self.client_configs(false).await?;
        if let Some(failure) = Self::config_failed(&payload) {
            return Err(failure);
        }
        let version = payload["data"]["configs"]["forceUpdate"]["minimalVersion"]
            .as_str()
            .map(str::trim)
            .filter(|v| !v.is_empty());
        Ok(match version {
            Some(version) => json!({"minimalVersion": version}),
            None => Value::Null,
        })
    }

    async fn off_peak_view(&self) -> Value {
        let (revision, resolved) = self.providers.snapshot().await;
        resolved.off_peak_model_selection_view(revision)
    }

    pub async fn off_peak_config(&self, force_refresh: bool) -> Result<Value, RpcError> {
        if self.env("ZCODE_OFFPEAK_MOCK").as_deref() == Some("1") {
            let view = self.off_peak_view().await;
            let has_models = view["providers"].as_array().into_iter().flatten().any(|p| p["models"].as_array().is_some_and(|m| !m.is_empty()));
            return Ok(json!({
                "enabled": has_models,
                "modelSelectionView": view,
                "codingPlanActive": self.env("ZCODE_OFFPEAK_MOCK_NO_PLAN").as_deref() != Some("1"),
            }));
        }
        let payload = self.client_configs(force_refresh).await?;
        let view = self.off_peak_view().await;
        let has_models = view["providers"].as_array().into_iter().flatten().any(|p| p["models"].as_array().is_some_and(|m| !m.is_empty()));
        Ok(json!({
            "enabled": payload["data"]["configs"]["offPeak"]["enable_offpeak_task"] == true && has_models,
            "modelSelectionView": view,
        }))
    }

    pub async fn dynamic_workflow_config(&self, force_refresh: bool) -> Value {
        let config = |mode: &str, source: &str| json!({"mode": mode, "enabled": mode != "disabled", "source": source});
        let override_mode = self
            .api
            .endpoints()
            .vars()
            .get("ZCODE_DYNAMIC_WORKFLOW_MODE")
            .map(|value| value.trim().to_owned())
            .filter(|value| DYNAMIC_WORKFLOW_MODES.contains(&value.as_str()));
        if let Some(mode) = override_mode {
            return config(&mode, "override");
        }
        match self.client_configs(force_refresh).await {
            Ok(payload) => match payload["data"]["configs"]["dynamicWorkflow"]["mode"]
                .as_str()
                .map(str::trim)
                .filter(|mode| DYNAMIC_WORKFLOW_MODES.contains(mode))
            {
                Some(mode) => config(mode, "remote"),
                None => config("disabled", "default"),
            },
            Err(failure) => {
                log::warn(format!(
                    "[codingPlanSubscription] 动态工作流灰度配置读取失败，按关闭处理: {}",
                    failure.message
                ));
                config("disabled", "default")
            }
        }
    }

    pub async fn enterprise_pricing(&self, request: &Value) -> Result<Value, RpcError> {
        let zai = request["family"] == "zai";
        let provider_id = if zai { ZAI_CODING_PLAN } else { BIGMODEL_CODING_PLAN };
        let host = if zai { self.zai_host() } else { self.bigmodel_host() };
        if request["authenticated"] == false {
            let payload = self
                .read(
                    ApiRequest::get(format!("{host}{BIZ_PREFIX}/campaign/partner/enterprise/pricing"))
                        .timeout_ms(REQUEST_TIMEOUT_MS),
                )
                .await?;
            return unwrap_envelope(payload, provider_id);
        }
        let token = if zai { self.zai_token().await? } else { self.bigmodel_token().await? };
        let response = unwrap_envelope(
            self.read(Self::login_headers(
                ApiRequest::get(format!("{host}{BIZ_PREFIX}/subscription/enterprise/v2/pricing")),
                &token,
            ))
            .await?,
            provider_id,
        )?;
        self.enrich_team_projects(response, &host, &token, provider_id).await
    }

    /// `enrichEnterprisePricingTeamProjects`：补全团队项目并预热每个团队项目的 API key。
    async fn enrich_team_projects(&self, mut response: Value, host: &str, token: &str, family: &str) -> Result<Value, RpcError> {
        let products: Vec<Value> = response["productList"].as_array().cloned().unwrap_or_default();
        if products.is_empty() {
            return Ok(response);
        }
        let trimmed = |value: &Value| value.as_str().map(str::trim).unwrap_or("").to_owned();
        let has_subscribed = products.iter().any(|p| p["subscribed"] == true);
        let missing_context = products.iter().any(|p| {
            p["subscribed"] == true && (trimmed(&p["projectId"]).is_empty() || trimmed(&p["organizationId"]).is_empty())
        });
        if !(has_subscribed && !missing_context) {
            let customer = self
                .read(Self::login_headers(
                    ApiRequest::get(format!("{host}{BIZ_PREFIX}/customer/getCustomerInfo")),
                    token,
                ))
                .await?;
            let mut team_projects = Vec::new();
            for organization in customer["data"]["organizations"].as_array().into_iter().flatten() {
                let organization_id = trimmed(&organization["organizationId"]);
                if organization_id.is_empty() {
                    continue;
                }
                for project in organization["projects"].as_array().into_iter().flatten() {
                    let project_id = trimmed(&project["projectId"]);
                    if project_id.is_empty() || super::auth::project_type(project) != "2" {
                        continue;
                    }
                    team_projects.push(json!({
                        "organizationId": organization_id,
                        "organizationName": organization.get("organizationName").cloned().unwrap_or(Value::Null),
                        "projectId": project_id,
                        "projectName": project.get("projectName").cloned().unwrap_or(Value::Null),
                    }));
                }
            }
            if team_projects.is_empty() {
                return Ok(response);
            }
            let fallback = if has_subscribed {
                None
            } else {
                let score = |p: &Value| {
                    (if p["tier"] == "PRO" { 100 } else { 0 })
                        + (if p["subscribeMode"] == "ONE_TIME" { 20 } else { 0 })
                        + (if p["subscribePeriod"] == "MONTHLY" { 10 } else { 0 })
                };
                let mut best: Option<&Value> = None;
                for product in &products {
                    if best.is_none_or(|current| score(product) > score(current)) {
                        best = Some(product);
                    }
                }
                best.map(|p| p["productId"].clone())
            };
            let enriched: Vec<Value> = products
                .into_iter()
                .map(|mut product| {
                    let enrich = product["subscribed"] == true
                        || fallback.as_ref().is_some_and(|id| product["productId"] == *id);
                    if !enrich {
                        return product;
                    }
                    let (org, project) = (trimmed(&product["organizationId"]), trimmed(&product["projectId"]));
                    let matched: Vec<Value> = team_projects
                        .iter()
                        .filter(|candidate| {
                            (org.is_empty() || candidate["organizationId"] == org.as_str())
                                && (project.is_empty() || candidate["projectId"] == project.as_str())
                        })
                        .cloned()
                        .collect();
                    let Some(primary) = matched.first().cloned() else {
                        return product;
                    };
                    for key in ["organizationId", "organizationName", "projectId", "projectName"] {
                        if !primary[key].is_null() {
                            product[key] = primary[key].clone();
                        }
                    }
                    product["teamProjects"] = Value::Array(matched);
                    product["subscribed"] = json!(true);
                    product
                })
                .collect();
            response["productList"] = Value::Array(enriched);
        }
        let statuses = self.prewarm_team_keys(&response, host, token, family).await;
        if statuses.is_empty() {
            return Ok(response);
        }
        let status_fields = |org: &str, project: &str| -> Option<Value> {
            let key = format!("{}:{}", org.trim(), project.trim());
            if org.trim().is_empty() || project.trim().is_empty() {
                return None;
            }
            statuses.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone())
        };
        if let Some(list) = response["productList"].as_array_mut() {
            for product in list {
                if let Some(projects) = product["teamProjects"].as_array_mut() {
                    for project in projects {
                        if let Some(fields) = status_fields(&trimmed(&project["organizationId"]), &trimmed(&project["projectId"])) {
                            for (key, value) in fields.as_object().into_iter().flatten() {
                                project[key] = value.clone();
                            }
                        }
                    }
                }
                if let Some(fields) = status_fields(&trimmed(&product["organizationId"]), &trimmed(&product["projectId"])) {
                    for (key, value) in fields.as_object().into_iter().flatten() {
                        product[key] = value.clone();
                    }
                }
            }
        }
        Ok(response)
    }

    async fn prewarm_team_keys(&self, response: &Value, host: &str, token: &str, family: &str) -> Vec<(String, Value)> {
        let mut contexts: Vec<(String, String)> = Vec::new();
        for product in response["productList"].as_array().into_iter().flatten() {
            if product["subscribed"] != true {
                continue;
            }
            let projects: Vec<&Value> = match product["teamProjects"].as_array().filter(|p| !p.is_empty()) {
                Some(projects) => projects.iter().collect(),
                None => vec![product],
            };
            for project in projects {
                let org = project["organizationId"].as_str().map(str::trim).unwrap_or("").to_owned();
                let id = project["projectId"].as_str().map(str::trim).unwrap_or("").to_owned();
                if org.is_empty() || id.is_empty() || contexts.contains(&(org.clone(), id.clone())) {
                    continue;
                }
                contexts.push((org, id));
            }
        }
        let mut statuses = Vec::new();
        for (org, project) in contexts {
            let status = match self
                .account_api
                .ensure_team_project_api_key(token, host, (&org, &project), REQUEST_TIMEOUT_MS)
                .await
            {
                Ok(Some(_)) => json!({"apiKeyStatus": "available", "apiKeyUnavailableReason": null, "apiKeyUnavailableMessage": null}),
                Ok(None) => json!({"apiKeyStatus": "unavailable", "apiKeyUnavailableReason": "request_failed", "apiKeyUnavailableMessage": null}),
                Err(failure) => {
                    log::warn(format!(
                        "[codingPlanSubscription] Team Plan project api key prewarm failed family={family}: {}",
                        failure.message
                    ));
                    json!({"apiKeyStatus": "unavailable", "apiKeyUnavailableReason": "request_failed", "apiKeyUnavailableMessage": failure.message})
                }
            };
            statuses.push((format!("{org}:{project}"), status));
        }
        statuses
    }
}

pub fn service(plan: CodingPlanSubscription) -> ServiceBuilder {
    macro_rules! handler {
        (|$p:ident, $request:ident| $body:expr) => {{
            let shared = plan.clone();
            move |_: zcode_host_kit::CallContext, args: zcode_host_kit::Args| {
                let $p = shared.clone();
                async move {
                    let $request: Value = args.opt(0, "request")?.unwrap_or_else(|| json!({}));
                    let result: Result<Value, RpcError> = $body;
                    result.map(Reply::Value)
                }
            }
        }};
    }
    let provider = |request: &Value| request["providerId"].as_str().map(str::to_owned);
    ServiceBuilder::new("coding-plan-subscription")
        .method("batchPreview", handler!(|p, r| p.post(provider(&r).as_deref(), "/pay/batch-preview", body(&[("invitationCode", field(&r, "invitationCode"))])).await))
        .method("getStaticProducts", handler!(|p, _r| p.static_products().await))
        .method("getStaticTeamProducts", handler!(|p, _r| p.static_team_products().await))
        .method("getStartPlanPreview", handler!(|p, _r| p.start_plan_preview().await))
        .method("getOffPeakClientConfig", handler!(|p, r| p.off_peak_config(r["forceRefresh"] == true).await))
        .method("getDynamicWorkflowClientConfig", handler!(|p, r| Ok(p.dynamic_workflow_config(r["forceRefresh"] == true).await)))
        .method("getModelContextBudgetStrategy", handler!(|_p, _r| Ok(json!("preflight-v1"))))
        .method("getForceUpdateConfig", handler!(|p, _r| p.force_update().await))
        .method("productInfo", handler!(|p, r| {
            let base = format!("{}{BIZ_PREFIX}/product/info", p.host_for(provider(&r).as_deref()));
            let url = p.url_with(&base, &[("productId", r["productId"].as_str().unwrap_or(""))]);
            p.get(provider(&r).as_deref(), url).await
        }))
        .method("preview", handler!(|p, r| {
            let id = provider(&r);
            let path = if id.as_deref().is_some_and(is_zai_coding_plan_provider) { "/pay/zai-preview" } else { "/pay/preview" };
            p.post(id.as_deref(), path, body(&[
                ("productId", field(&r, "productId")),
                ("invitationCode", field(&r, "invitationCode")),
                ("imRef", or(&r, "imRef", Value::Null)),
                ("ticket", or(&r, "ticket", Value::Null)),
                ("randstr", or(&r, "randstr", Value::Null)),
                ("salesChannel", or(&r, "salesChannel", json!("zcode"))),
            ])).await
        }))
        .method("createSign", handler!(|p, r| {
            if provider(&r).as_deref().is_some_and(is_zai_coding_plan_provider) {
                return Err(error(ZAI_OVERSEAS_PAYMENT_REQUIRED));
            }
            p.post(provider(&r).as_deref(), "/pay/create-sign", body(&[
                ("bizId", field(&r, "bizId")),
                ("payType", or(&r, "payType", json!("ALI"))),
                ("invitationCode", field(&r, "invitationCode")),
                ("renew", or(&r, "renew", Value::Null)),
                ("isDelay", or(&r, "isDelay", json!(0))),
                ("effectiveTime", or(&r, "effectiveTime", Value::Null)),
            ])).await
        }))
        .method("updateSign", handler!(|p, r| {
            if provider(&r).as_deref().is_some_and(is_zai_coding_plan_provider) {
                return Err(error(ZAI_OVERSEAS_PAYMENT_REQUIRED));
            }
            p.post(provider(&r).as_deref(), "/pay/product/update/sign", body(&[
                ("bizId", field(&r, "bizId")),
                ("payType", or(&r, "payType", json!("ALI"))),
            ])).await
        }))
        .method("checkPayment", handler!(|p, r| {
            let base = format!("{}{BIZ_PREFIX}/pay/check", p.host_for(provider(&r).as_deref()));
            let url = p.url_with(&base, &[("bizId", r["bizId"].as_str().unwrap_or(""))]);
            let status = p.get(provider(&r).as_deref(), url).await?;
            Ok(json!({"status": status}))
        }))
        .method("checkPendingOrders", handler!(|p, r| {
            let url = format!("{}{BIZ_PREFIX}/pay/check-pending-orders", p.host_for(provider(&r).as_deref()));
            p.get(provider(&r).as_deref(), url).await
        }))
        .method("queryStripeCards", handler!(|p, r| p.zai_pay(provider(&r).as_deref(), "/stripe/query", None).await))
        .method("bindStripeCard", handler!(|p, r| p.zai_pay(provider(&r).as_deref(), "/stripe/bind", Some(body(&[
            ("paymentMethodId", field(&r, "paymentMethodId")),
            ("returnUrl", field(&r, "returnUrl")),
            ("trackingContext", field(&r, "trackingContext")),
        ]))).await))
        .method("unbindStripeCard", handler!(|p, r| p.zai_pay(provider(&r).as_deref(), "/stripe/unbind", Some(body(&[
            ("paymentMethodId", field(&r, "paymentMethodId")),
        ]))).await))
        .method("payStripe", handler!(|p, r| p.zai_pay(provider(&r).as_deref(), "/stripe/pay", Some(body(&[
            ("productId", field(&r, "productId")),
            ("paymentMethodId", field(&r, "paymentMethodId")),
            ("isSubscribe", or(&r, "isSubscribe", json!(true))),
            ("returnUrl", field(&r, "returnUrl")),
            ("channelCode", field(&r, "channelCode")),
            ("estimatePayAmount", field(&r, "estimatePayAmount")),
            ("invitationCode", field(&r, "invitationCode")),
            ("bizId", field(&r, "bizId")),
            ("renew", or(&r, "renew", json!(false))),
        ]))).await))
        .method("checkPaypalSupport", handler!(|p, r| p.zai_pay(provider(&r).as_deref(), "/paypal/isSupport", None).await))
        .method("createPaypalSetupToken", handler!(|p, r| p.zai_pay(provider(&r).as_deref(), "/paypal/setupToken", Some(body(&[
            ("returnUrl", field(&r, "returnUrl")),
            ("cancelUrl", field(&r, "cancelUrl")),
        ]))).await))
        .method("subscribePaypal", handler!(|p, r| p.zai_pay(provider(&r).as_deref(), "/paypal/subscribe", Some(body(&[
            ("productId", field(&r, "productId")),
            ("productDesc", field(&r, "productDesc")),
            ("setupTokenId", field(&r, "setupTokenId")),
            ("amount", field(&r, "amount")),
            ("isSubscribe", or(&r, "isSubscribe", json!(true))),
            ("estimatePayAmount", field(&r, "estimatePayAmount")),
            ("invitationCode", field(&r, "invitationCode")),
            ("bizId", field(&r, "bizId")),
            ("channelCode", field(&r, "channelCode")),
        ]))).await))
        .method("getEnterprisePricing", handler!(|p, r| p.enterprise_pricing(&r).await))
        .method("getEnterpriseBalance", handler!(|p, _r| {
            let url = format!("{}{BIZ_PREFIX}/subscription/enterprise/v2/balance", p.bigmodel_host());
            p.get(Some(BIGMODEL_CODING_PLAN), url).await
        }))
        .method("calculateEnterpriseOrder", handler!(|p, r| p.post(Some(BIGMODEL_CODING_PLAN), "/subscription/enterprise/v2/order/calculate", r.clone()).await))
        .method("createEnterpriseOrder", handler!(|p, r| p.post(Some(BIGMODEL_CODING_PLAN), "/subscription/enterprise/v2/order", r.clone()).await))
        .method("getEnterprisePendingOrders", handler!(|p, _r| {
            let url = format!("{}{BIZ_PREFIX}/subscription/enterprise/v2/orders/pending", p.bigmodel_host());
            p.get(Some(BIGMODEL_CODING_PLAN), url).await
        }))
        .method("cancelEnterpriseOrder", handler!(|p, r| {
            let order = encode_uri_component(r["orderNo"].as_str().unwrap_or(""));
            p.post(Some(BIGMODEL_CODING_PLAN), &format!("/subscription/enterprise/v2/order/{order}/cancel"), json!({})).await
        }))
        .method("continueEnterpriseOrderPayment", handler!(|p, r| {
            let order = encode_uri_component(r["orderNo"].as_str().unwrap_or(""));
            p.post(Some(BIGMODEL_CODING_PLAN), &format!("/subscription/enterprise/v2/order/{order}/pay"), json!({})).await
        }))
        .method("checkEnterpriseOrderStatus", handler!(|p, r| {
            let order = encode_uri_component(r["orderNo"].as_str().unwrap_or(""));
            let url = format!("{}{BIZ_PREFIX}/subscription/enterprise/v2/order/{order}/status", p.bigmodel_host());
            p.get(Some(BIGMODEL_CODING_PLAN), url).await
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelopes_and_error_normalization_match_node() {
        assert_eq!(unwrap_envelope(json!({"code": 200, "data": {"a": 1}}), BIGMODEL_CODING_PLAN).unwrap(), json!({"a": 1}));
        assert_eq!(unwrap_envelope(json!({"data": "ok"}), BIGMODEL_CODING_PLAN).unwrap(), json!("ok"));
        assert_eq!(
            unwrap_envelope(json!({"code": 500}), ZAI_CODING_PLAN).unwrap_err().message,
            "Z.ai request failed: 500"
        );
        assert_eq!(
            unwrap_envelope(json!({"code": 1001, "msg": "<html><body>blocked"}), BIGMODEL_CODING_PLAN).unwrap_err().message,
            SYSTEM_BUSY
        );
        assert_eq!(
            unwrap_envelope(json!({"code": 200, "data": null}), BIGMODEL_CODING_PLAN).unwrap_err().message,
            "BigModel response missing data"
        );
        assert_eq!(
            body(&[("a", Some(json!(1))), ("b", None), ("c", Some(Value::Null))]),
            json!({"a": 1, "c": null})
        );
        let normalized = CodingPlanSubscription::normalize_product_ids(&json!({"builtin:zai-coding-plan": [], "x": [1]}));
        assert_eq!(normalized, json!({"x": [1], "account:zai-individual-coding-plan": []}));
    }
}
