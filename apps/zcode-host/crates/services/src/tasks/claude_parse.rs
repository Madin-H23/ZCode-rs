//! Claude Code 原生会话的解析（claude-native/claudeNativeSessionHeadParser.ts、
//! claudeNativeSessionImportParser.ts）：从 `~/.claude/projects/**/<session>.jsonl` 读出可见的
//! user / assistant 文本、workspace（cwd）、时间与模型。
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};
use zcode_cli_domain::js_string;

use super::trajectory::parse_iso;

static IDE_OPENED_FILE_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<ide_opened_file>.*?</ide_opened_file>").expect("regex"));
static COMMAND_TAG_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<(?:local-command|command)-[^>]+>.*?</(?:local-command|command)-[^>]+>")
        .expect("regex")
});
static NO_RESPONSE_PLACEHOLDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:\n+\s*)?No response requested\.$").expect("regex"));

pub type Record = Map<String, Value>;

fn object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

/// `readTrimmedString`：非空的 trim 后字符串。
fn trimmed(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(js_string::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// `deriveSessionTitle(content, [])`：前 50 个 UTF-16 单元，超出时加 `...`。
pub fn derive_session_title(content: &str) -> String {
    let prefix = js_string::utf16_prefix(content, 50);
    if prefix.len() < content.len() {
        format!("{prefix}...")
    } else {
        content.to_owned()
    }
}

fn number_timestamp(value: f64) -> Option<i64> {
    if !value.is_finite() {
        return None;
    }
    if value > 1_000_000_000_000.0 {
        Some(value.trunc() as i64)
    } else if value > 1_000_000_000.0 {
        Some((value * 1000.0).trunc() as i64)
    } else {
        None
    }
}

/// `toTimestampMs`：毫秒 / 秒级数字、数字字符串或可解析的日期字符串。
fn to_timestamp_ms(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number_timestamp(number.as_f64()?),
        Value::String(text) => {
            let normalized = js_string::trim(text);
            if normalized.is_empty() {
                return None;
            }
            if let Ok(number) = normalized.parse::<f64>() {
                return number_timestamp(number);
            }
            parse_iso(normalized).map(|ms| ms as i64)
        }
        _ => None,
    }
}

pub fn entry_timestamp(entry: &Record) -> Option<i64> {
    let message = object(entry.get("message"));
    let request = object(entry.get("request"));
    let candidates = [
        entry.get("timestamp"),
        entry.get("createdAt"),
        entry.get("updatedAt"),
        entry.get("created_at"),
        entry.get("updated_at"),
        entry.get("time"),
        message.and_then(|m| m.get("timestamp")),
        message.and_then(|m| m.get("createdAt")),
        message.and_then(|m| m.get("updatedAt")),
        request.and_then(|r| r.get("timestamp")),
    ];
    candidates.into_iter().find_map(to_timestamp_ms)
}

fn entry_workspace_path(entry: &Record) -> Option<String> {
    trimmed(entry.get("cwd"))
        .or_else(|| trimmed(object(entry.get("message")).and_then(|m| m.get("cwd"))))
        .or_else(|| trimmed(object(entry.get("request")).and_then(|r| r.get("cwd"))))
}

fn entry_model(entry: &Record) -> Option<String> {
    trimmed(entry.get("model"))
        .or_else(|| trimmed(object(entry.get("message")).and_then(|m| m.get("model"))))
}

fn flag(record: Option<&Map<String, Value>>, key: &str) -> bool {
    record.is_some_and(|r| r.get(key) == Some(&Value::Bool(true)))
}

pub fn has_sidechain_marker(entries: &[Record]) -> bool {
    entries.iter().any(|entry| {
        flag(Some(entry), "isSidechain")
            || flag(object(entry.get("message")), "isSidechain")
            || flag(object(entry.get("request")), "isSidechain")
    })
}

fn sanitize(text: &str) -> String {
    let text = IDE_OPENED_FILE_TAG.replace_all(text, " ");
    let text = COMMAND_TAG_BLOCK.replace_all(&text, " ");
    js_string::trim(&text.replace("\r\n", "\n")).to_owned()
}

fn text_field(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Object(map) => trimmed(map.get("text"))
            .or_else(|| trimmed(map.get("content")))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn user_text(entry: &Record) -> Option<String> {
    if entry.get("type").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let message = object(entry.get("message"));
    if flag(Some(entry), "isMeta") || flag(message, "isMeta") {
        return None;
    }
    // `message?.content ?? request.prompt`
    let content = message
        .and_then(|m| m.get("content"))
        .filter(|c| !c.is_null())
        .or_else(|| object(entry.get("request")).and_then(|r| r.get("prompt")));
    match content? {
        Value::String(text) => Some(sanitize(text)).filter(|t| !t.is_empty()),
        Value::Array(items) => {
            let parts: Vec<String> = items
                .iter()
                .filter_map(|item| match item {
                    Value::String(text) => Some(sanitize(text)),
                    Value::Object(map)
                        if map.get("type").and_then(Value::as_str) != Some("tool_result") =>
                    {
                        Some(sanitize(&text_field(item)))
                    }
                    _ => None,
                })
                .filter(|part| !part.is_empty())
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n\n"))
        }
        _ => None,
    }
}

/// 两个解析器对 assistant 的取舍不同：扫描（head）排除 API 错误与 `<synthetic>`；
/// 导入只排除非错误的 `<synthetic>` 占位，并去掉结尾的 “No response requested.”。
fn assistant_text(entry: &Record, for_import: bool) -> Option<String> {
    if entry.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let message = object(entry.get("message"));
    let synthetic = entry_model(entry).as_deref() == Some("<synthetic>");
    let api_error = flag(Some(entry), "isApiErrorMessage") || flag(message, "isApiErrorMessage");
    let hidden = if for_import {
        synthetic && !api_error
    } else {
        synthetic || api_error
    };
    if hidden {
        return None;
    }
    let text = match message.and_then(|m| m.get("content"))? {
        Value::String(text) => Some(sanitize(text)).filter(|t| !t.is_empty())?,
        Value::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(text) => Some(sanitize(text)),
                Value::Object(map) if map.get("type").and_then(Value::as_str) == Some("text") => {
                    Some(sanitize(&text_field(item)))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .concat(),
        _ => return None,
    };
    if text.is_empty() {
        return None;
    }
    if !for_import {
        return Some(text);
    }
    let stripped = NO_RESPONSE_PLACEHOLDER
        .replace(text.trim_end(), "")
        .trim_end()
        .to_owned();
    (!stripped.is_empty()).then_some(stripped)
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct HeadInfo {
    pub workspace_path: Option<String>,
    pub preview_title: Option<String>,
    pub created_at: Option<i64>,
}

/// `extractClaudeNativeSessionHeadInfo`（扫描与查找用 head 解析器的规则）。
pub fn head_info(entries: &[Record]) -> HeadInfo {
    let mut workspace_path = None;
    for entry in entries {
        let user = user_text(entry);
        let assistant = assistant_text(entry, false);
        if workspace_path.is_none() && (user.is_some() || assistant.is_some()) {
            workspace_path = entry_workspace_path(entry);
        }
        let Some(user) = user else {
            continue;
        };
        return HeadInfo {
            workspace_path,
            preview_title: Some(derive_session_title(&user)),
            created_at: entry_timestamp(entry),
        };
    }
    HeadInfo {
        workspace_path,
        ..HeadInfo::default()
    }
}

/// 导入用的会话源（`ClaudeNativeImportedSessionSource`）。
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedSource {
    pub session_id: String,
    pub workspace_path: String,
    pub source_path: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub title: String,
    pub model: Option<String>,
    /// `{role, content, timestamp, model?, turnIndex}`
    pub messages: Vec<Value>,
}

/// `parseClaudeNativeSessionRecords`：assistant 片段按用户轮次合并。
pub fn parse_records(
    records: &[Record],
    workspace_path: &str,
    session_id: &str,
    source_path: &str,
    fallback_created_at: Option<i64>,
    fallback_updated_at: Option<i64>,
    now: i64,
) -> Result<ImportedSource, String> {
    let mut messages: Vec<Value> = Vec::new();
    let mut detected: Option<String> = None;
    let mut first_user: Option<i64> = None;
    let mut updated_at = fallback_updated_at;
    let mut turn: i64 = -1;
    let mut model: Option<String> = None;
    let mut pending = String::new();
    let mut pending_at: Option<i64> = None;
    let flush = |messages: &mut Vec<Value>,
                 pending: &mut String,
                 pending_at: &mut Option<i64>,
                 updated_at: &mut Option<i64>,
                 turn: i64,
                 model: &Option<String>| {
        if pending.is_empty() || turn < 0 {
            pending.clear();
            *pending_at = None;
            return;
        }
        let mut message = Map::new();
        message.insert("role".into(), json!("assistant"));
        message.insert("content".into(), json!(pending.clone()));
        message.insert(
            "timestamp".into(),
            json!(pending_at.or(*updated_at).unwrap_or(now)),
        );
        if let Some(model) = model {
            message.insert("model".into(), json!(model));
        }
        message.insert("turnIndex".into(), json!(turn));
        messages.push(Value::Object(message));
        *updated_at = pending_at.or(*updated_at);
        pending.clear();
        *pending_at = None;
    };
    for entry in records {
        let user = user_text(entry);
        if detected.is_none() && user.is_some() {
            detected = entry_workspace_path(entry);
        }
        if let Some(user) = user {
            flush(
                &mut messages,
                &mut pending,
                &mut pending_at,
                &mut updated_at,
                turn,
                &model,
            );
            turn += 1;
            let timestamp = entry_timestamp(entry).or(updated_at).unwrap_or(now);
            first_user.get_or_insert(timestamp);
            updated_at = Some(timestamp);
            messages.push(
                json!({"role": "user", "content": user, "timestamp": timestamp, "turnIndex": turn}),
            );
            continue;
        }
        let assistant = assistant_text(entry, true);
        if detected.is_none() && assistant.is_some() {
            detected = entry_workspace_path(entry);
        }
        let Some(assistant) = assistant.filter(|_| turn >= 0) else {
            continue;
        };
        if model.is_none() {
            model = entry_model(entry);
        }
        pending.push_str(&assistant);
        pending_at = entry_timestamp(entry).or(pending_at).or(updated_at);
        updated_at = pending_at.or(updated_at);
    }
    flush(
        &mut messages,
        &mut pending,
        &mut pending_at,
        &mut updated_at,
        turn,
        &model,
    );
    if messages.is_empty() {
        return Err(format!(
            "[claude-native] Claude 原生 session {session_id} 没有可导入的可见消息"
        ));
    }
    let created_at = first_user
        .or(fallback_created_at)
        .or(updated_at)
        .unwrap_or(now);
    let first_user_text = messages
        .iter()
        .find(|m| m["role"] == "user")
        .and_then(|m| m["content"].as_str())
        .unwrap_or("");
    Ok(ImportedSource {
        session_id: session_id.to_owned(),
        workspace_path: detected.unwrap_or_else(|| workspace_path.to_owned()),
        source_path: source_path.to_owned(),
        created_at,
        updated_at: updated_at
            .or(first_user)
            .or(fallback_created_at)
            .unwrap_or(now),
        title: derive_session_title(first_user_text),
        model,
        messages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(values: Vec<Value>) -> Vec<Record> {
        values
            .into_iter()
            .map(|v| v.as_object().cloned().unwrap())
            .collect()
    }

    #[test]
    fn head_and_import_follow_typescript_rules() {
        let entries = records(vec![
            json!({"type": "user", "isMeta": true, "message": {"content": "meta"}, "cwd": "/meta"}),
            json!({"type": "assistant", "cwd": "/w", "message": {"model": "<synthetic>", "content": [{"type": "text", "text": "x"}]}}),
            json!({"type": "user", "cwd": "/w", "timestamp": "2026-01-01T00:00:00Z", "message": {"content": [
                {"type": "tool_result", "content": "ignored"},
                {"type": "text", "text": "<ide_opened_file>a.rs</ide_opened_file> fix the bug"},
                "second part",
            ]}}),
            json!({"type": "assistant", "timestamp": 1_767_225_601, "message": {"model": "claude-x", "content": [{"type": "text", "text": "On it"}]}}),
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": ".\n\nNo response requested."}]}}),
        ]);
        let head = head_info(&entries);
        assert_eq!(head.workspace_path.as_deref(), Some("/w"));
        assert_eq!(
            head.preview_title.as_deref(),
            Some("fix the bug\n\nsecond part")
        );
        assert_eq!(head.created_at, Some(1_767_225_600_000));
        let source =
            parse_records(&entries, "/fallback", "s1", "/p/s1.jsonl", None, Some(5), 9).unwrap();
        assert_eq!(source.workspace_path, "/w");
        assert_eq!(source.model.as_deref(), Some("claude-x"));
        assert_eq!(source.messages.len(), 2);
        assert_eq!(source.messages[1]["content"], "On it.");
        assert_eq!(source.messages[1]["timestamp"], 1_767_225_601_000i64);
        assert_eq!(source.updated_at, 1_767_225_601_000);
        assert_eq!(
            derive_session_title(&"a".repeat(60)),
            format!("{}...", "a".repeat(50))
        );
        assert!(has_sidechain_marker(&records(vec![
            json!({"message": {"isSidechain": true}})
        ])));
        assert!(parse_records(&[], "/w", "empty", "/p", None, None, 1).is_err());
    }
}
