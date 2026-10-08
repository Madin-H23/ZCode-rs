//! `/status` 的已工作时长与进展（statusFormatting.ts）。会话快照取自 CLI `session/read`
//! （legacy 会话快照：`messages[].info.time`、`messages[].parts`）。
use serde_json::Value;

use super::util::{collapse_whitespace, truncate_ellipsis};

const SECOND: i64 = 1_000;
const MINUTE: i64 = 60 * SECOND;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

/// `taskStatus`。
pub fn task_status(task: &Value) -> String {
    task["status"].as_str().unwrap_or("running").to_owned()
}

/// `formatTaskRunningDuration`。
pub fn format_duration(duration_ms: i64) -> String {
    let duration = duration_ms.max(0);
    let parts = [
        (duration / DAY, "d"),
        ((duration % DAY) / HOUR, "h"),
        ((duration % HOUR) / MINUTE, "m"),
    ];
    let mut out: Vec<String> = parts
        .iter()
        .filter(|(value, _)| *value > 0)
        .map(|(value, unit)| format!("{value}{unit}"))
        .collect();
    let seconds = (duration % MINUTE) / SECOND;
    if seconds > 0 || out.is_empty() {
        out.push(format!("{seconds}s"));
    }
    out.join(" ")
}

fn messages(snapshot: &Value) -> &[Value] {
    snapshot["messages"].as_array().map(Vec::as_slice).unwrap_or_default()
}

fn role(message: &Value) -> &str {
    message["info"]["role"].as_str().unwrap_or_default()
}

fn created(message: &Value) -> Option<i64> {
    message["info"]["time"]["created"].as_i64()
}

/// `readTaskWorkedDurationMs`：运行中取最后一条助手（或用户）消息到现在；结束后取最后一条助手消息耗时。
pub fn worked_duration(snapshot: &Value, task: &Value, now: i64) -> Option<i64> {
    let list = messages(snapshot);
    if task_status(task) == "running" {
        let started = list
            .iter()
            .rev()
            .find(|message| role(message) == "assistant")
            .and_then(created)
            .or_else(|| {
                list.iter()
                    .rev()
                    .find(|message| role(message) == "user")
                    .and_then(created)
            })
            .or_else(|| task["createdAt"].as_i64())
            .or_else(|| task["updatedAt"].as_i64())?;
        return Some((now - started).max(0));
    }
    let completed = list
        .iter()
        .rev()
        .find(|message| role(message) == "assistant")
        .and_then(|message| {
            let time = &message["info"]["time"];
            Some(time["completed"].as_i64()? - time["created"].as_i64()?)
        });
    if completed.is_some() {
        return completed;
    }
    match (task["updatedAt"].as_i64(), task["createdAt"].as_i64()) {
        (Some(updated), Some(created)) if updated >= created => Some(updated - created),
        _ => None,
    }
}

fn read_field(value: &Value, keys: &[&str]) -> Option<String> {
    if let Some(text) = value.as_str() {
        let text = collapse_whitespace(text);
        return (!text.is_empty()).then_some(text);
    }
    if !value.is_object() {
        return None;
    }
    keys.iter()
        .filter_map(|key| value[*key].as_str())
        .map(collapse_whitespace)
        .find(|text| !text.is_empty())
}

const DETAIL_KEYS: [&str; 5] = ["command", "path", "file_path", "filePath", "prompt"];

/// `formatStatusToolProgress`（legacy tool part）。
fn tool_progress(part: &Value) -> Option<String> {
    let state = &part["state"];
    let title = collapse_whitespace(
        state["title"]
            .as_str()
            .or_else(|| part["tool"].as_str())
            .unwrap_or("tool"),
    );
    let detail = read_field(&state["input"], &DETAIL_KEYS)
        .or_else(|| read_field(&state["output"], &DETAIL_KEYS));
    let status = state["status"]
        .as_str()
        .map(|status| format!(" [{status}]"))
        .unwrap_or_default();
    Some(match detail {
        Some(detail) => format!("{title}{status}: {detail}"),
        None => format!("{title}{status}"),
    })
}

/// `readLatestTaskProgress`：只看助手消息里最近的正文、思考或工具。
pub fn latest_progress(snapshot: &Value) -> Option<String> {
    for message in messages(snapshot).iter().rev() {
        if role(message) != "assistant" {
            continue;
        }
        let parts = message["parts"].as_array().map(Vec::as_slice).unwrap_or_default();
        for part in parts.iter().rev() {
            match part["type"].as_str() {
                Some("text" | "reasoning") => {
                    let text = part["text"].as_str().unwrap_or_default();
                    if !text.trim().is_empty() {
                        return Some(truncate_ellipsis(&collapse_whitespace(text), 180));
                    }
                }
                Some("tool") => {
                    if let Some(progress) = tool_progress(part) {
                        return Some(truncate_ellipsis(&progress, 180));
                    }
                }
                _ => {}
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn durations_and_progress_match_the_ts_helpers() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(61_000), "1m 1s");
        assert_eq!(format_duration(3_600_000), "1h");
        assert_eq!(format_duration(DAY + 2 * HOUR + 5_000), "1d 2h 5s");
        let snapshot = json!({"messages": [
            {"info": {"role": "user", "time": {"created": 100}}, "parts": [{"type": "text", "text": "hi"}]},
            {"info": {"role": "assistant", "time": {"created": 200, "completed": 1_200}}, "parts": [
                {"type": "tool", "tool": "Bash", "state": {"status": "completed", "input": {"command": "ls"}}},
                {"type": "text", "text": "  done  now "}]},
        ]});
        assert_eq!(latest_progress(&snapshot).as_deref(), Some("done now"));
        assert_eq!(worked_duration(&snapshot, &json!({"status": "completed"}), 0), Some(1_000));
        assert_eq!(worked_duration(&snapshot, &json!({}), 5_200), Some(5_000));
        let tool_only = json!({"messages": [{"info": {"role": "assistant", "time": {"created": 1}},
            "parts": [{"type": "tool", "tool": "Bash", "state": {"status": "running", "input": {"command": "make"}}}]}]});
        assert_eq!(latest_progress(&tool_only).as_deref(), Some("Bash [running]: make"));
    }
}
