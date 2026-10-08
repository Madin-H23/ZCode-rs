//! Subagent 的模型选择：结构化 `ModelSelection` 校验、Markdown `model` 字段的读写，以及旧 Provider
//! 写法的一次性迁移（packages/shared/src/model-selection.ts、custom-model-value.ts、
//! subagent-markdown-selection.ts、subagent-state-migration.ts、legacy-model-provider-identity.ts）。
use serde_json::{Map, Value, json};
use zcode_cli_domain::js_string::trim as js_trim;

const INHERIT_NAMES: [&str; 5] = ["inherit", "main", "sonnet", "opus", "haiku"];

/// `modelSelectionSchema.safeParse(value).data`：strict，字符串 trim 后非空。
pub fn parse_selection(value: Option<&Value>) -> Option<Value> {
    let map = value?.as_object()?;
    if map.keys().any(|key| !matches!(key.as_str(), "providerId" | "modelId" | "options")) {
        return None;
    }
    let text = |value: Option<&Value>| {
        let trimmed = js_trim(value?.as_str()?);
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    };
    let provider = text(map.get("providerId"))?;
    let model = text(map.get("modelId"))?;
    let mut out = Map::new();
    out.insert("providerId".into(), json!(provider));
    out.insert("modelId".into(), json!(model));
    if let Some(options) = map.get("options") {
        let options = options.as_object()?;
        if options.keys().any(|key| key != "reasoningLevel") {
            return None;
        }
        let mut parsed = Map::new();
        if let Some(level) = options.get("reasoningLevel") {
            parsed.insert("reasoningLevel".into(), json!(text(Some(level))?));
        }
        out.insert("options".into(), Value::Object(parsed));
    }
    Some(Value::Object(out))
}

/// `normalizeSubagentModelSelection`：只保留 reasoningLevel，非法时报 schema 错误。
pub fn normalize_selection(selection: Option<&Value>) -> Result<Option<Value>, String> {
    let Some(selection) = selection.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let level = selection
        .get("options")
        .and_then(|options| options.get("reasoningLevel"))
        .filter(|level| level.as_str().is_some_and(|text| !text.is_empty()))
        .cloned();
    let mut candidate = Map::new();
    candidate.insert(
        "providerId".into(),
        selection.get("providerId").cloned().unwrap_or(Value::Null),
    );
    candidate.insert(
        "modelId".into(),
        selection.get("modelId").cloned().unwrap_or(Value::Null),
    );
    if let Some(level) = level {
        candidate.insert("options".into(), json!({ "reasoningLevel": level }));
    }
    parse_selection(Some(&Value::Object(candidate)))
        .map(Some)
        .ok_or_else(|| "Invalid model selection".to_owned())
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// JS `decodeURIComponent`；URIError 返回 `None`。
pub fn decode_uri_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    let escaped = |at: usize| -> Option<u8> {
        if bytes.get(at) != Some(&b'%') {
            return None;
        }
        Some(hex(*bytes.get(at + 1)?)? << 4 | hex(*bytes.get(at + 2)?)?)
    };
    while at < bytes.len() {
        if bytes[at] != b'%' {
            out.push(bytes[at]);
            at += 1;
            continue;
        }
        let lead = escaped(at)?;
        at += 3;
        let width = match lead {
            0x00..=0x7f => 1,
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => return None,
        };
        let mut sequence = vec![lead];
        for _ in 1..width {
            let next = escaped(at)?;
            if next & 0xc0 != 0x80 {
                return None;
            }
            sequence.push(next);
            at += 3;
        }
        std::str::from_utf8(&sequence).ok()?;
        out.extend_from_slice(&sequence);
    }
    String::from_utf8(out).ok()
}

/// JS `encodeURIComponent`。
pub fn encode_uri_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn safe_decode(value: &str) -> String {
    decode_uri_component(value).unwrap_or_else(|| value.to_owned())
}

/// `decodeCustomModelValue`：（providerId，modelName）。
pub fn decode_custom(value: &str) -> Option<(String, Option<String>)> {
    let body = value.strip_prefix("custom:")?;
    let Some(separator) = body.find(':') else {
        return Some((safe_decode(body), None));
    };
    let parts: Vec<&str> = body.split(':').collect();
    if parts.len() >= 3 && parts[0] == "builtin" {
        return Some((
            format!("{}:{}", parts[0], parts[1]),
            Some(safe_decode(&parts[2..].join(":"))),
        ));
    }
    Some((
        safe_decode(&body[..separator]),
        Some(safe_decode(&body[separator + 1..])),
    ))
}

pub fn encode_custom(provider: &str, model: &str) -> String {
    if model.is_empty() {
        return format!("custom:{}", encode_uri_component(provider));
    }
    format!(
        "custom:{}:{}",
        encode_uri_component(provider),
        encode_uri_component(model)
    )
}

/// `parseModelPickerValue`（失败返回 `None`）。
fn parse_picker(value: &str) -> Option<Value> {
    let normalized = js_trim(value);
    let separator = normalized.find('/').filter(|index| *index > 0)?;
    let provider = &normalized[..separator];
    let raw_model = &normalized[separator + 1..];
    match raw_model.find('$') {
        Some(index) if index > 0 && index < raw_model.len() - 1 => parse_selection(Some(&json!({
            "providerId": provider,
            "modelId": &raw_model[..index],
            "options": { "reasoningLevel": &raw_model[index + 1..] },
        }))),
        _ => parse_selection(Some(&json!({ "providerId": provider, "modelId": raw_model }))),
    }
}

/// `parseSubagentMarkdownSelection`：frontmatter 的 `model` + `thoughtLevel`。
pub fn parse_markdown_selection(model: Option<&Value>, thought_level: Option<&Value>) -> Option<Value> {
    let value = js_trim(model?.as_str()?);
    if value.is_empty() || INHERIT_NAMES.contains(&value) {
        return None;
    }
    let mut selection = match decode_custom(value) {
        Some((provider, model)) => {
            let provider = js_trim(&provider).to_owned();
            let model = js_trim(model.as_deref().unwrap_or("")).to_owned();
            if provider.is_empty() || model.is_empty() {
                return None;
            }
            json!({ "providerId": provider, "modelId": model })
        }
        None => parse_picker(value)?,
    };
    let level = thought_level
        .and_then(Value::as_str)
        .map(js_trim)
        .unwrap_or("");
    if !level.is_empty() {
        selection["options"] = json!({ "reasoningLevel": level });
    }
    Some(selection)
}

/// `formatSubagentMarkdownModel`。
pub fn format_markdown_model(selection: &Value) -> String {
    let provider = selection["providerId"].as_str().unwrap_or("");
    let model = selection["modelId"].as_str().unwrap_or("");
    if provider.starts_with("custom:") || provider.contains('/') || model.contains('$') {
        encode_custom(provider, model)
    } else {
        format!("{provider}/{model}")
    }
}

/// `migrateLegacyModelProviderId`：未知的 `builtin:*` 无法迁移（`None`）。
pub fn migrate_provider_id(provider: &str) -> Option<String> {
    Some(
        match provider {
            "builtin:bigmodel" => "bigmodel-api",
            "builtin:zai" => "zai-api",
            "builtin:bigmodel-start-plan" => "account:bigmodel-start-plan",
            "builtin:zai-start-plan" => "account:zai-start-plan",
            "builtin:bigmodel-coding-plan" => "account:bigmodel-individual-coding-plan",
            "builtin:zai-coding-plan" => "account:zai-individual-coding-plan",
            other if other.starts_with("builtin:") => return None,
            other => other,
        }
        .to_owned(),
    )
}

const OFFICIAL_GLM_IDS: [&str; 22] = [
    "GLM-5.3",
    "GLM-5.3-Flash",
    "GLM-5V-Turbo",
    "GLM-5.2",
    "GLM-5.1",
    "GLM-5.1-Highspeed",
    "GLM-5",
    "GLM-5-Turbo",
    "GLM-4.7",
    "GLM-4.7-FlashX",
    "GLM-4.7-Flash",
    "GLM-4.6",
    "GLM-4.5-Air",
    "GLM-4.5",
    "GLM-4.6V",
    "GLM-4.6V-Flash",
    "GLM-4.6V-FlashX",
    "GLM-4.1V-Thinking-FlashX",
    "GLM-4.1V-Thinking-Flash",
    "GLM-4-FlashX-250414",
    "GLM-4-Flash-250414",
    "GLM-4V-Flash",
];

fn official_glm_provider(provider: &str) -> bool {
    let builtin = provider.strip_prefix("builtin:").is_some_and(|rest| {
        ["zai", "bigmodel"].iter().any(|site| {
            rest == *site
                || rest == format!("{site}-start-plan")
                || rest == format!("{site}-coding-plan")
        })
    });
    let account = provider.strip_prefix("account:").is_some_and(|rest| {
        ["zai", "bigmodel"].iter().any(|site| {
            ["start-plan", "individual-coding-plan", "team-coding-plan"]
                .iter()
                .any(|plan| rest == format!("{site}-{plan}"))
        })
    });
    builtin || account
}

/// `migrateLegacyOfficialGlmModelId`。
pub fn migrate_glm_model_id(provider: &str, model: &str) -> String {
    if !official_glm_provider(provider) {
        return model.to_owned();
    }
    let lower = model.to_lowercase();
    OFFICIAL_GLM_IDS
        .iter()
        .find(|id| id.to_lowercase() == lower)
        .map(|id| (*id).to_owned())
        .unwrap_or_else(|| model.to_owned())
}

fn migrate_model_value(value: &str) -> String {
    if value.starts_with("custom:") {
        let Some((provider, Some(model))) = decode_custom(value) else {
            return value.to_owned();
        };
        if model.is_empty() || !provider.starts_with("builtin:") {
            return value.to_owned();
        }
        let Some(next_provider) = migrate_provider_id(&provider).filter(|id| *id != provider) else {
            return value.to_owned();
        };
        // 未改名的模型保留编码原文（含 %24、%2F），不把模型名再当 Picker 解析。
        let model_id = migrate_glm_model_id(&provider, &model);
        let body = &value["custom:".len()..];
        let separator = if body.starts_with("builtin:") {
            body["builtin:".len()..].find(':').map(|index| index + "builtin:".len())
        } else {
            body.find(':')
        };
        let Some(separator) = separator else {
            return value.to_owned();
        };
        let tail = if model_id == model {
            body[separator + 1..].to_owned()
        } else {
            encode_uri_component(&model_id)
        };
        return format!("custom:{}:{tail}", encode_uri_component(&next_provider));
    }
    let Some(separator) = value.find('/').filter(|index| *index >= 1) else {
        return value.to_owned();
    };
    let old_provider = &value[..separator];
    if !old_provider.starts_with("builtin:") {
        return value.to_owned();
    }
    let model_id = &value[separator + 1..];
    // 旧 Picker 的 $ 后是档位，不是型号。
    let (name, suffix) = match model_id.find('$') {
        Some(index) => (&model_id[..index], &model_id[index..]),
        None => (model_id, ""),
    };
    match migrate_provider_id(old_provider) {
        Some(provider) => format!(
            "{provider}/{}{suffix}",
            migrate_glm_model_id(old_provider, name)
        ),
        None => value.to_owned(),
    }
}

/// `/^(?:﻿)?---[ \t]*\r?\n([\s\S]*?)\r?\n---[ \t]*(?:\r?\n|$)/u`：（正文起点，正文终点，整段长度）。
fn markdown_frontmatter(content: &str) -> Option<(usize, usize, usize)> {
    let start = if content.starts_with('\u{feff}') { 3 } else { 0 };
    let rest = &content[start..];
    let rest = rest.strip_prefix("---")?;
    let trimmed = rest.trim_start_matches([' ', '\t']);
    let after_ws = start + 3 + (rest.len() - trimmed.len());
    let body_start = if trimmed.starts_with("\r\n") {
        after_ws + 2
    } else if trimmed.starts_with('\n') {
        after_ws + 1
    } else {
        return None;
    };
    let bytes = content.as_bytes();
    let mut position = body_start;
    while position <= bytes.len() {
        let newline = match bytes.get(position) {
            Some(b'\r') if bytes.get(position + 1) == Some(&b'\n') => Some(position + 2),
            Some(b'\n') => Some(position + 1),
            _ => None,
        };
        if let Some(after) = newline
            && content[after..].starts_with("---")
        {
            let tail = &content[after + 3..];
            let spaces = tail.len() - tail.trim_start_matches([' ', '\t']).len();
            let rest = &tail[spaces..];
            let end = after + 3 + spaces;
            if rest.is_empty() {
                return Some((body_start, position, end));
            }
            if rest.starts_with("\r\n") {
                return Some((body_start, position, end + 2));
            }
            if rest.starts_with('\n') {
                return Some((body_start, position, end + 1));
            }
        }
        position += 1;
    }
    None
}

/// `/^("(?:\\.|[^"\\])*"|'(?:''|[^'])*'|[^#]*?)([ \t]+#.*|[ \t]*)$/u`。
fn split_scalar(raw: &str) -> Option<(String, String)> {
    let suffix_ok = |rest: &str| {
        let trimmed = rest.trim_start_matches([' ', '\t']);
        trimmed.is_empty() || (trimmed.len() < rest.len() && trimmed.starts_with('#'))
    };
    let chars: Vec<(usize, char)> = raw.char_indices().collect();
    if raw.starts_with('"') {
        let mut index = 1;
        while index < chars.len() {
            match chars[index].1 {
                '\\' if index + 1 < chars.len() => index += 2,
                '\\' => break,
                '"' => {
                    let end = chars[index].0 + 1;
                    if suffix_ok(&raw[end..]) {
                        return Some((raw[..end].to_owned(), raw[end..].to_owned()));
                    }
                    break;
                }
                _ => index += 1,
            }
        }
    }
    if raw.starts_with('\'') {
        let mut index = 1;
        while index < chars.len() {
            if chars[index].1 == '\'' {
                if chars.get(index + 1).is_some_and(|(_, c)| *c == '\'') {
                    index += 2;
                    continue;
                }
                let end = chars[index].0 + 1;
                if suffix_ok(&raw[end..]) {
                    return Some((raw[..end].to_owned(), raw[end..].to_owned()));
                }
                break;
            }
            index += 1;
        }
    }
    let mut boundaries: Vec<usize> = chars.iter().map(|(at, _)| *at).collect();
    boundaries.push(raw.len());
    for at in boundaries {
        if raw[..at].contains('#') {
            break;
        }
        if suffix_ok(&raw[at..]) {
            return Some((raw[..at].to_owned(), raw[at..].to_owned()));
        }
    }
    None
}

/// `migrateSubagentMarkdownProvider`：只替换 frontmatter 单行 `model` 的 Provider，不重建 YAML。
pub fn migrate_markdown_provider(content: &str) -> String {
    let Some((body_start, body_end, _)) = markdown_frontmatter(content) else {
        return content.to_owned();
    };
    let body = &content[body_start..body_end];
    let mut migrated = String::with_capacity(body.len());
    let mut first = true;
    for line in body.split('\n') {
        if !first {
            migrated.push('\n');
        }
        first = false;
        let (text, carriage) = match line.strip_suffix('\r') {
            Some(text) => (text, "\r"),
            None => (line, ""),
        };
        migrated.push_str(&migrate_model_line(text));
        migrated.push_str(carriage);
    }
    format!("{}{}{}", &content[..body_start], migrated, &content[body_end..])
}

fn migrate_model_line(line: &str) -> String {
    let Some(after_key) = line.strip_prefix("model") else {
        return line.to_owned();
    };
    let before_colon = after_key.trim_start_matches([' ', '\t']);
    let Some(after_colon) = before_colon.strip_prefix(':') else {
        return line.to_owned();
    };
    let value_part = after_colon.trim_start_matches([' ', '\t']);
    let prefix = &line[..line.len() - value_part.len()];
    let Some((token, suffix)) = split_scalar(value_part) else {
        return line.to_owned();
    };
    let value = if token.starts_with('"') {
        match serde_json::from_str::<String>(&token) {
            Ok(value) => value,
            Err(_) => return line.to_owned(),
        }
    } else if token.starts_with('\'') {
        token[1..token.len() - 1].replace("''", "'")
    } else {
        token.clone()
    };
    let next = migrate_model_value(&value);
    if next == value {
        return line.to_owned();
    }
    let encoded = if token.starts_with('"') {
        serde_json::to_string(&next).unwrap_or_default()
    } else if token.starts_with('\'') {
        format!("'{}'", next.replace('\'', "''"))
    } else {
        next
    };
    format!("{prefix}{encoded}{suffix}")
}

fn record(value: Option<&Value>) -> Map<String, Value> {
    value.and_then(Value::as_object).cloned().unwrap_or_default()
}

/// `parsePluginSubagentModelSelectionOverrides`。
pub fn parse_plugin_overrides(value: Option<&Value>) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(map) = value.and_then(Value::as_object) {
        for (id, candidate) in map {
            if id.starts_with("plugin:")
                && let Some(selection) = parse_selection(Some(candidate))
            {
                out.insert(id.clone(), selection);
            }
        }
    }
    out
}

fn migrate_selection(selection: Value, from_legacy: bool) -> Value {
    let provider = selection["providerId"].as_str().unwrap_or("").to_owned();
    let model = selection["modelId"].as_str().unwrap_or("").to_owned();
    let migrated = if from_legacy && provider.starts_with("builtin:") {
        migrate_provider_id(&provider)
    } else {
        Some(provider.clone())
    };
    match migrated.filter(|id| !id.is_empty()) {
        Some(id) => {
            let mut next = selection;
            next["providerId"] = json!(id);
            next["modelId"] = json!(if from_legacy {
                migrate_glm_model_id(&provider, &model)
            } else {
                model
            });
            next
        }
        None => selection,
    }
}

/// `importSubagentStateSelections`：只在存储导入时解释旧双 map。
pub fn import_state_selections(input: &Map<String, Value>) -> Map<String, Value> {
    let current = input.contains_key("builtInModelSelectionOverrides");
    let mut selections = Map::new();
    for name in ["Explore", "general-purpose"] {
        let selection = if current {
            parse_selection(record(input.get("builtInModelSelectionOverrides")).get(name))
        } else {
            parse_markdown_selection(
                record(input.get("builtInModelOverrides")).get(name),
                record(input.get("builtInThoughtLevelOverrides")).get(name),
            )
        };
        if let Some(selection) = selection {
            selections.insert(name.to_owned(), migrate_selection(selection, !current));
        }
    }
    let plugin = if input.contains_key("pluginAgentModelSelectionOverrides") {
        parse_plugin_overrides(input.get("pluginAgentModelSelectionOverrides"))
    } else {
        let models = record(input.get("pluginAgentModelOverrides"));
        let levels = record(input.get("pluginAgentThoughtLevelOverrides"));
        let mut out = Map::new();
        for (id, model) in &models {
            let Some(selection) = parse_markdown_selection(Some(model), levels.get(id)) else {
                continue;
            };
            if !id.starts_with("plugin:") {
                continue;
            }
            out.insert(id.clone(), migrate_selection(selection, true));
        }
        out
    };
    let mut next = input.clone();
    next.insert("builtInModelSelectionOverrides".into(), Value::Object(selections));
    next.insert("pluginAgentModelSelectionOverrides".into(), Value::Object(plugin));
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_model_migration_matches_typescript() {
        let content = "---\nname: a\nmodel: builtin:zai/glm-4.6$high # keep\nthoughtLevel: high\n---\nbody\nmodel: builtin:zai/x\n";
        assert_eq!(
            migrate_markdown_provider(content),
            "---\nname: a\nmodel: zai-api/GLM-4.6$high # keep\nthoughtLevel: high\n---\nbody\nmodel: builtin:zai/x\n"
        );
        let quoted = "---\r\nmodel: \"custom:builtin:zai-coding-plan:glm-5.1\"\r\n---\r\n";
        assert_eq!(
            migrate_markdown_provider(quoted),
            "---\r\nmodel: \"custom:account%3Azai-individual-coding-plan:GLM-5.1\"\r\n---\r\n"
        );
        assert_eq!(migrate_markdown_provider("no frontmatter"), "no frontmatter");
        let single = "---\nmodel: 'builtin:bigmodel/glm-5'\n---";
        assert_eq!(migrate_markdown_provider(single), "---\nmodel: 'bigmodel-api/GLM-5'\n---");
    }

    #[test]
    fn selections_parse_and_format() {
        assert_eq!(
            parse_markdown_selection(Some(&json!("openai/gpt$high")), Some(&json!(" low "))),
            Some(json!({"providerId":"openai","modelId":"gpt","options":{"reasoningLevel":"low"}}))
        );
        assert_eq!(parse_markdown_selection(Some(&json!("inherit")), None), None);
        assert_eq!(
            format_markdown_model(&json!({"providerId":"a/b","modelId":"m$1"})),
            "custom:a%2Fb:m%241"
        );
        assert_eq!(parse_selection(Some(&json!({"providerId":" p ","modelId":"m","x":1}))), None);
        let state = json!({"builtInModelOverrides":{"Explore":"builtin:zai/glm-4.6"},"disabledAgentIds":[]});
        let imported = import_state_selections(state.as_object().unwrap());
        assert_eq!(
            Value::Object(imported),
            json!({"builtInModelOverrides":{"Explore":"builtin:zai/glm-4.6"},"disabledAgentIds":[],"builtInModelSelectionOverrides":{"Explore":{"providerId":"zai-api","modelId":"GLM-4.6"}},"pluginAgentModelSelectionOverrides":{}})
        );
    }
}
