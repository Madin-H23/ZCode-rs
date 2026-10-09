//! 旧协议 configOptions 投影（packages/shared/src/zcode-agent-model-state.ts
//! `zcodeSessionSettingsToZCodeConfigOptions`）与模型选择字符串（model-selection.ts、
//! custom-model-value.ts）。
use serde_json::{Map, Value, json};

pub const MODEL_CONFIG_ID: &str = "model";
pub const MODE_CONFIG_ID: &str = "mode";
pub const THOUGHT_LEVEL_CONFIG_ID: &str = "thought_level";
const REASONING_SEPARATOR: &str = "$";

const MODE_OPTIONS: &[(&str, &str, &str)] = &[
    (
        "build",
        "Ask before changes",
        "Ask before each file changes.",
    ),
    (
        "edit",
        "Edit automatically",
        "Edit selected files or relevant workspace files automatically.",
    ),
    (
        "plan",
        "Plan mode",
        "Inspect the code and present a plan before editing.",
    ),
    (
        "yolo",
        "Full access",
        "Edit and run commands with fewer confirmations.",
    ),
];

/// `formatModelPickerValue`：`provider/model[$reasoning]`，未绑定为空串。
pub fn format_model_picker_value(selection: &Value) -> String {
    let (Some(provider), Some(model)) = (
        selection["providerId"].as_str(),
        selection["modelId"].as_str(),
    ) else {
        return String::new();
    };
    match selection["options"]["reasoningLevel"]
        .as_str()
        .filter(|l| !l.is_empty())
    {
        Some(level) => format!("{provider}/{model}{REASONING_SEPARATOR}{level}"),
        None => format!("{provider}/{model}"),
    }
}

fn decode_uri_component(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(byte) = value
                .get(index + 1..index + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    // decodeURIComponent 失败时 TS 保留原值。
    String::from_utf8(out).unwrap_or_else(|_| value.to_owned())
}

/// adapter 的 `parseModelPickerValue`：先认 `custom:provider:model`，再按 `provider/model$level` 解析。
pub fn parse_model_picker_value(value: &str) -> Result<Value, String> {
    if let Some(body) = value.strip_prefix("custom:")
        && let Some(separator) = body.find(':')
    {
        let parts: Vec<&str> = body.split(':').collect();
        let (provider, model) = if parts.len() >= 3 && parts[0] == "builtin" {
            (
                format!("{}:{}", parts[0], parts[1]),
                decode_uri_component(&parts[2..].join(":")),
            )
        } else {
            (
                decode_uri_component(&body[..separator]),
                decode_uri_component(&body[separator + 1..]),
            )
        };
        if !provider.is_empty() && !model.is_empty() {
            return Ok(json!({"providerId": provider, "modelId": model}));
        }
    }
    let normalized = value.trim();
    let Some(provider_end) = normalized.find('/').filter(|index| *index > 0) else {
        return Err(format!("模型选择缺少 Provider: {normalized}"));
    };
    let provider = &normalized[..provider_end];
    let raw_model = &normalized[provider_end + 1..];
    match raw_model.find(REASONING_SEPARATOR) {
        Some(index) if index > 0 && index < raw_model.len() - 1 => Ok(json!({
            "providerId": provider,
            "modelId": &raw_model[..index],
            "options": {"reasoningLevel": &raw_model[index + 1..]},
        })),
        _ if raw_model.is_empty() => Err(format!("模型选择缺少 Model: {normalized}")),
        _ => Ok(json!({"providerId": provider, "modelId": raw_model})),
    }
}

/// adapter 的 `toZCodeMode`。
pub fn to_zcode_mode(mode: Option<&str>) -> Option<&'static str> {
    match mode? {
        "plan" => Some("plan"),
        "edit" => Some("edit"),
        "yolo" => Some("yolo"),
        "auto" => Some("auto"),
        "build" | "autoEdit" => Some("build"),
        _ => None,
    }
}

fn put_str(map: &mut Map<String, Value>, key: &str, value: &Value) {
    if let Some(text) = value.as_str() {
        map.insert(key.into(), json!(text));
    }
}

pub fn settings_to_config_options(settings: &Value) -> Value {
    let models: Vec<Value> = settings["model"]["available"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|model| {
            let levels: Option<Vec<Value>> = model["reasoning"]["levels"]
                .as_array()
                .map(|levels| levels.iter().map(|level| level["value"].clone()).collect());
            let default_level = model["reasoning"]["defaultLevel"]
                .as_str()
                .filter(|level| !level.is_empty())
                .filter(|level| levels.as_ref().is_some_and(|l| l.contains(&json!(level))));
            let mut option = Map::new();
            option.insert(
                "value".into(),
                json!(format_model_picker_value(&model["ref"])),
            );
            option.insert("name".into(), model["label"].clone());
            put_str(&mut option, "description", &model["description"]);
            option.insert("modelProviderId".into(), model["ref"]["providerId"].clone());
            let provider_name = model["providerLabel"]
                .as_str()
                .map(|label| json!(label))
                .unwrap_or_else(|| model["ref"]["providerId"].clone());
            option.insert("modelProviderName".into(), provider_name);
            if let Some(levels) = levels {
                option.insert("modelThoughtLevels".into(), Value::Array(levels));
            }
            if let Some(level) = default_level {
                option.insert("modelDefaultThoughtLevel".into(), json!(level));
            }
            Value::Object(option)
        })
        .collect();
    let mode = settings["mode"]["current"].as_str().unwrap_or_default();
    let mode = if MODE_OPTIONS.iter().any(|(id, _, _)| *id == mode) {
        mode
    } else {
        "build"
    };
    let mut options = vec![
        json!({
            "id": MODEL_CONFIG_ID,
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": format_model_picker_value(&settings["model"]["current"]),
            "options": models,
        }),
        json!({
            "id": MODE_CONFIG_ID,
            "name": "Mode",
            "category": "mode",
            "type": "select",
            "currentValue": mode,
            "options": MODE_OPTIONS
                .iter()
                .map(|(id, name, description)| json!({"value": id, "name": name, "description": description}))
                .collect::<Vec<_>>(),
        }),
    ];
    let thought = &settings["thoughtLevel"];
    if thought["enabled"] == true {
        let available = thought["available"].as_array().cloned().unwrap_or_default();
        let has = |level: &str| available.iter().any(|entry| entry["value"] == level);
        let default_level = thought["defaultLevel"]
            .as_str()
            .filter(|level| !level.is_empty() && has(level));
        let current = thought["current"]
            .as_str()
            .or(default_level)
            .or_else(|| available.first().and_then(|entry| entry["value"].as_str()))
            .unwrap_or("");
        options.push(json!({
            "id": THOUGHT_LEVEL_CONFIG_ID,
            "name": "Thought Level",
            "category": "thought_level",
            "type": "select",
            "currentValue": current,
            "options": available
                .iter()
                .map(|level| {
                    let mut option = Map::new();
                    option.insert("value".into(), level["value"].clone());
                    option.insert("name".into(), level["label"].clone());
                    put_str(&mut option, "description", &level["description"]);
                    Value::Object(option)
                })
                .collect::<Vec<_>>(),
        }));
    }
    Value::Array(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_values_round_trip() {
        let selection = json!({"providerId": "glm", "modelId": "m:free", "options": {"reasoningLevel": "high"}});
        assert_eq!(format_model_picker_value(&selection), "glm/m:free$high");
        assert_eq!(
            parse_model_picker_value("glm/m:free$high").unwrap(),
            selection
        );
        assert_eq!(
            parse_model_picker_value("custom:my%20p:gpt-5").unwrap(),
            json!({"providerId": "my p", "modelId": "gpt-5"})
        );
        assert!(parse_model_picker_value("nomodel").is_err());
        let options = settings_to_config_options(&json!({
            "model": {"current": {"providerId": "glm", "modelId": "a"}, "available": [
                {"ref": {"providerId": "glm", "modelId": "a"}, "label": "A",
                 "reasoning": {"levels": [{"value": "low"}], "defaultLevel": "low"}}]},
            "mode": {"current": "auto"},
            "thoughtLevel": {"enabled": true, "available": [{"value": "low", "label": "Low"}]},
        }));
        assert_eq!(options[0]["options"][0]["modelDefaultThoughtLevel"], "low");
        assert_eq!(options[1]["currentValue"], "build");
        assert_eq!(options[2]["currentValue"], "low");
    }
}
