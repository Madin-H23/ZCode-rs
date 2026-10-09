//! `getModelTrajectory`（modelTrajectory.ts、modelTrajectoryFileTail.ts）：从 CLI 落盘的
//! `model-io-<session>.jsonl` 还原一个 task 的模型调用轨迹。
//!
//! 只读文件、异步 IO；每个文件最多读末尾 32 MiB（从行中间开始时丢弃残行）。输出的键序与可选字段
//! 与 TS 对象一致（undefined 字段不出现）。
use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

use crate::support::collate::locale_compare;

const DEFAULT_LIMIT: usize = 200;
const MAX_READ_BYTES: u64 = 32 * 1024 * 1024;
const SESSION_TITLE_PROMPT_PREFIX: &str = "Generate a concise title for this coding session.";

/// `resolveModelIODirs`：`~/.zcode/cli` 与 `<dataBaseDir>/.zcode/cli` 下的 debug 与 rollout。
pub fn model_io_dirs(user_home: &Path, data_base_dir: &Path) -> Vec<PathBuf> {
    let mut roots = vec![user_home.join(".zcode").join("cli")];
    let data_root = data_base_dir.join(".zcode").join("cli");
    if !roots.contains(&data_root) {
        roots.push(data_root);
    }
    roots
        .into_iter()
        .flat_map(|root| [root.join("debug"), root.join("rollout")])
        .collect()
}

/// `sanitizeSessionSegment`：非 `[A-Za-z0-9_-]` 的连续字符换成 `-`，去首尾 `-`，截 80。
pub fn sanitize_session_segment(value: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for c in value.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            if pending_dash {
                out.push('-');
                pending_dash = false;
            }
            out.push(c);
        } else {
            pending_dash = true;
        }
    }
    if pending_dash {
        out.push('-');
    }
    let trimmed = out.trim_matches('-');
    trimmed.chars().take(80).collect()
}

async fn read_tail(path: &Path) -> std::io::Result<(String, bool)> {
    let mut file = tokio::fs::File::open(path).await?;
    let size = file.metadata().await?.len();
    let start = size.saturating_sub(MAX_READ_BYTES);
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let mut buffer = Vec::with_capacity((size - start) as usize);
    file.take(size - start).read_to_end(&mut buffer).await?;
    let mut text = String::from_utf8_lossy(&buffer).into_owned();
    if start > 0 {
        text = match text.find('\n') {
            Some(index) => text[index + 1..].to_owned(),
            None => String::new(),
        };
    }
    Ok((text, start > 0))
}

pub async fn read_model_trajectory(task_id: &str, limit: Option<f64>, dirs: &[PathBuf]) -> Value {
    let safe_limit = limit
        .filter(|limit| limit.is_finite() && *limit > 0.0)
        .map(|limit| limit.trunc() as usize)
        .unwrap_or(DEFAULT_LIMIT);
    let sanitized = sanitize_session_segment(task_id);
    let file_name = format!(
        "model-io-{}.jsonl",
        if sanitized.is_empty() {
            "no-session"
        } else {
            &sanitized
        }
    );
    let mut source_files = Vec::new();
    let mut raw: Vec<Value> = Vec::new();
    let mut input_truncated = false;
    for dir in dirs {
        let path = dir.join(&file_name);
        // 目录不存在或文件不可读都跳过（与 readdir + includes 的结果相同）。
        let Ok((text, truncated)) = read_tail(&path).await else {
            continue;
        };
        input_truncated |= truncated;
        let mut matched = false;
        for line in text.split('\n') {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(parsed) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            if parsed["type"] != "model_io" || parsed["sessionId"] != task_id {
                continue;
            }
            raw.push(parsed);
            matched = true;
        }
        if matched {
            source_files.push(path.to_string_lossy().into_owned());
        }
    }
    raw.sort_by(|left, right| {
        let time = |record: &Value| record["startedAt"].as_str().map(parse_time).unwrap_or(0.0);
        time(left)
            .partial_cmp(&time(right))
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                locale_compare(
                    left["requestId"].as_str().unwrap_or(""),
                    right["requestId"].as_str().unwrap_or(""),
                )
            })
    });
    let mut previous: Option<Value> = None;
    let mut records = Vec::with_capacity(raw.len());
    for record in raw {
        let expanded = expand_record(record, previous.as_ref());
        records.push(map_record(&expanded));
        previous = Some(expanded);
    }
    let truncated = input_truncated || records.len() > safe_limit;
    if truncated && records.len() > safe_limit {
        records.drain(..records.len() - safe_limit);
    }
    json!({
        "taskId": task_id,
        "available": true,
        "records": records,
        "sourceFiles": source_files,
        "truncated": truncated,
    })
}

/// `Date.parse` 对 `toISOString` 风格时间的子集：`YYYY-MM-DD[THH:MM[:SS[.fff]]][Z|±HH:MM]`；
/// 解析失败为 0（TS 的 NaN → 0）。
pub fn parse_time(text: &str) -> f64 {
    parse_iso(text).unwrap_or(0.0)
}

pub(crate) fn parse_iso(text: &str) -> Option<f64> {
    let digits = |s: &str| -> Option<i64> {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok())?
    };
    let (date, rest) = match text.find('T') {
        Some(index) => (&text[..index], Some(&text[index + 1..])),
        None => (text, None),
    };
    let mut parts = date.split('-');
    let year = digits(parts.next()?)?;
    let month = digits(parts.next()?)?;
    let day = digits(parts.next()?)?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut millis = 0.0;
    let mut offset_minutes = 0;
    if let Some(rest) = rest {
        let (clock, zone) = match rest.find(['Z', '+', '-']) {
            Some(index) => (&rest[..index], &rest[index..]),
            // 没有时区的日期时间按本地时间解析；Host 不追踪本地时区，按 UTC 近似。
            None => (rest, "Z"),
        };
        let (hms, fraction) = match clock.find('.') {
            Some(index) => (&clock[..index], Some(&clock[index + 1..])),
            None => (clock, None),
        };
        let mut fields = hms.split(':');
        let hour = digits(fields.next()?)?;
        let minute = digits(fields.next()?)?;
        let second = fields.next().map(digits).unwrap_or(Some(0))?;
        if hour > 24 || minute > 59 || second > 59 {
            return None;
        }
        if let Some(fraction) = fraction {
            let padded: String = fraction.chars().chain("000".chars()).take(3).collect();
            millis += digits(&padded)? as f64;
        }
        millis += ((hour * 60 + minute) * 60 + second) as f64 * 1000.0;
        if zone != "Z" {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let mut zone_parts = zone[1..].split(':');
            let zone_hours = digits(zone_parts.next()?)?;
            let zone_minutes = zone_parts.next().map(digits).unwrap_or(Some(0))?;
            offset_minutes = sign * (zone_hours * 60 + zone_minutes);
        }
    }
    let days = days_from_civil(year, month as u32, day as u32);
    Some(days as f64 * 86_400_000.0 + millis - offset_minutes as f64 * 60_000.0)
}

/// Howard Hinnant 的 civil 日期 → 自 1970-01-01 的天数。
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = month as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn as_object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

fn as_str(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str)
}

/// `asNumber`：有限数字（serde_json 只会给出有限数）。
fn as_number(value: Option<&Value>) -> Option<Value> {
    value.filter(|value| value.is_number()).cloned()
}

fn put(map: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        map.insert(key.into(), value);
    }
}

fn put_str(map: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    put(map, key, value.map(|value| json!(value)));
}

/// JS `a ?? b`：a 缺失或为 null 时取 b（b 缺失则字段不出现）。
fn nullish<'a>(a: Option<&'a Value>, b: Option<&'a Value>) -> Option<&'a Value> {
    match a {
        Some(value) if !value.is_null() => Some(value),
        _ => b,
    }
}

fn expand_record(record: Value, previous: Option<&Value>) -> Value {
    let Some(request) = record.get("request").and_then(Value::as_object).cloned() else {
        return record;
    };
    let previous_request = previous
        .and_then(|p| p.get("request"))
        .and_then(Value::as_object);
    let mut next = request;
    expand_collection(
        &mut next,
        previous_request,
        ("messages", "messagesKind", "messageOffset"),
        None,
    );
    expand_collection(
        &mut next,
        previous_request,
        ("sdkMessages", "sdkMessagesKind", "sdkMessageOffset"),
        None,
    );
    if let Some(body) = next.get("body").and_then(Value::as_object).cloned() {
        let mut next_body = body;
        let previous_body = previous_request
            .and_then(|p| p.get("body"))
            .and_then(Value::as_object);
        expand_collection(
            &mut next_body,
            previous_body,
            ("messages", "bodyMessagesKind", "bodyMessageOffset"),
            Some(&next),
        );
        next.insert("body".into(), Value::Object(next_body));
    }
    let mut record = record;
    if let Some(object) = record.as_object_mut() {
        object.insert("request".into(), Value::Object(next));
    }
    record
}

/// delta 记录只存增量消息：按 offset 截取上一条的完整消息再拼接；tail 是新的起点，不再拼接。
fn expand_collection(
    target: &mut Map<String, Value>,
    previous: Option<&Map<String, Value>>,
    (collection, kind_key, offset_key): (&str, &str, &str),
    metadata: Option<&Map<String, Value>>,
) {
    let source = metadata.unwrap_or(target);
    if source.get(kind_key).and_then(Value::as_str) != Some("delta") {
        return;
    }
    let offset = source
        .get(offset_key)
        .and_then(Value::as_f64)
        .filter(|offset| offset.fract() == 0.0 && *offset >= 0.0)
        .map(|offset| offset as usize);
    let (Some(Value::Array(delta)), Some(Value::Array(previous)), Some(offset)) = (
        target.get(collection),
        previous.and_then(|p| p.get(collection)),
        offset,
    ) else {
        return;
    };
    let mut merged: Vec<Value> = previous.iter().take(offset).cloned().collect();
    merged.extend(delta.iter().cloned());
    target.insert(collection.into(), Value::Array(merged));
}

fn map_record(record: &Value) -> Value {
    let request = as_object(record.get("request"));
    let response = as_object(record.get("response"));
    let model = as_object(record.get("model"));
    let error = as_object(record.get("error"));
    let query_source = as_str(record.get("querySource"))
        .map(str::to_owned)
        .or_else(|| infer_query_source(request));
    let model_role = model.and_then(|m| as_str(m.get("role")));
    let mut mapped = Map::new();
    mapped.insert(
        "requestId".into(),
        json!(as_str(record.get("requestId")).unwrap_or("")),
    );
    mapped.insert(
        "attempt".into(),
        as_number(record.get("attempt")).unwrap_or(json!(1)),
    );
    mapped.insert(
        "startedAt".into(),
        json!(as_str(record.get("startedAt")).unwrap_or("")),
    );
    put_str(
        &mut mapped,
        "completedAt",
        as_str(record.get("completedAt")),
    );
    put(
        &mut mapped,
        "durationMs",
        as_number(record.get("durationMs")),
    );
    put_str(&mut mapped, "turnId", as_str(record.get("turnId")));
    put_str(&mut mapped, "traceId", as_str(record.get("traceId")));
    mapped.insert(
        "callSource".into(),
        call_source(query_source.as_deref(), model_role),
    );
    let mut model_out = Map::new();
    for key in ["modelId", "providerId", "role", "source"] {
        put_str(&mut model_out, key, model.and_then(|m| as_str(m.get(key))));
    }
    mapped.insert("model".into(), Value::Object(model_out));
    mapped.insert(
        "request".into(),
        json!({
            "messages": map_messages(request.and_then(|r| r.get("messages"))),
            "toolNames": request
                .and_then(|r| r.get("toolNames"))
                .and_then(Value::as_array)
                .map(|names| names.iter().filter(|n| n.is_string()).cloned().collect::<Vec<_>>())
                .unwrap_or_default(),
        }),
    );
    if let Some(response) = response {
        let mut out = Map::new();
        for key in ["finishReason", "text", "reasoningText"] {
            put_str(&mut out, key, as_str(response.get(key)));
        }
        let tool_calls: Vec<Value> = response
            .get("toolCalls")
            .and_then(Value::as_array)
            .map(|calls| calls.iter().map(map_response_tool_call).collect())
            .unwrap_or_default();
        out.insert("toolCalls".into(), Value::Array(tool_calls));
        put(&mut out, "usage", map_usage(response.get("usage")));
        put_str(&mut out, "responseId", as_str(response.get("responseId")));
        put_str(&mut out, "modelId", as_str(response.get("modelId")));
        mapped.insert("response".into(), Value::Object(out));
    }
    if let Some(error) = error
        && (as_str(error.get("message")).is_some_and(|m| !m.is_empty())
            || as_str(error.get("name")).is_some_and(|n| !n.is_empty()))
    {
        let mut out = Map::new();
        out.insert(
            "name".into(),
            json!(as_str(error.get("name")).unwrap_or("Error")),
        );
        out.insert(
            "message".into(),
            json!(as_str(error.get("message")).unwrap_or("")),
        );
        put_str(&mut out, "stack", as_str(error.get("stack")));
        mapped.insert("error".into(), Value::Object(out));
    }
    Value::Object(mapped)
}

fn infer_query_source(request: Option<&Map<String, Value>>) -> Option<String> {
    let first = request?.get("messages")?.as_array()?.first()?.as_object()?;
    if first.get("role").and_then(Value::as_str) != Some("system") {
        return None;
    }
    as_str(first.get("content"))
        .filter(|content| content.starts_with(SESSION_TITLE_PROMPT_PREFIX))
        .map(|_| "session_title".to_owned())
}

fn call_source(query_source: Option<&str>, model_role: Option<&str>) -> Value {
    let with_source = |kind: &str, source: Option<&str>| match source {
        Some(source) => json!({"kind": kind, "querySource": source}),
        None => json!({"kind": kind}),
    };
    match (query_source, model_role) {
        (Some("main_turn"), _) => with_source("main", query_source),
        (Some("subagent"), _) => with_source("subagent", query_source),
        (Some("compact"), _) | (_, Some("compact")) => with_source("compact", query_source),
        (Some(source), _) if !source.is_empty() => with_source("sidecar", query_source),
        (_, Some("subagent")) => json!({"kind": "subagent"}),
        _ => json!({"kind": "main"}),
    }
}

fn map_messages(value: Option<&Value>) -> Vec<Value> {
    let Some(messages) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    messages
        .iter()
        .map(|entry| {
            let empty = Map::new();
            let message = entry.as_object().unwrap_or(&empty);
            let role = as_str(message.get("role")).unwrap_or("unknown");
            let tool_call_id = as_str(message.get("toolCallId"))
                .or_else(|| as_str(message.get("tool_call_id")))
                .or_else(|| as_str(message.get("tool_use_id")));
            let tool_name = as_str(message.get("toolName")).or_else(|| as_str(message.get("name")));
            let is_error = message.get("isError") == Some(&json!(true))
                || message.get("is_error") == Some(&json!(true));
            json!({
                "role": role,
                "parts": map_content(message.get("content"), role, (tool_call_id, tool_name, is_error)),
            })
        })
        .collect()
}

type MessageTool<'a> = (Option<&'a str>, Option<&'a str>, bool);

fn map_content(content: Option<&Value>, role: &str, tool: MessageTool<'_>) -> Vec<Value> {
    match content {
        Some(Value::String(text)) => {
            if text.is_empty() {
                return Vec::new();
            }
            if role == "tool" {
                let mut part = Map::new();
                part.insert("kind".into(), json!("tool-result"));
                put_str(&mut part, "toolCallId", tool.0);
                put_str(&mut part, "toolName", tool.1);
                let output = if tool.2 {
                    json!({"type": "error-text", "value": text})
                } else {
                    serde_json::from_str(text).unwrap_or_else(|_| json!(text))
                };
                part.insert("output".into(), output);
                return vec![Value::Object(part)];
            }
            vec![json!({"kind": "text", "text": text})]
        }
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| map_part(part, (role == "tool").then_some(tool)))
            .collect(),
        _ => Vec::new(),
    }
}

fn map_part(raw: &Value, tool: Option<MessageTool<'_>>) -> Value {
    let Some(part) = raw.as_object() else {
        return json!({"kind": "unknown", "raw": raw});
    };
    let mut out = Map::new();
    match part.get("type").and_then(Value::as_str) {
        Some(kind @ ("text" | "reasoning")) => {
            out.insert("kind".into(), json!(kind));
            out.insert("text".into(), json!(as_str(part.get("text")).unwrap_or("")));
        }
        Some("tool-call") => {
            out.insert("kind".into(), json!("tool-call"));
            put_str(&mut out, "toolCallId", as_str(part.get("toolCallId")));
            out.insert(
                "toolName".into(),
                json!(as_str(part.get("toolName")).unwrap_or("tool")),
            );
            put(
                &mut out,
                "input",
                nullish(part.get("input"), part.get("args")).cloned(),
            );
        }
        Some("tool-result") => {
            out.insert("kind".into(), json!("tool-result"));
            put_str(
                &mut out,
                "toolCallId",
                as_str(part.get("toolCallId")).or(tool.and_then(|t| t.0)),
            );
            put_str(
                &mut out,
                "toolName",
                as_str(part.get("toolName")).or(tool.and_then(|t| t.1)),
            );
            put(
                &mut out,
                "output",
                nullish(part.get("output"), part.get("result")).cloned(),
            );
        }
        Some("image" | "file") => {
            out.insert("kind".into(), json!("image"));
            put_str(&mut out, "mediaType", as_str(part.get("mediaType")));
        }
        _ => return json!({"kind": "unknown", "raw": raw}),
    }
    Value::Object(out)
}

fn map_response_tool_call(raw: &Value) -> Value {
    let Some(call) = raw.as_object() else {
        return json!({"kind": "unknown", "raw": raw});
    };
    let mut out = Map::new();
    out.insert("kind".into(), json!("tool-call"));
    put_str(
        &mut out,
        "toolCallId",
        as_str(call.get("id")).or_else(|| as_str(call.get("toolCallId"))),
    );
    out.insert(
        "toolName".into(),
        json!(
            as_str(call.get("name"))
                .or_else(|| as_str(call.get("toolName")))
                .unwrap_or("tool")
        ),
    );
    put(
        &mut out,
        "input",
        nullish(call.get("input"), call.get("args")).cloned(),
    );
    Value::Object(out)
}

fn map_usage(value: Option<&Value>) -> Option<Value> {
    let usage = value?.as_object()?;
    let mut out = Map::new();
    for key in [
        "inputTokens",
        "outputTokens",
        "totalTokens",
        "cacheReadTokens",
        "reasoningTokens",
    ] {
        put(&mut out, key, as_number(usage.get(key)));
    }
    Some(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_and_time_follow_typescript() {
        assert_eq!(sanitize_session_segment("--a b/c--"), "a-b-c");
        assert_eq!(sanitize_session_segment("a-!b"), "a--b");
        assert_eq!(parse_time("2026-01-02T03:04:05.678Z"), 1_767_323_045_678.0);
        assert_eq!(parse_time("2026-01-02T05:04:05+02:00"), 1_767_323_045_000.0);
        assert_eq!(parse_time("garbage"), 0.0);
    }

    #[tokio::test]
    async fn trajectory_expands_deltas_and_maps_records() {
        let dir = tempfile::tempdir().unwrap();
        let debug = dir.path().join("debug");
        std::fs::create_dir_all(&debug).unwrap();
        let lines = [
            json!({"type": "model_io", "sessionId": "s/1", "requestId": "r2", "startedAt": "2026-01-01T00:00:02Z",
                "request": {"messagesKind": "delta", "messageOffset": 1, "messages": [{"role": "tool", "content": "{\"ok\":1}", "tool_call_id": "c1"}]},
                "response": {"toolCalls": [{"id": "c2", "name": "Read", "input": {"p": 1}}], "usage": {"inputTokens": 3}}}),
            json!({"type": "model_io", "sessionId": "s/1", "requestId": "r1", "startedAt": "2026-01-01T00:00:01Z", "querySource": "compact",
                "request": {"messages": [{"role": "user", "content": "hi"}, {"role": "assistant", "content": [{"type": "text", "text": "yo"}]}]},
                "error": {"message": "boom"}}),
            json!({"type": "model_io", "sessionId": "other"}),
        ];
        let text: Vec<String> = lines.iter().map(Value::to_string).collect();
        std::fs::write(debug.join("model-io-s-1.jsonl"), text.join("\n")).unwrap();
        let trajectory =
            read_model_trajectory("s/1", None, &[debug.clone(), dir.path().join("none")]).await;
        assert_eq!(trajectory["truncated"], false);
        let records = trajectory["records"].as_array().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["requestId"], "r1");
        assert_eq!(
            records[0]["callSource"],
            json!({"kind": "compact", "querySource": "compact"})
        );
        assert_eq!(
            records[0]["error"],
            json!({"name": "Error", "message": "boom"})
        );
        // delta：前一条的第一条消息 + 本条增量。
        let messages = records[1]["request"]["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[1]["parts"][0],
            json!({"kind": "tool-result", "toolCallId": "c1", "output": {"ok": 1}})
        );
        assert_eq!(
            records[1]["response"]["toolCalls"][0],
            json!({"kind": "tool-call", "toolCallId": "c2", "toolName": "Read", "input": {"p": 1}})
        );
        let limited = read_model_trajectory("s/1", Some(1.0), &[debug]).await;
        assert_eq!(limited["truncated"], true);
        assert_eq!(limited["records"][0]["requestId"], "r2");
    }
}
