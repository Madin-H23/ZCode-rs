//! Personal Provider Config 的写入（provider-settings 的写成员；packages/provider/src/config-service.ts、
//! config/*.ts 的 schema 与序列化、provider-node/src/personal-provider-config-repository.ts、
//! provider-config-file-codec.ts）。
//!
//! `<appConfigDir>/provider_config.json` 与 Node Host、CLI 共用：读改写全程持目录锁，锁内校验、
//! 规范化（键序同 TS 各 config 类的 `toJSON`）后私有原子写（`JSON.stringify(encoded, null, 2)`）。
use serde_json::{Map, Value, json};

pub type SchemaResult<T> = Result<T, String>;

fn invalid(path: &str, message: &str) -> String {
    format!("Invalid provider config at {path}: {message}")
}

fn object<'a>(value: &'a Value, path: &str) -> SchemaResult<&'a Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| invalid(path, "expected object"))
}

/// strict：只允许列出的键。
fn strict_keys(object: &Map<String, Value>, allowed: &[&str], path: &str) -> SchemaResult<()> {
    match object.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(invalid(path, &format!("Unrecognized key(s) in object: '{key}'"))),
        None => Ok(()),
    }
}

fn is_url(value: &str) -> bool {
    url::Url::parse(value).is_ok()
}

/// `nullable().optional()` 的字段：缺失为 None，null 原样保留。
fn sparse<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    object.get(key)
}

fn check_enum(value: &Value, allowed: &[&str], path: &str, nullable: bool) -> SchemaResult<()> {
    if value.is_null() && nullable {
        return Ok(());
    }
    match value.as_str() {
        Some(text) if allowed.contains(&text) => Ok(()),
        _ => Err(invalid(path, &format!("expected one of {allowed:?}"))),
    }
}

fn check_bool(value: &Value, path: &str, nullable: bool) -> SchemaResult<()> {
    if value.is_boolean() || (nullable && value.is_null()) {
        Ok(())
    } else {
        Err(invalid(path, "expected boolean"))
    }
}

fn check_positive_int(value: &Value, path: &str, nullable: bool) -> SchemaResult<()> {
    if nullable && value.is_null() {
        return Ok(());
    }
    let ok = value
        .as_f64()
        .is_some_and(|number| number > 0.0 && number.fract() == 0.0);
    if ok {
        Ok(())
    } else {
        Err(invalid(path, "expected positive integer"))
    }
}

fn check_ids(value: &Value, path: &str) -> SchemaResult<()> {
    if value.is_null() {
        return Ok(());
    }
    let ok = value
        .as_array()
        .is_some_and(|items| items.iter().all(|item| item.as_str().is_some_and(|s| !s.is_empty())));
    if ok {
        Ok(())
    } else {
        Err(invalid(path, "expected array of non-empty strings"))
    }
}

/// 选项映射（`optionMapSchema`）：能用字符串或数字输入求值即视为可编译。
fn check_option_map(value: &Value, variable: &str, path: &str) -> SchemaResult<()> {
    let Some(source) = value.as_str().filter(|source| !source.is_empty()) else {
        return Err(invalid(path, "expected non-empty option map"));
    };
    let compiles = [json!("x"), json!(1)].iter().any(|input| {
        zcode_cli_domain::option_map::evaluate(source, variable, input).is_ok()
    });
    if compiles {
        Ok(())
    } else {
        Err(invalid(path, "Option map 无法编译"))
    }
}

/// 只保留出现的键，按给定顺序输出（TS `objectWithoutUndefined({...})` 的键序）。
fn ordered(object: &Map<String, Value>, keys: &[&str], transform: impl Fn(&str, &Value) -> Value) -> Value {
    let mut result = Map::new();
    for key in keys {
        if let Some(value) = object.get(*key) {
            result.insert((*key).to_owned(), transform(key, value));
        }
    }
    Value::Object(result)
}

/// provider config 的校验上下文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderContext {
    /// `providerConfigDataSchema`（UI 传入的 overlay）。
    Input,
    /// personal 规则：不能有 builtinModelIds，group 只能 standard-personal，baseUrl 任意字符串。
    Personal,
}

fn validate_access(value: &Value, path: &str) -> SchemaResult<()> {
    if value.is_null() {
        return Ok(());
    }
    let access = object(value, path)?;
    match access.get("type").and_then(Value::as_str) {
        Some("api-key" | "zhipu-coding-plan-api-key") => {
            strict_keys(access, &["type", "apiKey", "apiKeyManagementUrl"], path)?;
            if let Some(key) = sparse(access, "apiKey")
                && !(key.is_null() || key.is_string())
            {
                return Err(invalid(&format!("{path}.apiKey"), "expected string"));
            }
            if let Some(url) = sparse(access, "apiKeyManagementUrl")
                && !(url.is_null() || url.as_str().is_some_and(is_url))
            {
                return Err(invalid(&format!("{path}.apiKeyManagementUrl"), "Invalid url"));
            }
            Ok(())
        }
        Some("zhipu-account") => {
            strict_keys(access, &["type", "accountType", "mode", "entitled"], path)?;
            if let Some(kind) = sparse(access, "accountType") {
                check_enum(kind, &["zai", "bigmodel"], &format!("{path}.accountType"), true)?;
            }
            if let Some(mode) = sparse(access, "mode") {
                check_enum(
                    mode,
                    &["start-plan", "individual-coding-plan", "team-coding-plan", "off-peak"],
                    &format!("{path}.mode"),
                    true,
                )?;
            }
            if let Some(entitled) = sparse(access, "entitled") {
                check_bool(entitled, &format!("{path}.entitled"), true)?;
            }
            Ok(())
        }
        _ => Err(invalid(&format!("{path}.type"), "Invalid discriminator value")),
    }
}

fn canonical_access(value: &Value) -> Value {
    let Some(access) = value.as_object() else {
        return value.clone();
    };
    if access["type"] == "zhipu-account" {
        ordered(access, &["type", "accountType", "mode", "entitled"], |_, v| v.clone())
    } else {
        ordered(access, &["type", "apiKey", "apiKeyManagementUrl"], |_, v| v.clone())
    }
}

fn validate_api(value: &Value, path: &str, personal: bool) -> SchemaResult<()> {
    if value.is_null() {
        return Ok(());
    }
    let api = object(value, path)?;
    strict_keys(api, &["type", "baseUrl", "headers"], path)?;
    if let Some(kind) = sparse(api, "type") {
        check_enum(
            kind,
            &["anthropic-messages", "openai-chat-completions", "openai-responses"],
            &format!("{path}.type"),
            true,
        )?;
    }
    if let Some(base) = sparse(api, "baseUrl") {
        let ok = base.is_null() || base.as_str().is_some_and(|url| personal || is_url(url));
        if !ok {
            return Err(invalid(&format!("{path}.baseUrl"), "Invalid url"));
        }
    }
    if let Some(headers) = sparse(api, "headers") {
        let ok = headers.is_null()
            || headers
                .as_object()
                .is_some_and(|map| map.values().all(Value::is_string));
        if !ok {
            return Err(invalid(&format!("{path}.headers"), "expected record of strings"));
        }
    }
    Ok(())
}

pub fn validate_provider_config(value: &Value, context: ProviderContext, path: &str) -> SchemaResult<()> {
    let config = object(value, path)?;
    let allowed: &[&str] = match context {
        ProviderContext::Input => &[
            "group",
            "logo",
            "access",
            "api",
            "builtinModelIds",
            "personalModelIds",
            "modelOrder",
            "visibility",
        ],
        ProviderContext::Personal => &[
            "group",
            "logo",
            "access",
            "api",
            "personalModelIds",
            "modelOrder",
            "visibility",
        ],
    };
    strict_keys(config, allowed, path)?;
    if let Some(group) = sparse(config, "group") {
        let groups: &[&str] = match context {
            ProviderContext::Input => &["standard-personal", "zai-family", "bigmodel-family"],
            ProviderContext::Personal => &["standard-personal"],
        };
        check_enum(group, groups, &format!("{path}.group"), true)?;
    }
    if let Some(logo) = sparse(config, "logo")
        && !logo.is_null()
    {
        let logo_object = object(logo, &format!("{path}.logo"))?;
        strict_keys(logo_object, &["type", "key"], &format!("{path}.logo"))?;
        if logo_object.get("type") != Some(&json!("builtin"))
            || !logo_object
                .get("key")
                .and_then(Value::as_str)
                .is_some_and(|key| !key.is_empty())
        {
            return Err(invalid(&format!("{path}.logo"), "invalid logo"));
        }
    }
    if let Some(access) = sparse(config, "access") {
        validate_access(access, &format!("{path}.access"))?;
    }
    if let Some(api) = sparse(config, "api") {
        validate_api(api, &format!("{path}.api"), context == ProviderContext::Personal)?;
    }
    for key in ["builtinModelIds", "personalModelIds", "modelOrder"] {
        if let Some(ids) = sparse(config, key) {
            check_ids(ids, &format!("{path}.{key}"))?;
        }
    }
    if let Some(visibility) = sparse(config, "visibility") {
        check_enum(visibility, &["visible", "hidden"], &format!("{path}.visibility"), true)?;
    }
    Ok(())
}

/// `ProviderConfig.toJSON` 键序。
pub fn canonical_provider_config(value: &Value) -> Value {
    let Some(config) = value.as_object() else {
        return value.clone();
    };
    ordered(
        config,
        &[
            "group",
            "logo",
            "access",
            "api",
            "builtinModelIds",
            "personalModelIds",
            "modelOrder",
            "visibility",
        ],
        |key, value| match key {
            "access" => canonical_access(value),
            "api" => match value.as_object() {
                Some(api) => ordered(api, &["type", "baseUrl", "headers"], |_, v| v.clone()),
                None => value.clone(),
            },
            "logo" => match value.as_object() {
                Some(logo) => ordered(logo, &["type", "key"], |_, v| v.clone()),
                None => value.clone(),
            },
            _ => value.clone(),
        },
    )
}

const INPUT_FORMAT_KEYS: [&str; 5] = [
    "supportsText",
    "supportsImage",
    "supportsVideo",
    "supportsAudio",
    "supportsPdf",
];
const PROPERTY_KEYS: [&str; 8] = [
    "requiresMfjsToolSchema",
    "contextWindow",
    "inputFormat",
    "outputFormat",
    "supportsToolCall",
    "supportsJsonSchemaOutput",
    "supportsNativeWebSearch",
    "supportsMidConversationSystem",
];

/// `modelConfigDataSchema`（稀疏）。
pub fn validate_model_config(value: &Value, path: &str) -> SchemaResult<()> {
    let config = object(value, path)?;
    strict_keys(config, &["enabled", "properties", "optionSpecs"], path)?;
    if let Some(enabled) = sparse(config, "enabled") {
        check_bool(enabled, &format!("{path}.enabled"), true)?;
    }
    if let Some(properties) = sparse(config, "properties")
        && !properties.is_null()
    {
        let path = format!("{path}.properties");
        let properties = object(properties, &path)?;
        strict_keys(properties, &PROPERTY_KEYS, &path)?;
        for key in [
            "requiresMfjsToolSchema",
            "supportsToolCall",
            "supportsJsonSchemaOutput",
            "supportsNativeWebSearch",
            "supportsMidConversationSystem",
        ] {
            if let Some(flag) = sparse(properties, key) {
                check_bool(flag, &format!("{path}.{key}"), true)?;
            }
        }
        if let Some(window) = sparse(properties, "contextWindow") {
            check_positive_int(window, &format!("{path}.contextWindow"), true)?;
        }
        for (key, allowed) in [
            ("inputFormat", &INPUT_FORMAT_KEYS[..]),
            ("outputFormat", &["supportsText"][..]),
        ] {
            if let Some(format) = sparse(properties, key)
                && !format.is_null()
            {
                let format_path = format!("{path}.{key}");
                let format = object(format, &format_path)?;
                strict_keys(format, allowed, &format_path)?;
                for flag in allowed {
                    if let Some(value) = sparse(format, flag) {
                        check_bool(value, &format!("{format_path}.{flag}"), true)?;
                    }
                }
            }
        }
    }
    if let Some(specs) = sparse(config, "optionSpecs")
        && !specs.is_null()
    {
        let path = format!("{path}.optionSpecs");
        let specs = object(specs, &path)?;
        strict_keys(specs, &["reasoningLevel", "maxOutputTokens"], &path)?;
        if let Some(reasoning) = sparse(specs, "reasoningLevel")
            && !reasoning.is_null()
        {
            let reasoning_path = format!("{path}.reasoningLevel");
            let reasoning = object(reasoning, &reasoning_path)?;
            strict_keys(reasoning, &["values", "map"], &reasoning_path)?;
            if let Some(values) = sparse(reasoning, "values")
                && !values.is_null()
            {
                let items = values
                    .as_array()
                    .ok_or_else(|| invalid(&reasoning_path, "values must be an array"))?;
                if items.is_empty() {
                    return Err(invalid(&reasoning_path, "reasoningLevel.values 不能为空"));
                }
                if !items
                    .iter()
                    .all(|item| item.as_str().is_some_and(|s| !s.trim().is_empty()))
                {
                    return Err(invalid(&reasoning_path, "reasoningLevel.values 必须是非空字符串"));
                }
                let unique: std::collections::HashSet<&str> =
                    items.iter().filter_map(Value::as_str).collect();
                if unique.len() != items.len() {
                    return Err(invalid(&reasoning_path, "reasoningLevel.values 不能重复"));
                }
            }
            if let Some(map) = sparse(reasoning, "map")
                && !map.is_null()
            {
                check_option_map(map, "reasoningLevel", &format!("{reasoning_path}.map"))?;
            }
        }
        if let Some(max) = sparse(specs, "maxOutputTokens")
            && !max.is_null()
        {
            let max_path = format!("{path}.maxOutputTokens");
            let max = object(max, &max_path)?;
            strict_keys(max, &["max", "map"], &max_path)?;
            if let Some(limit) = sparse(max, "max") {
                check_positive_int(limit, &format!("{max_path}.max"), true)?;
            }
            if let Some(map) = sparse(max, "map")
                && !map.is_null()
            {
                check_option_map(map, "maxOutputTokens", &format!("{max_path}.map"))?;
            }
        }
    }
    Ok(())
}

/// `manualModelConfigSchema`：只含可编辑叶子，且全部必填。
pub fn validate_manual_model_config(value: &Value, path: &str) -> SchemaResult<()> {
    validate_model_config(value, path)?;
    let config = object(value, path)?;
    let properties = config
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid(path, "manual properties are required"))?;
    strict_keys(
        properties,
        &[
            "contextWindow",
            "supportsJsonSchemaOutput",
            "supportsNativeWebSearch",
            "supportsMidConversationSystem",
            "inputFormat",
        ],
        &format!("{path}.properties"),
    )?;
    check_positive_int(&properties["contextWindow"], &format!("{path}.properties.contextWindow"), false)?;
    for key in [
        "supportsJsonSchemaOutput",
        "supportsNativeWebSearch",
        "supportsMidConversationSystem",
    ] {
        check_bool(&properties[key], &format!("{path}.properties.{key}"), false)?;
    }
    let input = properties["inputFormat"]
        .as_object()
        .ok_or_else(|| invalid(path, "manual inputFormat is required"))?;
    strict_keys(input, &["supportsImage", "supportsVideo", "supportsPdf"], path)?;
    for key in ["supportsImage", "supportsVideo", "supportsPdf"] {
        check_bool(&input[key], &format!("{path}.properties.inputFormat.{key}"), false)?;
    }
    let specs = config
        .get("optionSpecs")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid(path, "manual optionSpecs are required"))?;
    let reasoning = specs
        .get("reasoningLevel")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid(path, "manual reasoningLevel is required"))?;
    if !reasoning.get("values").is_some_and(Value::is_array) || !reasoning.get("map").is_some_and(Value::is_string) {
        return Err(invalid(path, "manual reasoningLevel must be complete"));
    }
    let max = specs
        .get("maxOutputTokens")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid(path, "manual maxOutputTokens is required"))?;
    strict_keys(max, &["max"], &format!("{path}.optionSpecs.maxOutputTokens"))?;
    check_positive_int(&max["max"], &format!("{path}.optionSpecs.maxOutputTokens.max"), false)?;
    Ok(())
}

/// 旧编辑器留下的完整 / 含 MFJS 的手动规则：只提取现行可编辑字段（`extractManualModelConfig`）。
pub fn extract_manual_model_config(value: &Value) -> Value {
    let mut result = Map::new();
    if let Some(enabled) = value.get("enabled") {
        result.insert("enabled".into(), enabled.clone());
    }
    if let Some(properties) = value["properties"].as_object() {
        let mut picked = Map::new();
        for key in [
            "contextWindow",
            "supportsJsonSchemaOutput",
            "supportsNativeWebSearch",
            "supportsMidConversationSystem",
        ] {
            if let Some(v) = properties.get(key) {
                picked.insert(key.into(), v.clone());
            }
        }
        if let Some(input) = properties.get("inputFormat").and_then(Value::as_object) {
            let mut format = Map::new();
            for key in ["supportsImage", "supportsVideo", "supportsPdf"] {
                if let Some(v) = input.get(key) {
                    format.insert(key.into(), v.clone());
                }
            }
            picked.insert("inputFormat".into(), Value::Object(format));
        }
        result.insert("properties".into(), Value::Object(picked));
    }
    if let Some(specs) = value["optionSpecs"].as_object() {
        let mut picked = Map::new();
        if let Some(reasoning) = specs.get("reasoningLevel") {
            picked.insert("reasoningLevel".into(), reasoning.clone());
        }
        if let Some(max) = specs.get("maxOutputTokens").and_then(Value::as_object) {
            let mut limit = Map::new();
            if let Some(v) = max.get("max") {
                limit.insert("max".into(), v.clone());
            }
            picked.insert("maxOutputTokens".into(), Value::Object(limit));
        }
        result.insert("optionSpecs".into(), Value::Object(picked));
    }
    Value::Object(result)
}

/// `ModelConfig.toJSON` 键序。
pub fn canonical_model_config(value: &Value) -> Value {
    let Some(config) = value.as_object() else {
        return value.clone();
    };
    ordered(config, &["enabled", "properties", "optionSpecs"], |key, value| {
        let Some(inner) = value.as_object() else {
            return value.clone();
        };
        match key {
            "properties" => ordered(inner, &PROPERTY_KEYS, |key, value| match (key, value.as_object()) {
                ("inputFormat", Some(format)) => ordered(format, &INPUT_FORMAT_KEYS, |_, v| v.clone()),
                ("outputFormat", Some(format)) => ordered(format, &["supportsText"], |_, v| v.clone()),
                _ => value.clone(),
            }),
            "optionSpecs" => ordered(inner, &["reasoningLevel", "maxOutputTokens"], |key, value| {
                match (key, value.as_object()) {
                    ("reasoningLevel", Some(spec)) => ordered(spec, &["values", "map"], |_, v| v.clone()),
                    ("maxOutputTokens", Some(spec)) => ordered(spec, &["max", "map"], |_, v| v.clone()),
                    _ => value.clone(),
                }
            }),
            _ => value.clone(),
        }
    })
}

/// `manualModelConfigSchema` 的输出键序（pick 后 extend）。
pub fn canonical_manual_model_config(value: &Value) -> Value {
    let Some(config) = value.as_object() else {
        return value.clone();
    };
    ordered(config, &["enabled", "properties", "optionSpecs"], |key, value| {
        let Some(inner) = value.as_object() else {
            return value.clone();
        };
        match key {
            "properties" => ordered(
                inner,
                &[
                    "contextWindow",
                    "supportsJsonSchemaOutput",
                    "supportsNativeWebSearch",
                    "supportsMidConversationSystem",
                    "inputFormat",
                ],
                |key, value| match (key, value.as_object()) {
                    ("inputFormat", Some(format)) => {
                        ordered(format, &["supportsImage", "supportsVideo", "supportsPdf"], |_, v| v.clone())
                    }
                    _ => value.clone(),
                },
            ),
            "optionSpecs" => ordered(inner, &["reasoningLevel", "maxOutputTokens"], |key, value| {
                match (key, value.as_object()) {
                    ("reasoningLevel", Some(spec)) => ordered(spec, &["values", "map"], |_, v| v.clone()),
                    ("maxOutputTokens", Some(spec)) => ordered(spec, &["max"], |_, v| v.clone()),
                    _ => value.clone(),
                }
            }),
            _ => value.clone(),
        }
    })
}

/// TS `ConfigOverlay` 语义：缺失继承，null 与叶子整体替换，嵌套 config 递归。
pub fn overlay_model_config(base: &Value, next: &Value) -> Value {
    let mut merged = base.clone();
    if !merged.is_object() {
        merged = json!({});
    }
    let Some(next) = next.as_object() else {
        return merged;
    };
    for (key, value) in next {
        let nested = matches!(
            key.as_str(),
            "properties" | "optionSpecs" | "inputFormat" | "outputFormat" | "reasoningLevel" | "maxOutputTokens"
        );
        if nested && value.is_object() && merged[key].is_object() {
            merged[key] = overlay_model_config(&merged[key], value);
        } else {
            merged[key] = value.clone();
        }
    }
    merged
}

/// 一条精确模型规则；`manual` 决定写进 providerModelRules 还是 manualProviderModelRules。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRule {
    pub manual: bool,
    pub provider_id: String,
    pub model_id: String,
    pub config: Value,
}

/// Personal 层（`ProviderConfigLayerUpdate`）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Personal {
    /// 规则：`{providerId, templateId?, providerName?, enabled?, config}`。
    pub providers: Vec<Value>,
    /// 精确模型规则，保留两组的交错顺序（`setExact` 原位替换）。
    pub models: Vec<ModelRule>,
    pub provider_order: Option<Vec<String>>,
    pub default_model_selection: Option<Value>,
}

fn id_string(value: &Value) -> bool {
    value.as_str().is_some_and(|s| !s.is_empty())
}

fn decode_provider_rule(rule: &Value, path: &str) -> SchemaResult<Value> {
    let object_rule = object(rule, path)?;
    strict_keys(
        object_rule,
        &["providerId", "templateId", "providerName", "enabled", "config"],
        path,
    )?;
    if !id_string(&rule["providerId"]) {
        return Err(invalid(&format!("{path}.providerId"), "expected non-empty string"));
    }
    for key in ["templateId", "providerName"] {
        if let Some(value) = object_rule.get(key)
            && !(value.is_null() || id_string(value))
        {
            return Err(invalid(&format!("{path}.{key}"), "expected non-empty string"));
        }
    }
    if let Some(enabled) = object_rule.get("enabled") {
        check_bool(enabled, &format!("{path}.enabled"), false)?;
    }
    validate_provider_config(&rule["config"], ProviderContext::Personal, &format!("{path}.config"))?;
    if rule["providerId"]
        .as_str()
        .is_some_and(|id| id.starts_with("account:"))
        && rule["config"].get("access").is_some()
    {
        return Err(invalid(
            &format!("{path}.config.access"),
            "固定 Account Provider 的 Access 只能由 ZCode Built-in Config 声明",
        ));
    }
    Ok(canonical_provider_rule(rule))
}

/// 规则键序（zod schema 顺序）：providerId、templateId、providerName、enabled、config。
pub fn canonical_provider_rule(rule: &Value) -> Value {
    let Some(object) = rule.as_object() else {
        return rule.clone();
    };
    ordered(
        object,
        &["providerId", "templateId", "providerName", "enabled", "config"],
        |key, value| {
            if key == "config" {
                canonical_provider_config(value)
            } else {
                value.clone()
            }
        },
    )
}

fn decode_model_rule(rule: &Value, manual: bool, path: &str) -> SchemaResult<ModelRule> {
    let object_rule = object(rule, path)?;
    strict_keys(object_rule, &["providerId", "modelId", "config"], path)?;
    if !id_string(&rule["providerId"]) || !id_string(&rule["modelId"]) {
        return Err(invalid(path, "providerId / modelId must be non-empty strings"));
    }
    let mut config = rule["config"].clone();
    if manual {
        if validate_manual_model_config(&config, &format!("{path}.config")).is_err() {
            // 旧完整形状 / 含 MFJS 的旧可编辑形状：提取现行可编辑字段，其余损坏仍报错。
            validate_model_config(&config, &format!("{path}.config"))?;
            let extracted = extract_manual_model_config(&config);
            validate_manual_model_config(&extracted, &format!("{path}.config"))?;
            config = extracted;
        }
    } else {
        validate_model_config(&config, &format!("{path}.config"))?;
    }
    Ok(ModelRule {
        manual,
        provider_id: rule["providerId"].as_str().unwrap_or("").to_owned(),
        model_id: rule["modelId"].as_str().unwrap_or("").to_owned(),
        config: canonical_model_config(&config),
    })
}

/// `decodeProviderConfigFile`。
pub fn decode(file: &Value) -> SchemaResult<Personal> {
    let root = object(file, "$")?;
    match root.get("schemaVersion") {
        None => return Err("Provider Config 缺少 schemaVersion".to_owned()),
        Some(version) => {
            let integer = version
                .as_f64()
                .filter(|v| v.fract() == 0.0 && *v >= 0.0)
                .ok_or_else(|| "Provider Config schemaVersion 必须是非负整数".to_owned())?;
            if integer > 1.0 {
                return Err(format!(
                    "Provider Config schemaVersion {integer} 高于当前支持的 1"
                ));
            }
            if integer < 1.0 {
                return Err(format!(
                    "缺少 Provider Config schemaVersion {integer} 到 {} 的迁移器",
                    integer + 1.0
                ));
            }
        }
    }
    strict_keys(root, &["schemaVersion", "config"], "$")?;
    let config = object(&file["config"], "$.config")?;
    strict_keys(
        config,
        &["providerOrder", "providerConfigRules", "modelConfigRules", "defaultModelSelection"],
        "$.config",
    )?;
    let provider_order = match config.get("providerOrder") {
        None => None,
        Some(order) => {
            let items = order
                .as_array()
                .filter(|items| items.iter().all(id_string))
                .ok_or_else(|| invalid("$.config.providerOrder", "expected array of ids"))?;
            Some(items.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        }
    };
    let rules = object(&file["config"]["providerConfigRules"], "$.config.providerConfigRules")?;
    strict_keys(rules, &["providerRules"], "$.config.providerConfigRules")?;
    let mut providers = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (index, rule) in file["config"]["providerConfigRules"]["providerRules"]
        .as_array()
        .ok_or_else(|| invalid("$.config.providerConfigRules.providerRules", "expected array"))?
        .iter()
        .enumerate()
    {
        let decoded = decode_provider_rule(rule, &format!("providerRules[{index}]"))?;
        let id = decoded["providerId"].as_str().unwrap_or("").to_owned();
        if !seen.insert(id.clone()) {
            return Err(invalid("providerRules", &format!("duplicate providerId: {id}")));
        }
        providers.push(decoded);
    }
    let models_value = &file["config"]["modelConfigRules"];
    let model_rules = object(models_value, "$.config.modelConfigRules")?;
    strict_keys(
        model_rules,
        &["providerModelRules", "manualProviderModelRules"],
        "$.config.modelConfigRules",
    )?;
    let mut models = Vec::new();
    for (key, manual) in [("providerModelRules", false), ("manualProviderModelRules", true)] {
        for (index, rule) in models_value[key]
            .as_array()
            .ok_or_else(|| invalid(&format!("$.config.modelConfigRules.{key}"), "expected array"))?
            .iter()
            .enumerate()
        {
            models.push(decode_model_rule(rule, manual, &format!("{key}[{index}]"))?);
        }
    }
    // 同一模型不能同时有智能与手动规则。
    for rule in models.iter().filter(|rule| rule.manual) {
        if models
            .iter()
            .any(|other| !other.manual && other.provider_id == rule.provider_id && other.model_id == rule.model_id)
        {
            return Err(invalid(
                "manualProviderModelRules",
                &format!("conflicting smart and manual rules: {}/{}", rule.provider_id, rule.model_id),
            ));
        }
    }
    let default_model_selection = match config.get("defaultModelSelection") {
        None => None,
        Some(selection) => {
            if !selection["providerId"].is_string() || !selection["modelId"].is_string() {
                return Err(invalid("$.config.defaultModelSelection", "invalid model selection"));
            }
            Some(selection.clone())
        }
    };
    Ok(Personal {
        providers,
        models,
        provider_order,
        default_model_selection,
    })
}

/// `encodeProviderConfigFile`。
pub fn encode(personal: &Personal) -> Value {
    let mut config = Map::new();
    if let Some(order) = &personal.provider_order {
        config.insert("providerOrder".into(), json!(order));
    }
    config.insert(
        "providerConfigRules".into(),
        json!({"providerRules": personal.providers.iter().map(canonical_provider_rule).collect::<Vec<_>>()}),
    );
    let collect = |manual: bool| -> Vec<Value> {
        personal
            .models
            .iter()
            .filter(|rule| rule.manual == manual)
            .map(|rule| {
                // toPersonalJSON 经 zod 输出：键序是 schema 顺序 modelId、config、providerId。
                let config = if rule.manual {
                    canonical_manual_model_config(&rule.config)
                } else {
                    canonical_model_config(&rule.config)
                };
                json!({
                    "modelId": rule.model_id,
                    "config": config,
                    "providerId": rule.provider_id,
                })
            })
            .collect()
    };
    config.insert(
        "modelConfigRules".into(),
        json!({"providerModelRules": collect(false), "manualProviderModelRules": collect(true)}),
    );
    if let Some(selection) = &personal.default_model_selection {
        config.insert("defaultModelSelection".into(), selection.clone());
    }
    json!({"schemaVersion": 1, "config": Value::Object(config)})
}
