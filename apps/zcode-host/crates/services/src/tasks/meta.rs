//! 会话快照 / sessions-index 摘要 → `ZCodeTaskMeta`（zcodeTaskIndexSyncer.ts 的 buildMetaFromSnapshot、
//! buildBaselineMetaFromSummary、hasUserVisibleContent、buildSearchableTextFromSnapshot，以及
//! packages/shared 的 deriveZCodeTaskStatusFromSessionSnapshot、resolveZCodeVisibleSessionTitle）。
//!
//! 消息可见性按 conversation-message-projection-policy.ts 的规则近似：只用于标题、搜索正文与
//! “是否已有用户内容”的判断，不影响会话本身的渲染（那由 CLI 的 V4 帧决定）。
use serde_json::{Map, Value, json};

use crate::agent::WorkspaceTarget;
use crate::agent::service::uuid_v4;

pub const PROVIDER: &str = "glm";
const SEARCH_TEXT_MAX_CHARS: usize = 200_000;
const TITLE_MAX_CHARS: usize = 80;

const PROVIDER_CONTEXT_SOURCES: &[&str] = &[
    "agent_control_message",
    "background_task",
    "goal-continuation",
    "goal_completion_verification",
    "goal_state_change",
    "plugin_reference",
    "queued_system_notification",
    "resume_goal_state",
    "resume_referenced_session_context",
    "rewind",
    "selection_side_chat",
    "subagent",
    "subagent_message",
    "target_continuation",
    "task_notification",
    "task_status",
    "todo_reminder",
];
const HIDDEN_TEXT_PREFIXES: &[&str] = &[
    "<system-reminder",
    "<task-notification>",
    "<subagent-notification>",
];

pub fn is_terminal_phase(phase: &Value) -> bool {
    matches!(
        phase.as_str(),
        Some("completedSuccess" | "completedInterrupted" | "error")
    )
}

pub fn status_from_phase(phase: &Value) -> Option<&'static str> {
    match phase.as_str() {
        Some("running" | "prewarming") => Some("running"),
        Some("completedSuccess" | "completedInterrupted") => Some("completed"),
        Some("error") => Some("error"),
        _ => None,
    }
}

/// resolveTerminalUnreadSignal：失败，或完成且没有未验证的目标时，后台任务需要提醒未读。
pub fn terminal_unread_signal(summary: &Value) -> Option<&'static str> {
    match summary["phase"].as_str() {
        Some("error") => Some("background_terminal"),
        Some("completedSuccess" | "completedInterrupted")
            if summary["goalStatus"].is_null() || summary["goalStatus"] == "verified" =>
        {
            Some("background_terminal")
        }
        _ => None,
    }
}

fn insert_target_fields(meta: &mut Map<String, Value>, target: &WorkspaceTarget) {
    meta.insert("workspacePath".into(), json!(target.workspace_path));
    if let Some(identity) = target
        .workspace_identity
        .as_deref()
        .map(str::trim)
        .filter(|identity| !identity.is_empty())
    {
        meta.insert("workspaceIdentity".into(), json!(identity));
    }
}

/// buildBaselineMetaFromSummary：首份快照里已有会话的静默基线行。
pub fn baseline_meta(target: &WorkspaceTarget, summary: &Value) -> Value {
    let mut meta = Map::new();
    meta.insert("taskId".into(), summary["sessionId"].clone());
    meta.insert("traceId".into(), json!(uuid_v4()));
    meta.insert("title".into(), summary["title"].clone());
    if summary["titleSource"] == "custom" {
        meta.insert("titleOverridden".into(), json!(true));
    }
    insert_target_fields(&mut meta, target);
    meta.insert("createdAt".into(), summary["createdAt"].clone());
    meta.insert("updatedAt".into(), summary["lastActivityAt"].clone());
    meta.insert("mode".into(), json!("build"));
    meta.insert("provider".into(), json!(PROVIDER));
    if let Some(parent) = summary["parentSessionId"].as_str() {
        meta.insert("forkedFromTaskId".into(), json!(parent));
    }
    if let Some(status) = status_from_phase(&summary["phase"]) {
        meta.insert("status".into(), json!(status));
    }
    Value::Object(meta)
}

fn texts(message: &Value) -> impl Iterator<Item = &str> {
    message["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| part["type"] == "text")
        .filter_map(|part| part["text"].as_str())
}

/// getConversationMessageProjectionPolicy 的近似：只给模型看的消息（压缩摘要、运行时注入、
/// 子 agent 消息、system-reminder 等）不是用户可见内容。
fn is_model_only(message: &Value) -> bool {
    let info = &message["info"];
    let semantics = &info["semantics"];
    if semantics["kind"] == "compact_summary" || !info["summary"].is_null() {
        return true;
    }
    if semantics.is_object() {
        if semantics["origin"] == "real_user"
            && info["synthetic"] != true
            && info["visibility"] != "model-only"
        {
            return false;
        }
        if info["role"] == "assistant"
            && semantics["kind"] == "assistant_response"
            && semantics["uiVisibility"] == "visible"
            && semantics["transcriptVisibility"] == "visible"
        {
            return false;
        }
        if semantics["providerVisibility"] == "visible" {
            return true;
        }
        if semantics["origin"] == "agent_runtime"
            || semantics["uiVisibility"] == "hidden"
            || semantics["transcriptVisibility"] == "hidden"
        {
            return true;
        }
    }
    if info["visibility"] == "model-only" {
        return true;
    }
    let source = info["source"]
        .as_str()
        .or_else(|| info["metadata"]["source"].as_str());
    if source.is_some_and(|source| PROVIDER_CONTEXT_SOURCES.contains(&source)) {
        return true;
    }
    info["role"] == "user"
        && texts(message).any(|text| {
            let text = text.trim_start();
            HIDDEN_TEXT_PREFIXES
                .iter()
                .any(|prefix| text.starts_with(prefix))
        })
}

fn is_compact_summary(message: &Value) -> bool {
    message["info"]["role"] == "user"
        && message["parts"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|part| {
                part["type"] == "compaction" && !part["metadata"]["timelineStatus"].is_string()
            })
}

fn visible_messages(snapshot: &Value) -> Vec<&Value> {
    snapshot["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| !is_model_only(message) && !is_compact_summary(message))
        .collect()
}

fn truncate_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn is_reminder_title(title: &str) -> bool {
    title.trim_start().starts_with("<system-reminder")
}

/// resolveZCodeVisibleSessionTitle。
pub fn derive_title(snapshot: &Value) -> String {
    let title = snapshot["session"]["title"].as_str().unwrap_or("").trim();
    if !title.is_empty() && !is_reminder_title(title) {
        return title.to_owned();
    }
    let first_user = visible_messages(snapshot)
        .into_iter()
        .find(|message| message["info"]["role"] == "user")
        .map(|message| texts(message).collect::<String>())
        .unwrap_or_default();
    if !first_user.trim().is_empty() {
        return truncate_chars(first_user.trim(), TITLE_MAX_CHARS);
    }
    let objective = snapshot["projection"]["target"]["objective"]
        .as_str()
        .unwrap_or("")
        .trim();
    if !objective.is_empty() {
        return truncate_chars(objective, TITLE_MAX_CHARS);
    }
    "New session".to_owned()
}

/// hasUserVisibleContent：刚建出来、还没有任何用户内容的会话不广播，避免侧栏闪 "New session"。
pub fn has_user_visible_content(snapshot: &Value) -> bool {
    let title = snapshot["session"]["title"].as_str().unwrap_or("").trim();
    if !title.is_empty() && !is_reminder_title(title) {
        return true;
    }
    if snapshot["projection"]["target"]["objective"]
        .as_str()
        .is_some_and(|objective| !objective.trim().is_empty())
    {
        return true;
    }
    snapshot["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["info"]["role"] == "user" && !is_model_only(message))
        .any(|message| texts(message).any(|text| !text.trim().is_empty()))
}

/// buildSearchableTextFromSnapshot：用户消息的全部正文 + 助手消息的最后一段正文，上限 20 万字符。
pub fn searchable_text(snapshot: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut total = 0;
    'messages: for message in visible_messages(snapshot) {
        let all: Vec<&str> = texts(message).collect();
        let chosen: &[&str] = if message["info"]["role"] == "assistant" {
            &all[all.len().saturating_sub(1)..]
        } else {
            &all
        };
        for text in chosen {
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            parts.push(text.to_owned());
            total += text.chars().count();
            if total >= SEARCH_TEXT_MAX_CHARS {
                break 'messages;
            }
        }
    }
    truncate_chars(&parts.join("\n"), SEARCH_TEXT_MAX_CHARS)
}

fn blocking_runtime(snapshot: &Value) -> bool {
    let runtime = &snapshot["runtime"];
    if !runtime["activeTurnId"].is_null() || !runtime["activeTurnKind"].is_null() {
        return true;
    }
    let projection = &snapshot["projection"];
    if projection["pendingPermissions"]
        .as_array()
        .is_some_and(|pending| !pending.is_empty())
    {
        return true;
    }
    projection["activeToolCalls"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|call| matches!(call["status"].as_str(), Some("pending" | "running")))
}

fn completed_visible_assistant_turn(snapshot: &Value) -> bool {
    let Some(last) = visible_messages(snapshot).pop() else {
        return false;
    };
    let info = &last["info"];
    let finish = info["finish"]
        .as_str()
        .map(|finish| finish.trim().to_lowercase().replace('_', "-"));
    info["role"] == "assistant"
        && finish.as_deref() != Some("tool-calls")
        && info["time"]["completed"].is_number()
}

/// deriveZCodeTaskStatusFromSessionSnapshot。
pub fn derive_status(snapshot: &Value) -> Option<&'static str> {
    if !snapshot["projection"]["lastError"].is_null() {
        return Some("error");
    }
    let status = match snapshot["session"]["status"].as_str() {
        Some("running" | "waiting" | "paused") => Some("running"),
        Some("error") => Some("error"),
        Some("completed") => Some("completed"),
        _ => None,
    };
    if matches!(status, Some("error" | "completed")) {
        return status;
    }
    let blocking = blocking_runtime(snapshot);
    if completed_visible_assistant_turn(snapshot) && !blocking {
        return Some("completed");
    }
    if blocking {
        return status.or(Some("running"));
    }
    status
}

/// formatModelPickerValue：`provider/model`，有推理档位时追加 `$level`。
fn format_model(selection: &Value) -> String {
    let (Some(provider), Some(model)) = (
        selection["providerId"].as_str(),
        selection["modelId"].as_str(),
    ) else {
        return String::new();
    };
    match selection["options"]["reasoningLevel"].as_str() {
        Some(level) => format!("{provider}/{model}${level}"),
        None => format!("{provider}/{model}"),
    }
}

fn copy_if_present(from: &Value, to: &mut Map<String, Value>, from_key: &str, to_key: &str) {
    if let Some(value) = from.get(from_key).filter(|value| !value.is_null()) {
        to.insert(to_key.to_owned(), value.clone());
    }
}

/// fromZCodeGoal。
fn goal(target: &Value) -> Value {
    let mut out = Map::new();
    for (from, to) in [
        ("sessionId", "sessionID"),
        ("targetId", "targetID"),
        ("objective", "objective"),
        ("summaryTitle", "summaryTitle"),
        ("status", "status"),
        ("tokenBudget", "tokenBudget"),
        ("tokensUsed", "tokensUsed"),
        ("timeUsedSeconds", "timeUsedSeconds"),
    ] {
        copy_if_present(target, &mut out, from, to);
    }
    let mut time = Map::new();
    copy_if_present(target, &mut time, "createdAt", "created");
    copy_if_present(target, &mut time, "updatedAt", "updated");
    out.insert("time".into(), Value::Object(time));
    Value::Object(out)
}

/// buildMetaFromSnapshot：`session/read` 快照 → 任务行。
pub fn meta_from_snapshot(snapshot: &Value) -> Value {
    let session = &snapshot["session"];
    let mut meta = Map::new();
    meta.insert("taskId".into(), session["sessionId"].clone());
    meta.insert(
        "traceId".into(),
        session["traceId"]
            .as_str()
            .map(|trace| json!(trace))
            .unwrap_or_else(|| json!(uuid_v4())),
    );
    meta.insert("title".into(), json!(derive_title(snapshot)));
    copy_if_present(
        &session["workspace"],
        &mut meta,
        "workspacePath",
        "workspacePath",
    );
    copy_if_present(
        &session["workspace"],
        &mut meta,
        "workspaceIdentity",
        "workspaceIdentity",
    );
    copy_if_present(session, &mut meta, "createdAt", "createdAt");
    copy_if_present(session, &mut meta, "updatedAt", "updatedAt");
    copy_if_present(session, &mut meta, "mode", "mode");
    // 最近一条消息的模型优先，其次是当前设置（resolveTaskMetaModelSelectionFromSnapshot）。
    let latest_model = snapshot["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .rev()
        .map(|message| &message["info"]["model"])
        .find(|model| model.is_object());
    let model = format_model(latest_model.unwrap_or(&snapshot["settings"]["model"]["current"]));
    meta.insert("model".into(), json!(model));
    copy_if_present(
        &snapshot["settings"]["thoughtLevel"],
        &mut meta,
        "current",
        "thoughtLevel",
    );
    meta.insert("provider".into(), json!(PROVIDER));
    if let Some(status) = derive_status(snapshot) {
        meta.insert("status".into(), json!(status));
    }
    let last_error = &snapshot["projection"]["lastError"];
    if last_error.is_object() {
        let mut error = Map::new();
        let code = if last_error["code"].is_null() {
            last_error["type"].clone()
        } else {
            last_error["code"].clone()
        };
        error.insert("code".into(), code);
        copy_if_present(last_error, &mut error, "detail", "detail");
        copy_if_present(last_error, &mut error, "attribution", "attribution");
        error.insert("message".into(), last_error["message"].clone());
        meta.insert("lastError".into(), Value::Object(error));
    }
    if let Some(target) = snapshot["projection"].get("target") {
        // 缺席表示快照没带目标信息（不覆盖旧值）；null 表示明确没有目标。
        let value = if target.is_object() {
            goal(target)
        } else {
            Value::Null
        };
        meta.insert("target".into(), value);
    }
    Value::Object(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Value {
        json!({
            "session": {
                "sessionId": "s1", "workspace": {"workspacePath": "/w", "workspaceKey": "/w"},
                "title": "", "mode": "build", "status": "idle", "createdAt": 1, "updatedAt": 2,
            },
            "settings": {"model": {"current": {"providerId": "p", "modelId": "m"}}, "thoughtLevel": {"current": "enabled"}},
            "projection": {"pendingPermissions": [], "activeToolCalls": []},
            "runtime": {},
            "messages": [
                {"info": {"role": "user", "source": "todo_reminder"}, "parts": [{"type": "text", "text": "hidden"}]},
                {"info": {"role": "user"}, "parts": [{"type": "text", "text": "  fix the build  "}]},
                {"info": {"role": "assistant", "time": {"completed": 5}, "model": {"providerId": "p", "modelId": "m2", "options": {"reasoningLevel": "high"}}},
                 "parts": [{"type": "text", "text": "draft"}, {"type": "text", "text": "done"}]},
            ],
        })
    }

    #[test]
    fn snapshot_meta_follows_typescript_rules() {
        let meta = meta_from_snapshot(&snapshot());
        assert_eq!(meta["title"], "fix the build");
        assert_eq!(meta["model"], "p/m2$high");
        assert_eq!(meta["thoughtLevel"], "enabled");
        assert_eq!(meta["status"], "completed");
        assert_eq!(meta["provider"], "glm");
        assert!(meta.get("workspaceIdentity").is_none());
        assert!(meta.get("target").is_none());
        assert_eq!(searchable_text(&snapshot()), "fix the build\ndone");
        assert!(has_user_visible_content(&snapshot()));

        let mut empty = snapshot();
        empty["messages"] = json!([]);
        assert!(!has_user_visible_content(&empty));
        assert_eq!(derive_title(&empty), "New session");
        assert_eq!(derive_status(&empty), None);

        let mut running = snapshot();
        running["projection"]["pendingPermissions"] = json!([{"id": "x"}]);
        assert_eq!(derive_status(&running), Some("running"));
        running["projection"]["lastError"] = json!({"type": "provider", "message": "boom"});
        let meta = meta_from_snapshot(&running);
        assert_eq!(meta["status"], "error");
        assert_eq!(
            meta["lastError"],
            json!({"code": "provider", "message": "boom"})
        );
    }

    #[test]
    fn baseline_and_unread_rules() {
        let target = WorkspaceTarget::local("/w");
        let summary = json!({"sessionId": "s", "title": "T", "titleSource": "custom", "phase": "completedSuccess",
                             "createdAt": 1, "lastActivityAt": 3, "parentSessionId": "p"});
        let meta = baseline_meta(&target, &summary);
        assert_eq!(meta["titleOverridden"], true);
        assert_eq!(meta["status"], "completed");
        assert_eq!(meta["forkedFromTaskId"], "p");
        assert_eq!(meta["updatedAt"], 3);
        assert_eq!(
            terminal_unread_signal(&summary),
            Some("background_terminal")
        );
        let unverified = json!({"phase": "completedSuccess", "goalStatus": "active"});
        assert_eq!(terminal_unread_signal(&unverified), None);
        assert!(is_terminal_phase(&json!("error")));
        assert!(!is_terminal_phase(&json!("running")));
    }
}
