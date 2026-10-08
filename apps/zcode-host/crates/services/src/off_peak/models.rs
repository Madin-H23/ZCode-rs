//! 闲时任务的凭据、支持矩阵与模型选择（offPeakRuntimeModel.ts、offPeakModelSelectionView.ts、
//! node.ts 的 `resolveModelSelection`、coding-plan 的 `resolveOffPeakClientConfig`）。
//!
//! Rust Host 还没有账号与 OAuth（R4）：真实凭据链（zhipu-account Provider、zcode JWT、Coding Plan
//! key）一律解析为 `connection_unavailable`。`ZCODE_OFFPEAK_MOCK=1` 时与 Node 相同，使用确定性的
//! mock 凭据，配合进程内 mock 网关跑通取号与派发。
use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::providers::resolve::{ProviderEntry, Resolved};

pub const OFF_PEAK_PROVIDER_ZAI: &str = "account:zai-offpeak-idle-plan";
pub const OFF_PEAK_PROVIDER_BIGMODEL: &str = "account:bigmodel-offpeak-idle-plan";
const BIGMODEL_INDIVIDUAL_CODING_PLAN: &str = "account:bigmodel-individual-coding-plan";

pub fn mock_enabled(vars: &BTreeMap<String, String>) -> bool {
    vars.get("ZCODE_OFFPEAK_MOCK").map(String::as_str) == Some("1")
}

fn mock_no_plan(vars: &BTreeMap<String, String>) -> bool {
    vars.get("ZCODE_OFFPEAK_MOCK_NO_PLAN").map(String::as_str) == Some("1")
}

/// 一次凭据解析的结果（`OffPeakCredentialSnapshot`）。
#[derive(Debug, Clone, PartialEq)]
pub struct Credentials {
    pub jwt: String,
    pub coding_plan_api_key: String,
    pub kind: &'static str,
    pub provider_family: &'static str,
    pub provider_id: String,
}

/// 凭据不可用（`OffPeakCodingPlanUnavailableError` / `OffPeakCredentialsUnavailableError`）。
#[derive(Debug, Clone, PartialEq)]
pub struct Unavailable {
    /// `OffPeakCodingPlanUnsupportedReason`。
    pub reason: &'static str,
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "off-peak selected coding plan unavailable: {}",
            self.reason
        )
    }
}

impl std::error::Error for Unavailable {}

/// `resolveOffPeakCredentials`。
pub fn resolve_credentials(vars: &BTreeMap<String, String>) -> Result<Credentials, Unavailable> {
    if mock_enabled(vars) {
        if mock_no_plan(vars) {
            return Err(Unavailable {
                reason: "connection_unavailable",
            });
        }
        return Ok(Credentials {
            jwt: "offpeak-mock-jwt".into(),
            coding_plan_api_key: "offpeak-mock-key".into(),
            kind: "bigmodel-personal",
            provider_family: "bigmodel",
            provider_id: BIGMODEL_INDIVIDUAL_CODING_PLAN.into(),
        });
    }
    // 账号 Provider（zhipu-account）在 Rust Host 里尚不可用：与 Node 未选中 Coding Plan 时相同。
    Err(Unavailable {
        reason: "connection_unavailable",
    })
}

/// `resolveOffPeakCodingPlanSupport`：renderer 可见的脱敏快照。
pub fn coding_plan_support(vars: &BTreeMap<String, String>) -> Value {
    match resolve_credentials(vars) {
        Ok(credentials) => json!({
            "supported": true,
            "kind": credentials.kind,
            "providerFamily": credentials.provider_family,
            "providerId": credentials.provider_id,
        }),
        Err(error) => json!({"supported": false, "reason": error.reason}),
    }
}

pub fn provider_for_family(family: &str) -> &'static str {
    if family == "zai" {
        OFF_PEAK_PROVIDER_ZAI
    } else {
        OFF_PEAK_PROVIDER_BIGMODEL
    }
}

fn registry_provider<'a>(resolved: &'a Resolved, provider_id: &str) -> Option<&'a ProviderEntry> {
    resolved
        .providers
        .iter()
        .find(|provider| provider.provider_id == provider_id && provider.in_registry())
}

/// `projectModelSelectionProviderView`：与 model-selection 视图同形。
fn project(provider: &ProviderEntry) -> Value {
    let mut view = Map::new();
    if let Some(name) = &provider.provider_name {
        view.insert("providerName".into(), json!(name));
    }
    if let Some(template) = &provider.template_id {
        view.insert("templateId".into(), json!(template));
    }
    view.insert("providerId".into(), json!(provider.provider_id));
    view.insert("config".into(), provider.effective_config.clone());
    view.insert(
        "models".into(),
        Value::Array(
            provider
                .models
                .iter()
                .filter(|model| model.executable)
                .map(|model| json!({"modelId": model.model_id, "config": model.effective_config}))
                .collect(),
        ),
    );
    Value::Object(view)
}

/// `buildOffPeakModelSelectionView`：只投影两个固定的隐藏 Provider。
pub fn model_selection_view(resolved: &Resolved, revision: u64) -> Value {
    let providers: Vec<Value> = resolved
        .providers
        .iter()
        .filter(|provider| {
            provider.in_registry()
                && provider.hidden
                && [OFF_PEAK_PROVIDER_ZAI, OFF_PEAK_PROVIDER_BIGMODEL]
                    .contains(&provider.provider_id.as_str())
        })
        .map(project)
        .collect();
    json!({"revision": revision, "providers": providers})
}

/// `resolveOffPeakClientConfig` 的 mock 分支：灰度由 mock 直接替代远端曝光配置。
pub fn mock_client_config(vars: &BTreeMap<String, String>, view: Value) -> Value {
    let has_models = view["providers"].as_array().is_some_and(|providers| {
        providers.iter().any(|provider| {
            provider["models"]
                .as_array()
                .is_some_and(|models| !models.is_empty())
        })
    });
    json!({
        "enabled": has_models,
        "modelSelectionView": view,
        "codingPlanActive": !mock_no_plan(vars),
    })
}

fn reasoning_levels(config: &Value) -> Vec<String> {
    config["optionSpecs"]["reasoningLevel"]["values"]
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// `registryService.validateSelection` 的判定：Provider 在 Registry、模型可执行、档位受支持。
pub fn is_valid_selection(resolved: &Resolved, selection: &Value) -> bool {
    let (Some(provider_id), Some(model_id)) = (
        selection["providerId"].as_str(),
        selection["modelId"].as_str(),
    ) else {
        return false;
    };
    let Some(model) = registry_provider(resolved, provider_id).and_then(|provider| {
        provider
            .models
            .iter()
            .find(|model| model.executable && model.model_id == model_id)
    }) else {
        return false;
    };
    let levels = reasoning_levels(&model.effective_config);
    match selection["options"]["reasoningLevel"].as_str() {
        Some(level) => levels.iter().any(|value| value == level),
        None => levels.is_empty(),
    }
}

/// node.ts 的 `resolveModelSelection`：固定 Off-Peak Provider + 输入模型（缺省取第一个）。
pub fn resolve_model_selection(
    resolved: &Resolved,
    vars: &BTreeMap<String, String>,
    model_id: Option<&str>,
    reasoning_level: Option<&str>,
) -> Option<Value> {
    let credentials = resolve_credentials(vars).ok()?;
    let provider = registry_provider(resolved, provider_for_family(credentials.provider_family))?;
    let model_id = match model_id {
        Some(model_id) => model_id.to_owned(),
        None => provider.models.first()?.model_id.clone(),
    };
    let mut selection = Map::new();
    selection.insert("providerId".into(), json!(provider.provider_id));
    selection.insert("modelId".into(), json!(model_id));
    if let Some(level) = reasoning_level {
        selection.insert("options".into(), json!({"reasoningLevel": level}));
    }
    let selection = Value::Object(selection);
    is_valid_selection(resolved, &selection).then_some(selection)
}

/// `completeNewModelSelection`：取模型档位列表的最后一个（最高档）。
pub fn complete_new_selection(view: &Value, provider_id: &str, model_id: &str) -> Option<Value> {
    let model = view["providers"]
        .as_array()?
        .iter()
        .find(|provider| provider["providerId"] == provider_id)?["models"]
        .as_array()?
        .iter()
        .find(|model| model["modelId"] == model_id)?
        .clone();
    let level = reasoning_levels(&model["config"]).pop()?;
    Some(
        json!({"providerId": provider_id, "modelId": model_id, "options": {"reasoningLevel": level}}),
    )
}

/// `buildOffPeakRequestAuth`：一次闲时执行的动态鉴权材料。
pub fn request_auth(credentials: &Credentials, ticket_id: &str) -> Value {
    json!({
        "apiKey": credentials.jwt,
        "headers": {
            "Authorization": format!("Bearer {}", credentials.jwt),
            "X-Coding-Plan-Api-Key": credentials.coding_plan_api_key,
            "X-Off-Peak-Ticket-ID": ticket_id,
        },
    })
}
