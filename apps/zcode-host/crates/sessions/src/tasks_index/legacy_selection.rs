//! 迁移 0002 的冻结转换（packages/services/src/session/tasksDatabase/provider-selection-v2.ts）。
//!
//! 只在库级迁移事务里运行一次：把旧 automation 的 model/provider/thought_level 三列解码成
//! `model_selection` JSON；旧三列与时间戳原样保留。解码规则冻结在发布前的裁决，不能改用运行时
//! parser。
use rusqlite::Connection;
use serde_json::{Map, Value};
use zcode_kit::js::{json as js_json, string as js_string};

/// 旧 builtin provider 到 provider id 的冻结映射。
const PROVIDER_NAMES: [(&str, &str); 6] = [
    ("builtin:bigmodel", "bigmodel-api"),
    ("builtin:zai", "zai-api"),
    ("builtin:bigmodel-start-plan", "account:bigmodel-start-plan"),
    ("builtin:zai-start-plan", "account:zai-start-plan"),
    (
        "builtin:bigmodel-coding-plan",
        "account:bigmodel-individual-coding-plan",
    ),
    (
        "builtin:zai-coding-plan",
        "account:zai-individual-coding-plan",
    ),
];

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// `%XX` 在 `at` 处解出的字节。
fn escaped_byte(bytes: &[u8], at: usize) -> Option<u8> {
    if bytes.get(at) != Some(&b'%') {
        return None;
    }
    Some(hex(*bytes.get(at + 1)?)? << 4 | hex(*bytes.get(at + 2)?)?)
}

/// JS `decodeURIComponent`；任何 URIError（坏转义、非法 UTF-8）返回 `None`。
pub(crate) fn decode_uri_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'%' {
            // '%' 不会出现在多字节 UTF-8 序列中间，原样拷贝即可。
            out.push(bytes[at]);
            at += 1;
            continue;
        }
        let lead = escaped_byte(bytes, at)?;
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
            let next = escaped_byte(bytes, at)?;
            if next & 0xc0 != 0x80 {
                return None;
            }
            sequence.push(next);
            at += 3;
        }
        // 过长编码、代理区与超出 U+10FFFF 都是 URIError，与 from_utf8 的拒绝范围一致。
        std::str::from_utf8(&sequence).ok()?;
        out.extend_from_slice(&sequence);
    }
    String::from_utf8(out).ok()
}

/// `decodeComponent`：解码失败时保留原值。
fn decode_component(value: &str) -> String {
    decode_uri_component(value).unwrap_or_else(|| value.to_owned())
}

/// `decodeLegacySelection`：返回 ModelSelection JSON；无法确定身份时返回 `None`。
pub(crate) fn decode_legacy_selection(
    model: Option<&str>,
    provider: Option<&str>,
    thought_level: Option<&str>,
) -> Option<Value> {
    let value = js_string::trim(model?);
    if value.is_empty() {
        return None;
    }
    let mut provider = provider.map(js_string::trim).unwrap_or("").to_owned();
    let mut model = value.to_owned();
    let mut reasoning_level = thought_level.map(|level| js_string::trim(level).to_owned());
    if let Some(body) = value.strip_prefix("custom:") {
        let separator = body.find(':')?;
        let parts: Vec<&str> = body.split(':').collect();
        if parts.len() >= 3 && parts[0] == "builtin" {
            provider = format!("builtin:{}", parts[1]);
            model = decode_component(&parts[2..].join(":"));
        } else {
            provider = decode_component(&body[..separator]);
            model = decode_component(&body[separator + 1..]);
        }
    } else if let Some(separator) = value.find('/') {
        provider = value[..separator].to_owned();
        model = value[separator + 1..].to_owned();
        if let Some(level_separator) = model.find('$')
            && level_separator > 0
            && level_separator < model.len() - 1
        {
            let level = js_string::trim(&model[level_separator + 1..]);
            if level.is_empty() {
                return None;
            }
            reasoning_level = Some(level.to_owned());
            model.truncate(level_separator);
        }
    } else if provider == "glm" || provider == "zcode" {
        // 旧 provider=glm/zcode 是执行后端，不是供应商身份。
        return None;
    }
    let provider = js_string::trim(&provider);
    let model = js_string::trim(&model);
    if provider.is_empty() || model.is_empty() {
        return None;
    }
    let provider_id = if provider.starts_with("builtin:") {
        PROVIDER_NAMES
            .iter()
            .find(|(name, _)| *name == provider)
            .map(|(_, id)| *id)?
    } else {
        provider
    };
    let mut selection = Map::new();
    selection.insert("providerId".into(), Value::String(provider_id.to_owned()));
    selection.insert("modelId".into(), Value::String(model.to_owned()));
    if let Some(level) = reasoning_level.filter(|level| !level.is_empty()) {
        let mut options = Map::new();
        options.insert("reasoningLevel".into(), Value::String(level));
        selection.insert("options".into(), Value::Object(options));
    }
    Some(Value::Object(selection))
}

/// `importLegacyAutomationSelections`：必须在库级 IMMEDIATE 迁移事务里调用。
pub(crate) fn import_legacy_automation_selections(conn: &Connection) -> rusqlite::Result<()> {
    let mut statement = conn.prepare(
        "SELECT automation_id, model, provider, thought_level FROM automations WHERE model IS NOT NULL",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (automation_id, model, provider, thought_level) in rows {
        let Some(decoded) = decode_legacy_selection(
            model.as_deref(),
            provider.as_deref(),
            thought_level.as_deref(),
        ) else {
            // 有旧显式意图但无法确定身份：不留未发布中间态的“默认”，避免静默换模型。
            if model
                .as_deref()
                .is_some_and(|m| !js_string::trim(m).is_empty())
            {
                conn.execute(
                    "UPDATE automations SET model_selection=NULL WHERE automation_id=?",
                    [&automation_id],
                )?;
            }
            continue;
        };
        conn.execute(
            "UPDATE automations SET model_selection = ?
       WHERE automation_id = ?
         AND model IS ? AND provider IS ? AND thought_level IS ?",
            (
                js_json::stringify(&decoded),
                &automation_id,
                &model,
                &provider,
                &thought_level,
            ),
        )?;
    }
    conn.execute_batch(
        "UPDATE automations SET model_selection='null'
    WHERE model_selection IS NULL AND (model IS NULL OR trim(model)='')",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_uri_component_matches_js() {
        assert_eq!(decode_uri_component("a%20b").as_deref(), Some("a b"));
        assert_eq!(decode_uri_component("%E4%B8%AD").as_deref(), Some("中"));
        assert_eq!(decode_uri_component("%2F%3A").as_deref(), Some("/:"));
        assert_eq!(decode_uri_component("模型").as_deref(), Some("模型"));
        for bad in [
            "%",
            "%2",
            "%ZZ",
            "%E0%A4%A",
            "%C0%AF",
            "%ED%A0%80",
            "%F8%80",
            "%80",
        ] {
            assert_eq!(decode_uri_component(bad), None, "{bad}");
        }
    }

    #[test]
    fn decodes_legacy_selections() {
        let decode = |m, p, t| decode_legacy_selection(Some(m), p, t).map(|v| v.to_string());
        assert_eq!(
            decode(
                "custom:builtin:zai-coding-plan:glm-5.1",
                None,
                Some(" high ")
            )
            .as_deref(),
            Some(
                r#"{"providerId":"account:zai-individual-coding-plan","modelId":"glm-5.1","options":{"reasoningLevel":"high"}}"#
            )
        );
        assert_eq!(
            decode("openai/gpt-4o$high", None, None).as_deref(),
            Some(
                r#"{"providerId":"openai","modelId":"gpt-4o","options":{"reasoningLevel":"high"}}"#
            )
        );
        // 先整体 trim，末尾的 `$` 不再算推理级别分隔符。
        assert_eq!(
            decode("openai/gpt$  ", None, None).as_deref(),
            Some(r#"{"providerId":"openai","modelId":"gpt$"}"#)
        );
        assert_eq!(decode("glm-4.6", Some("glm"), None), None);
        assert_eq!(decode("custom:builtin:unknown:x", None, None), None);
    }
}
