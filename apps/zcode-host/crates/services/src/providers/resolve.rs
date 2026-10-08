//! 配置解析（packages/provider/src/resolver.ts、facades.ts，以及 Rust CLI 的
//! `crates/model/src/registry_rules.rs`）。
//!
//! 规则顺序与 CLI 的 resolver 一致：Host 列给 UI 的可选模型，必须正好是 CLI 能执行的模型，
//! 否则用户选中后 CLI 会拒绝。可执行性与 issues 按 TS view 的语义计算。
use std::collections::HashSet;

use serde_json::{Map, Value, json};
use zcode_cli_domain::option_map::{evaluate, validate_patches};

/// 模型规则：分组顺序决定覆盖顺序。
struct Rule {
    value: Value,
    personal: bool,
    manual: bool,
    model: Option<regex::Regex>,
    api: Option<regex::Regex>,
    url: Option<regex::Regex>,
}

fn array(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}

fn ids(value: &Value) -> Vec<String> {
    let mut seen = HashSet::new();
    array(value)
        .iter()
        .filter_map(Value::as_str)
        .filter(|id| seen.insert(id.to_owned()))
        .map(str::to_owned)
        .collect()
}

/// 配置子树递归覆盖；headers 等字典与叶子整体替换。access 的 type 是判别字段，换类型时不继承旧密钥。
pub fn overlay(base: &mut Value, next: &Value) {
    let Some(object) = next.as_object() else {
        *base = next.clone();
        return;
    };
    if !base.is_object() {
        *base = json!({});
    }
    for (key, value) in object {
        let nested = [
            "config",
            "api",
            "access",
            "properties",
            "optionSpecs",
            "inputFormat",
            "outputFormat",
            "reasoningLevel",
            "maxOutputTokens",
        ]
        .contains(&key.as_str());
        if nested && value.is_object() {
            if key == "access" && base[key]["type"] != value["type"] {
                base[key] = value.clone();
            } else {
                overlay(&mut base[key], value);
            }
        } else {
            base[key] = value.clone();
        }
    }
}

/// `resolveOwnedOrder`：未排序的内置 → 请求的顺序 → 未排序的个人。
fn owned_order(builtin: &[String], personal: &[String], requested: &Value) -> Vec<String> {
    let requested: Vec<String> = array(requested)
        .iter()
        .filter_map(Value::as_str)
        .filter(|id| builtin.iter().chain(personal).any(|known| known == id))
        .map(str::to_owned)
        .collect();
    let mut result: Vec<String> = Vec::new();
    for id in builtin
        .iter()
        .filter(|id| !requested.contains(id))
        .chain(&requested)
        .chain(personal.iter().filter(|id| !requested.contains(id)))
    {
        if !result.contains(id) {
            result.push(id.clone());
        }
    }
    result
}

fn rules(config: &Value, personal: bool) -> Vec<Rule> {
    let groups: &[&str] = if personal {
        &["providerModelRules", "manualProviderModelRules"]
    } else {
        &[
            "modelRules",
            "modelApiRules",
            "providerSiteRules",
            "templateModelRules",
            "builtinProviderModelRules",
        ]
    };
    let mut result = Vec::new();
    for group in groups {
        for rule in array(&config[*group]) {
            let regex = |key: &str, insensitive: bool| {
                rule[key].as_str().and_then(|pattern| {
                    regex::RegexBuilder::new(&format!("^(?:{pattern})$"))
                        .case_insensitive(insensitive)
                        .build()
                        .ok()
                })
            };
            result.push(Rule {
                value: rule.clone(),
                personal,
                manual: *group == "manualProviderModelRules",
                model: regex("modelMatch", true),
                api: regex("apiTypeMatch", false),
                url: regex("baseUrlMatch", false),
            });
        }
    }
    result
}

/// 手动规则只清除产品开放给用户编辑的叶子，系统参数映射继续继承。
fn clear_manual(value: &mut Value) {
    if let Some(properties) = value["properties"].as_object_mut() {
        for key in [
            "contextWindow",
            "supportsJsonSchemaOutput",
            "supportsNativeWebSearch",
            "supportsMidConversationSystem",
        ] {
            properties.remove(key);
        }
    }
    if let Some(input) = value["properties"]["inputFormat"].as_object_mut() {
        for key in ["supportsImage", "supportsVideo", "supportsPdf"] {
            input.remove(key);
        }
    }
    if let Some(specs) = value["optionSpecs"].as_object_mut() {
        specs.remove("reasoningLevel");
    }
    if let Some(max) = value["optionSpecs"]["maxOutputTokens"].as_object_mut() {
        max.remove("max");
    }
}

/// 完整模型配置：属性齐全、档位非空且唯一、预算为正、两个 option map 都能求值。
fn model_complete(config: &Value) -> bool {
    let properties = &config["properties"];
    let flags_ok = [
        "requiresMfjsToolSchema",
        "supportsToolCall",
        "supportsJsonSchemaOutput",
        "supportsNativeWebSearch",
        "supportsMidConversationSystem",
    ]
    .iter()
    .all(|key| properties[*key].is_boolean())
        && [
            "supportsText",
            "supportsImage",
            "supportsVideo",
            "supportsAudio",
            "supportsPdf",
        ]
        .iter()
        .all(|key| properties["inputFormat"][*key].is_boolean())
        && properties["outputFormat"]["supportsText"].is_boolean();
    let levels = &config["optionSpecs"]["reasoningLevel"]["values"];
    let levels_ok = levels.as_array().is_some_and(|values| {
        !values.is_empty()
            && values
                .iter()
                .all(|value| value.as_str().is_some_and(|text| !text.trim().is_empty()))
            && values
                .iter()
                .filter_map(Value::as_str)
                .collect::<HashSet<_>>()
                .len()
                == values.len()
    });
    let window = properties["contextWindow"].as_u64().unwrap_or(0);
    let max = config["optionSpecs"]["maxOutputTokens"]["max"]
        .as_u64()
        .unwrap_or(0);
    flags_ok && levels_ok && window > 0 && max > 0 && option_maps_valid(config)
}

fn option_maps_valid(config: &Value) -> bool {
    let specs = &config["optionSpecs"];
    let (Some(reasoning_map), Some(max_map)) = (
        specs["reasoningLevel"]["map"].as_str(),
        specs["maxOutputTokens"]["map"].as_str(),
    ) else {
        return false;
    };
    let max = specs["maxOutputTokens"]["max"].clone();
    array(&specs["reasoningLevel"]["values"])
        .iter()
        .all(|level| {
            let patches = [
                evaluate(reasoning_map, "reasoningLevel", level),
                evaluate(max_map, "maxOutputTokens", &max),
            ];
            match patches {
                [Ok(a), Ok(b)] => validate_patches(&[a, b]).is_ok(),
                _ => false,
            }
        })
}

fn issue(code: &str, path: &[&str], message: &str) -> Value {
    json!({"code": code, "path": path, "message": message})
}

/// 完整 provider 配置校验（provider-data-schema.ts）。
fn provider_issues(config: &Value, missing_template: bool) -> Vec<Value> {
    let mut issues = Vec::new();
    if missing_template {
        issues.push(issue(
            "missing-template",
            &["templateId"],
            "Provider template is missing",
        ));
    }
    if config["group"].as_str().is_none_or(str::is_empty) {
        issues.push(issue(
            "required-field-missing",
            &["group"],
            "Provider group is required",
        ));
    }
    match config["access"]["type"].as_str() {
        Some("zhipu-account") => {}
        Some("api-key" | "zhipu-coding-plan-api-key") => {
            if config["access"]["apiKey"]
                .as_str()
                .is_none_or(|key| key.trim().is_empty())
            {
                issues.push(issue(
                    "required-field-missing",
                    &["access", "apiKey"],
                    "API key is required",
                ));
            }
        }
        _ => issues.push(issue(
            "required-field-missing",
            &["access"],
            "Provider access is required",
        )),
    }
    if !matches!(
        config["api"]["type"].as_str(),
        Some("anthropic-messages" | "openai-chat-completions" | "openai-responses")
    ) {
        issues.push(issue(
            "required-field-missing",
            &["api", "type"],
            "API type is required",
        ));
    }
    let base_ok = config["api"]["baseUrl"]
        .as_str()
        .and_then(|base| url::Url::parse(base).ok())
        .is_some_and(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.username().is_empty()
                && url.password().is_none()
        });
    if !base_ok {
        issues.push(issue(
            "invalid-url",
            &["api", "baseUrl"],
            "Base URL is invalid",
        ));
    }
    issues
}

#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub model_id: String,
    pub builtin: bool,
    pub builtin_config: Value,
    pub personal_exact_config: Option<Value>,
    pub use_recommended_config: Option<bool>,
    pub effective_config: Value,
    pub enabled: bool,
    pub executable: bool,
    pub selectable: bool,
}

#[derive(Debug, Clone)]
pub struct ProviderEntry {
    pub provider_id: String,
    pub provider_name: Option<String>,
    pub template_id: Option<String>,
    pub template_config: Option<Value>,
    pub builtin_config: Option<Value>,
    pub personal_config: Option<Value>,
    pub effective_config: Value,
    pub issues: Vec<Value>,
    pub enabled: bool,
    pub executable: bool,
    pub hidden: bool,
    pub account_state: Option<Value>,
    pub models: Vec<ModelEntry>,
}

impl ProviderEntry {
    /// Registry 收录：无 issue 且至少一个可执行模型。
    pub fn in_registry(&self) -> bool {
        self.executable && self.issues.is_empty() && self.models.iter().any(|m| m.executable)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Resolved {
    pub providers: Vec<ProviderEntry>,
    pub templates: Vec<Value>,
    pub default_selection: Value,
}

/// 未登录时 account provider 的状态（account-provider-resolution.ts）。
fn logged_out_account_state(config: &Value) -> Value {
    if config["access"]["mode"].as_str() == Some("off-peak") {
        json!({"availability": "unavailable", "entitled": false})
    } else {
        json!({"availability": "unavailable", "entitled": false, "unavailableReason": "not-connected", "current": false})
    }
}

fn template_of<'a>(templates: &'a [Value], id: Option<&str>) -> Option<&'a Value> {
    let id = id?;
    templates
        .iter()
        .find(|template| template["templateId"] == id)
}

/// 三层解析：内置 → 账号 overlay（只作用于内置 provider，去掉 group）→ 个人。
/// 账号层缺失时 account provider 一律按未授权处理（fail-closed）。
pub fn resolve(
    builtin: &Value,
    personal: &Value,
    account_layer: Option<&super::AccountLayer>,
) -> Resolved {
    let templates: Vec<Value> = array(&builtin["providerConfigRules"]["templateRules"]).to_vec();
    let mut builtin_ids = Vec::new();
    let mut personal_ids = Vec::new();
    // id → (合并后的 rule, 内置层配置, 个人层配置, 缺模板)
    let mut merged: std::collections::HashMap<String, (Value, Option<Value>, Option<Value>, bool)> =
        std::collections::HashMap::new();

    for rule in array(&builtin["providerConfigRules"]["providerRules"]) {
        let id = text(rule, "providerId");
        if id.is_empty() || id == "builtin:zapi" || merged.contains_key(id) {
            continue;
        }
        builtin_ids.push(id.to_owned());
        let mut next = rule.clone();
        if next["config"]["access"]["type"] == "zhipu-account" {
            match account_layer.and_then(|layer| layer.providers.get(id)) {
                Some(overlay_config) => {
                    let mut overlay_config = overlay_config.clone();
                    if let Some(object) = overlay_config.as_object_mut() {
                        object.remove("group");
                    }
                    overlay(&mut next["config"], &overlay_config);
                }
                // 账号层未发布或没有该 provider 的结果：不授权。
                None => next["config"]["access"]["entitled"] = Value::Bool(false),
            }
        }
        let template = template_of(&templates, next["templateId"].as_str());
        let missing = next["templateId"].is_string() && template.is_none();
        if let Some(template) = template {
            let mut config = template["config"].clone();
            overlay(&mut config, &next["config"]);
            next["config"] = config;
        }
        let builtin_config = next["config"].clone();
        merged.insert(id.to_owned(), (next, Some(builtin_config), None, missing));
    }
    for rule in array(&personal["providerConfigRules"]["providerRules"]) {
        let id = text(rule, "providerId");
        if id.is_empty() || personal_ids.iter().any(|known| known == id) {
            continue;
        }
        personal_ids.push(id.to_owned());
        let personal_config = rule["config"].clone();
        let mut next = rule.clone();
        match merged.get_mut(id) {
            Some((current, _, personal_layer, _)) => {
                if let Some(config) = next["config"].as_object_mut() {
                    config.remove("group");
                }
                overlay(current, &next);
                *personal_layer = Some(personal_config);
            }
            None => {
                let template = template_of(&templates, next["templateId"].as_str());
                let missing = next["templateId"].is_string() && template.is_none();
                if let Some(template) = template {
                    let mut config = template["config"].clone();
                    overlay(&mut config, &next["config"]);
                    next["config"] = config;
                }
                merged.insert(id.to_owned(), (next, None, Some(personal_config), missing));
            }
        }
    }

    let mut model_rules = rules(&builtin["modelConfigRules"], false);
    model_rules.extend(rules(&personal["modelConfigRules"], true));

    let mut ordered = owned_order(&builtin_ids, &personal_ids, &personal["providerOrder"]);
    ordered.sort_by_key(|id| {
        let group = merged[id].0["config"]["group"].as_str();
        if matches!(group, Some("zai-family" | "bigmodel-family")) {
            0
        } else {
            1
        }
    });

    let mut providers = Vec::new();
    for id in ordered {
        let (rule, builtin_config, personal_config, missing_template) = &merged[&id];
        let config = &rule["config"];
        let account = config["access"]["type"] == "zhipu-account";
        let enabled = account || rule["enabled"] != false;
        let issues = provider_issues(config, *missing_template);
        let entitled = !account || config["access"]["entitled"] == true;
        let account_states = account_layer.map(|layer| &layer.states);
        // 非当前账号连接保留设置展示，但不向 Registry 发布模型（off-peak 不定义 current）。
        let account_current = account_states
            .and_then(|states| states.get(&id))
            .is_none_or(|state| state["current"] != false);
        let executable = enabled && entitled && account_current && issues.is_empty();
        let hidden = config["visibility"] == "hidden";
        let template = template_of(&templates, rule["templateId"].as_str());
        let base_url = config["api"]["baseUrl"]
            .as_str()
            .unwrap_or("")
            .trim_end_matches('/')
            .to_owned();
        let api_type = text(&config["api"], "type").to_owned();
        let builtin_models = ids(&config["builtinModelIds"]);
        let personal_models: Vec<String> = ids(&config["personalModelIds"])
            .into_iter()
            .filter(|model| !builtin_models.contains(model))
            .collect();
        let models = owned_order(&builtin_models, &personal_models, &config["modelOrder"])
            .into_iter()
            .map(|model_id| {
                let mut effective = json!({});
                let mut builtin_only = json!({});
                let mut personal_exact: Option<Value> = None;
                let mut use_recommended = None;
                for rule_entry in &model_rules {
                    let value = &rule_entry.value;
                    if value.get("providerId").is_some()
                        && (value["providerId"] != id.as_str()
                            || value["modelId"] != model_id.as_str())
                    {
                        continue;
                    }
                    if value.get("templateId").is_some()
                        && (value["templateId"] != rule["templateId"]
                            || value["modelId"] != model_id.as_str())
                    {
                        continue;
                    }
                    if rule_entry
                        .model
                        .as_ref()
                        .is_some_and(|re| !re.is_match(&model_id))
                        || rule_entry
                            .api
                            .as_ref()
                            .is_some_and(|re| !re.is_match(&api_type))
                        || rule_entry
                            .url
                            .as_ref()
                            .is_some_and(|re| !re.is_match(&base_url))
                    {
                        continue;
                    }
                    if rule_entry.manual {
                        clear_manual(&mut effective);
                    }
                    overlay(&mut effective, &value["config"]);
                    if rule_entry.personal {
                        let mut exact = personal_exact.unwrap_or_else(|| json!({}));
                        overlay(&mut exact, &value["config"]);
                        personal_exact = Some(exact);
                        use_recommended = Some(!rule_entry.manual);
                    } else {
                        overlay(&mut builtin_only, &value["config"]);
                    }
                }
                let model_enabled = effective["enabled"] == true;
                let model_executable = executable && model_enabled && model_complete(&effective);
                ModelEntry {
                    builtin: builtin_models.contains(&model_id),
                    model_id,
                    builtin_config: builtin_only,
                    personal_exact_config: personal_exact,
                    use_recommended_config: use_recommended,
                    effective_config: effective,
                    enabled: model_enabled,
                    executable: model_executable,
                    selectable: model_executable && !hidden,
                }
            })
            .collect();
        providers.push(ProviderEntry {
            provider_name: rule["providerName"].as_str().map(str::to_owned),
            template_id: rule["templateId"].as_str().map(str::to_owned),
            template_config: template.map(|template| template["config"].clone()),
            builtin_config: builtin_config.clone(),
            personal_config: personal_config.clone(),
            effective_config: config.clone(),
            account_state: match account_states {
                Some(states) => states.get(&id).cloned(),
                None => account.then(|| logged_out_account_state(config)),
            },
            provider_id: id,
            issues,
            enabled,
            executable,
            hidden,
            models,
        });
    }
    Resolved {
        providers,
        templates,
        default_selection: personal["defaultModelSelection"].clone(),
    }
}

fn reasoning_levels(model: &ModelEntry) -> Vec<String> {
    ids(&model.effective_config["optionSpecs"]["reasoningLevel"]["values"])
}

impl Resolved {
    fn visible_registry(&self) -> impl Iterator<Item = &ProviderEntry> {
        self.providers
            .iter()
            .filter(|provider| provider.in_registry() && !provider.hidden)
    }

    /// `ModelSelectionView`（facades.ts:519-563）；带 input 时附加 effectiveSelection / selectionIssue。
    pub fn model_selection_view(&self, revision: u64, input: Option<&Value>) -> Value {
        let providers: Vec<Value> = self
            .visible_registry()
            .map(|provider| {
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
            })
            .collect();
        let mut view = Map::new();
        view.insert("revision".into(), json!(revision));
        view.insert("providers".into(), Value::Array(providers));
        if let Some(preferred) = self.preferred_selection() {
            view.insert("preferredSelection".into(), preferred);
        }
        if let Some(input) = input {
            let (effective, issue) = self.effective_selection(&input["selection"]);
            view.insert("effectiveSelection".into(), effective);
            if let Some(issue) = issue {
                view.insert("selectionIssue".into(), json!(issue));
            }
        }
        Value::Object(view)
    }

    fn find_model(&self, provider_id: &str, model_id: &str) -> Option<&ModelEntry> {
        self.visible_registry()
            .find(|provider| provider.provider_id == provider_id)?
            .models
            .iter()
            .find(|model| model.executable && model.model_id == model_id)
    }

    fn preferred_selection(&self) -> Option<Value> {
        let default = &self.default_selection;
        if let (Some(provider_id), Some(model_id)) =
            (default["providerId"].as_str(), default["modelId"].as_str())
            && let Some(model) = self.find_model(provider_id, model_id)
            && let Some(level) = default["options"]["reasoningLevel"].as_str()
            && reasoning_levels(model).iter().any(|value| value == level)
        {
            return Some(
                json!({"providerId": provider_id, "modelId": model_id, "options": {"reasoningLevel": level}}),
            );
        }
        let provider = self.visible_registry().next()?;
        let model = provider.models.iter().find(|model| model.executable)?;
        let level = reasoning_levels(model).last()?.clone();
        Some(
            json!({"providerId": provider.provider_id, "modelId": model.model_id, "options": {"reasoningLevel": level}}),
        )
    }

    /// effective-model-selection.ts:17-77。
    fn effective_selection(&self, selection: &Value) -> (Value, Option<&'static str>) {
        let (Some(provider_id), Some(model_id)) = (
            selection["providerId"].as_str().map(str::trim),
            selection["modelId"].as_str().map(str::trim),
        ) else {
            return (Value::Null, Some("selection-missing"));
        };
        let Some(provider) = self
            .visible_registry()
            .find(|provider| provider.provider_id == provider_id)
        else {
            let account_plan =
                provider_id.starts_with("account:") && !provider_id.ends_with("start-plan");
            return (
                Value::Null,
                Some(if account_plan {
                    "account-connection-unavailable"
                } else {
                    "provider-not-found"
                }),
            );
        };
        let Some(model) = provider
            .models
            .iter()
            .find(|model| model.executable && model.model_id == model_id)
        else {
            return (Value::Null, Some("model-not-found"));
        };
        let levels = reasoning_levels(model);
        match selection["options"]["reasoningLevel"].as_str() {
            None => (Value::Null, Some("reasoning-level-missing")),
            Some(level) if !levels.iter().any(|value| value == level) => {
                (Value::Null, Some("reasoning-level-not-supported"))
            }
            Some(level) => (
                json!({"providerId": provider_id, "modelId": model_id, "options": {"reasoningLevel": level}}),
                None,
            ),
        }
    }

    /// `ProviderSettingsView`（facades.ts:605-688）。
    pub fn provider_settings_view(&self, revision: u64) -> Value {
        let visible: Vec<&ProviderEntry> = self
            .providers
            .iter()
            .filter(|provider| !provider.hidden)
            .collect();
        let provider_order: Vec<&str> = visible
            .iter()
            .filter(|provider| provider.effective_config["group"] == "standard-personal")
            .map(|provider| provider.provider_id.as_str())
            .collect();
        let providers: Vec<Value> = visible
            .iter()
            .map(|provider| {
                let mut view = Map::new();
                if let Some(name) = &provider.provider_name {
                    view.insert("providerName".into(), json!(name));
                }
                if let Some(template) = &provider.template_id {
                    view.insert("templateId".into(), json!(template));
                }
                view.insert("enabled".into(), json!(provider.enabled));
                if let Some(state) = &provider.account_state {
                    view.insert("accountState".into(), state.clone());
                }
                view.insert("providerId".into(), json!(provider.provider_id));
                view.insert("executable".into(), json!(provider.executable));
                if let Some(config) = &provider.builtin_config {
                    view.insert("effectiveBuiltinConfig".into(), config.clone());
                }
                if let Some(config) = &provider.personal_config {
                    view.insert("personalConfig".into(), config.clone());
                }
                view.insert("effectiveConfig".into(), provider.effective_config.clone());
                view.insert("issues".into(), Value::Array(provider.issues.clone()));
                view.insert(
                    "models".into(),
                    Value::Array(provider.models.iter().map(model_candidate_view).collect()),
                );
                if let Some(config) = &provider.template_config {
                    view.insert("templateConfig".into(), config.clone());
                }
                Value::Object(view)
            })
            .collect();
        let templates: Vec<Value> = self
            .templates
            .iter()
            .map(|template| {
                json!({
                    "templateId": template["templateId"],
                    "templateNameMap": template["templateNameMap"],
                    "config": template["config"],
                })
            })
            .collect();
        json!({
            "revision": revision,
            "providerTemplates": templates,
            "providerOrder": provider_order,
            "providers": providers,
        })
    }

    /// 至少一个可见 provider 有可执行模型（Host 启动 agent 前的门槛）。
    pub fn has_selectable_model(&self) -> bool {
        self.visible_registry().next().is_some()
    }

    /// 可见且进入 Registry 的 provider 数（provider-not-ready 错误的 `providerCount`）。
    pub fn registry_provider_count(&self) -> usize {
        self.visible_registry().count()
    }
}

fn model_candidate_view(model: &ModelEntry) -> Value {
    let mut view = Map::new();
    view.insert("kind".into(), json!("candidate"));
    view.insert("modelId".into(), json!(model.model_id));
    view.insert("builtin".into(), json!(model.builtin));
    view.insert(
        "effectiveBuiltinConfig".into(),
        model.builtin_config.clone(),
    );
    if let Some(config) = &model.personal_exact_config {
        view.insert("personalExactConfig".into(), config.clone());
    }
    if let Some(flag) = model.use_recommended_config {
        view.insert("useRecommendedConfig".into(), json!(flag));
    }
    view.insert("effectiveConfig".into(), model.effective_config.clone());
    view.insert("enabled".into(), json!(model.enabled));
    view.insert("executable".into(), json!(model.executable));
    view.insert("selectable".into(), json!(model.selectable));
    let issues: Vec<Value> = if model.enabled && !model_complete(&model.effective_config) {
        vec![issue(
            "required-field-missing",
            &["config"],
            "Model configuration is incomplete",
        )]
    } else {
        Vec::new()
    };
    view.insert("issues".into(), Value::Array(issues));
    Value::Object(view)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin() -> Value {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../../config/provider/zcode-builtin.json"
        ))
        .unwrap();
        serde_json::from_str::<Value>(&text).unwrap()["config"].clone()
    }

    fn byok() -> Value {
        json!({
            "providerOrder": ["new-provider"],
            "providerConfigRules": {"providerRules": [{
                "providerId": "new-provider", "providerName": "My Provider",
                "config": {
                    "group": "standard-personal",
                    "access": {"type": "api-key", "apiKey": "sk-test"},
                    "api": {"type": "openai-chat-completions", "baseUrl": "https://api.example.com/v1"},
                    "personalModelIds": ["my-model"], "modelOrder": ["my-model"],
                },
            }]},
            "modelConfigRules": {
                "providerModelRules": [{"providerId": "new-provider", "modelId": "my-model", "config": {"enabled": true}}],
                "manualProviderModelRules": [],
            },
        })
    }

    #[test]
    fn nothing_configured_lists_no_selectable_models() {
        let resolved = resolve(&builtin(), &json!({}), None);
        let view = resolved.model_selection_view(1, None);
        assert_eq!(view["providers"], json!([]));
        assert!(view.get("preferredSelection").is_none());
        let settings = resolved.provider_settings_view(1);
        assert_eq!(settings["providerTemplates"].as_array().unwrap().len(), 20);
        let accounts = settings["providers"].as_array().unwrap();
        assert_eq!(accounts.len(), 6, "visible account providers");
        assert!(accounts.iter().all(|p| p["executable"] == false));
        assert_eq!(
            accounts[0]["accountState"]["unavailableReason"],
            "not-connected"
        );
    }

    #[test]
    fn byok_provider_is_selectable_with_builtin_catch_all_rules() {
        let resolved = resolve(&builtin(), &byok(), None);
        let view = resolved.model_selection_view(1, None);
        let providers = view["providers"].as_array().unwrap();
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0]["providerId"], "new-provider");
        let model = &providers[0]["models"][0];
        assert_eq!(model["modelId"], "my-model");
        assert_eq!(
            model["config"]["optionSpecs"]["reasoningLevel"]["values"],
            json!(["disabled", "enabled"])
        );
        assert_eq!(
            view["preferredSelection"],
            json!({"providerId": "new-provider", "modelId": "my-model", "options": {"reasoningLevel": "enabled"}})
        );

        let with_input = resolved.model_selection_view(1, Some(&json!({"selection": null})));
        assert_eq!(with_input["effectiveSelection"], Value::Null);
        assert_eq!(with_input["selectionIssue"], "selection-missing");
        let chosen = json!({"selection": {"providerId": "new-provider", "modelId": "my-model", "options": {"reasoningLevel": "disabled"}}});
        let with_choice = resolved.model_selection_view(1, Some(&chosen));
        assert_eq!(with_choice["effectiveSelection"], chosen["selection"]);
        assert!(with_choice.get("selectionIssue").is_none());

        let settings = resolved.provider_settings_view(1);
        assert_eq!(settings["providerOrder"], json!(["new-provider"]));
        let personal = settings["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["providerId"] == "new-provider")
            .unwrap();
        assert_eq!(personal["executable"], true);
        assert_eq!(personal["models"][0]["useRecommendedConfig"], true);
        assert_eq!(
            personal["models"][0]["personalExactConfig"],
            json!({"enabled": true})
        );
    }

    #[test]
    fn missing_api_key_is_an_issue_and_not_selectable() {
        let mut personal = byok();
        personal["providerConfigRules"]["providerRules"][0]["config"]["access"]["apiKey"] =
            json!(" ");
        let resolved = resolve(&builtin(), &personal, None);
        assert_eq!(
            resolved.model_selection_view(1, None)["providers"],
            json!([])
        );
        let settings = resolved.provider_settings_view(1);
        let personal = settings["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["providerId"] == "new-provider")
            .unwrap();
        assert_eq!(personal["issues"][0]["path"], json!(["access", "apiKey"]));
    }
}
