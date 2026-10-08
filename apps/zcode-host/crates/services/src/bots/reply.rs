//! 第三方回复的文本格式（replyFormatter.ts、shared tool-call-summary.ts）：工具摘要行、工具卡、
//! 长回复切分。各通道共用，避免文案分叉。
use regex::Regex;
use serde_json::Value;

use super::stream::ToolState;
use super::util::{collapse_whitespace, js_len, js_slice, truncate_ellipsis};

const MAX_FIELD_LENGTH: usize = 160;
const MAX_COMMAND_FIELD_LENGTH: usize = 96;
pub const MAX_REPLY_MESSAGE_LENGTH: usize = 3500;
const DIFF_ADDED_MARKER: &str = "🟢";
const DIFF_REMOVED_MARKER: &str = "🔴";

fn english(locale: Option<&str>) -> bool {
    locale == Some("en-US")
}

/// `formatterMessages`。
fn t(locale: Option<&str>, key: &str) -> &'static str {
    let en = english(locale);
    match key {
        "toolCalls" => if en { "Tool calls:" } else { "工具调用：" },
        "completed" => if en { "Completed" } else { "完成" },
        "failed" => if en { "Failed" } else { "失败" },
        "denied" => if en { "Denied" } else { "已拒绝" },
        "inProgress" => if en { "Running" } else { "⏳ 运行中" },
        "permissionRequired" => if en { "Permission required:" } else { "需要权限：" },
        _ => if en { "Pending" } else { "等待中" },
    }
}

/// `truncateMiddleText`（UTF-16 计长）。
fn truncate_middle(value: &str, max: usize) -> String {
    let len = js_len(value);
    if len <= max {
        return value.to_owned();
    }
    let head = ((max - 5) as f64 * 0.65).ceil() as usize;
    let tail = (max - 5).saturating_sub(head);
    format!("{} ... {}", js_slice(value, 0, head), js_slice(value, len - tail, len))
}

/// `formatMarkdownInlineCode`。
pub fn inline_code(value: &str) -> String {
    format!("`{}`", value.replace('\\', "\\\\").replace('`', "\\`"))
}

fn count_lines(value: &str) -> usize {
    if value.is_empty() {
        return 0;
    }
    let mut count = 1 + value.matches('\n').count();
    if value.ends_with('\n') {
        count -= 1;
    }
    count
}

fn first_string<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| value.get(*key).and_then(Value::as_str))
}

/// `extractBeforeAfterText`。
fn before_after(source: &Value) -> Option<(String, String)> {
    if !source.is_object() {
        return None;
    }
    const BEFORE: [&str; 4] = ["old_string", "oldString", "oldText", "before"];
    const AFTER: [&str; 5] = ["new_string", "newString", "newText", "after", "content"];
    let pair = |value: &Value| match (first_string(value, &BEFORE), first_string(value, &AFTER)) {
        (Some(before), Some(after)) => Some((before.to_owned(), after.to_owned())),
        _ => None,
    };
    if let Some(found) = pair(source) {
        return Some(found);
    }
    let diff = &source["metadata"]["filediff"];
    if diff.is_object()
        && let Some(found) = pair(diff)
    {
        return Some(found);
    }
    source["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block.is_object())
        .find_map(pair)
}

/// `getChangeStat`。
fn change_stat(kind: &str, input: &Value, output: &Value) -> Option<(usize, usize)> {
    static EDIT_KIND: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)(edit|patch|replace|multi.?edit)").expect("valid regex")
    });
    if !EDIT_KIND.is_match(kind) {
        return None;
    }
    let (before, after) = before_after(input).or_else(|| before_after(output))?;
    let removed = count_lines(&before);
    let added = count_lines(&after);
    (added != 0 || removed != 0).then_some((added, removed))
}

/// `getInputSummary`。
fn input_summary(input: &Value) -> Option<String> {
    if let Some(text) = input.as_str() {
        let summary = collapse_whitespace(text);
        return (!summary.is_empty()).then_some(summary);
    }
    if !input.is_object() {
        return None;
    }
    ["command", "path", "file_path", "filePath", "prompt"]
        .iter()
        .filter_map(|key| input[*key].as_str())
        .map(collapse_whitespace)
        .find(|summary| !summary.is_empty())
}

/// `formatBotDiffCount`。
pub fn diff_count(added: usize, removed: usize) -> String {
    let mut parts = Vec::new();
    if added > 0 {
        parts.push(format!("{DIFF_ADDED_MARKER} {}", inline_code(&format!("+{added}"))));
    }
    if removed > 0 {
        parts.push(format!("{DIFF_REMOVED_MARKER} {}", inline_code(&format!("-{removed}"))));
    }
    parts.join(" ")
}

fn strip_trailing_slash(value: &str) -> &str {
    if value.len() > 1 {
        value.trim_end_matches('/')
    } else {
        value
    }
}

/// `toWorkspaceRelativePath`。
fn workspace_relative(path: &str, workspace: &str) -> String {
    let normalized_path = path.trim().replace('\\', "/");
    let normalized_path = strip_trailing_slash(&normalized_path).to_owned();
    if workspace.trim().is_empty() {
        return normalized_path;
    }
    let normalized_workspace = workspace.trim().replace('\\', "/");
    let normalized_workspace = strip_trailing_slash(&normalized_workspace);
    let comparable_path = normalized_path.to_lowercase();
    let comparable_workspace = normalized_workspace.to_lowercase();
    if comparable_path == comparable_workspace {
        return ".".into();
    }
    if comparable_path.starts_with(&format!("{comparable_workspace}/")) {
        return js_slice(&normalized_path, js_len(normalized_workspace) + 1, usize::MAX);
    }
    normalized_path
}

/// `normalizeToolCallSummaryInput`：只有 kind 恰为 `edit` 时把路径改成相对 workspace。
fn summary_input(tool: &ToolState, workspace: &str) -> Value {
    if !tool.input.is_object() || tool.tool_name != "edit" {
        return tool.input.clone();
    }
    let mut input = tool.input.clone();
    for key in ["path", "file_path", "filePath"] {
        if let Some(path) = input[key].as_str().map(str::to_owned) {
            input[key] = Value::String(workspace_relative(&path, workspace));
        }
    }
    input
}

fn status_label(tool: &ToolState, locale: Option<&str>) -> String {
    match tool.status {
        "completed" => t(locale, "completed").to_owned(),
        "failed" => {
            let detail = tool
                .error
                .as_deref()
                .filter(|error| !error.is_empty())
                .map(|error| format!(": {}", truncate_ellipsis(&collapse_whitespace(error), MAX_FIELD_LENGTH)))
                .unwrap_or_default();
            format!("{}{detail}", t(locale, "failed"))
        }
        "denied" => t(locale, "denied").to_owned(),
        "in_progress" => t(locale, "inProgress").to_owned(),
        _ => t(locale, "pending").to_owned(),
    }
}

/// `formatBotToolCallSummaryLine`：`- <状态> · <工具名>[ · \`<命令或路径>\`][ · 🟢 \`+n\` 🔴 \`-m\`]`。
pub fn tool_summary_line(tool: &ToolState, workspace: &str, locale: Option<&str>) -> String {
    let input = summary_input(tool, workspace);
    let kind = if tool.tool_name.is_empty() { "tool" } else { tool.tool_name.as_str() };
    let primary = {
        let title = collapse_whitespace(&tool.tool_name);
        if title.is_empty() { "tool".to_owned() } else { title }
    };
    let mut detail = Vec::new();
    if let Some(secondary) = input_summary(&input) {
        detail.push(inline_code(&truncate_middle(&collapse_whitespace(&secondary), MAX_COMMAND_FIELD_LENGTH)));
    }
    if let Some((added, removed)) = change_stat(kind, &input, &tool.output) {
        let stat = diff_count(added, removed);
        if !stat.is_empty() {
            detail.push(stat);
        }
    }
    let detail = if detail.is_empty() {
        String::new()
    } else {
        format!(" · {}", detail.join(" · "))
    };
    format!("- {} · {primary}{detail}", status_label(tool, locale))
}

/// `formatBotToolCallReply`。
pub fn tool_reply(tool: &ToolState, workspace: &str, locale: Option<&str>) -> String {
    format!("{}\n{}", t(locale, "toolCalls"), tool_summary_line(tool, workspace, locale))
}

/// `isBotToolCallReplyTerminal`。
pub fn is_tool_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "denied" | "stopped")
}

fn last_index_of(units: &[u16], needle: u16, from: usize) -> Option<usize> {
    let end = from.min(units.len().saturating_sub(1));
    (0..=end).rev().find(|index| units[*index] == needle)
}

/// `splitLongReplyText`：超过 3500 码元时优先在换行或空格处切开。
pub fn split_long_reply(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut remaining: Vec<u16> = text.trim().encode_utf16().collect();
    while remaining.len() > MAX_REPLY_MESSAGE_LENGTH {
        let breakpoint = last_index_of(&remaining, u16::from(b'\n'), MAX_REPLY_MESSAGE_LENGTH)
            .into_iter()
            .chain(last_index_of(&remaining, u16::from(b' '), MAX_REPLY_MESSAGE_LENGTH))
            .max()
            .unwrap_or(0);
        let end = if breakpoint > 0 { breakpoint } else { MAX_REPLY_MESSAGE_LENGTH };
        chunks.push(String::from_utf16_lossy(&remaining[..end]).trim().to_owned());
        remaining = String::from_utf16_lossy(&remaining[end..])
            .trim()
            .encode_utf16()
            .collect();
    }
    let rest = String::from_utf16_lossy(&remaining);
    if !rest.is_empty() {
        chunks.push(rest);
    }
    chunks
}

/// `extractBotAssistantResponseMessages`：非 force 时整段留在缓冲区；force 时整段切分发出。
pub fn extract_messages(buffer: &str, force: bool) -> (Vec<String>, String) {
    let normalized = buffer.replace("\r\n", "\n");
    if !force {
        return (Vec::new(), normalized);
    }
    (split_long_reply(&normalized), String::new())
}

/// 权限请求的摘要（`formatBotPermissionRequestSummary` 的 V4 版本：用交互自带的 summary 与 detail）。
pub fn permission_summary(payload: &Value, workspace: &str, locale: Option<&str>) -> String {
    let title = payload["summary"]
        .as_str()
        .filter(|summary| !summary.trim().is_empty())
        .or_else(|| payload["toolName"].as_str())
        .unwrap_or("tool");
    let header = format!("{}\n{title}", t(locale, "permissionRequired"));
    let detail = &payload["detail"];
    if let Some(command) = first_string(detail, &["command"]) {
        return format!("{header}\n{}", inline_code(&truncate_middle(command, MAX_COMMAND_FIELD_LENGTH)));
    }
    let paths: Vec<String> = ["path", "file_path", "filePath"]
        .iter()
        .filter_map(|key| detail[*key].as_str())
        .take(3)
        .map(|path| inline_code(&workspace_relative(path, workspace)))
        .collect();
    if !paths.is_empty() {
        return format!("{header}\n{}", truncate_ellipsis(&paths.join(", "), MAX_FIELD_LENGTH));
    }
    header
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str, status: &'static str, input: Value) -> ToolState {
        ToolState {
            tool_id: "t".into(),
            tool_name: name.into(),
            input,
            output: Value::Null,
            status,
            error: None,
        }
    }

    #[test]
    fn tool_lines_match_the_ts_formatter() {
        assert_eq!(
            tool_summary_line(&tool("Bash", "completed", json!({"command": "ls  -la"})), "/w", None),
            "- 完成 · Bash · `ls -la`"
        );
        assert_eq!(
            tool_summary_line(
                &tool("Edit", "in_progress", json!({"file_path": "/w/a.rs", "old_string": "a\nb", "new_string": "c"})),
                "/w",
                Some("en-US")
            ),
            "- Running · Edit · `/w/a.rs` · 🟢 `+1` 🔴 `-2`"
        );
        let mut failed = tool("Read", "failed", json!({}));
        failed.error = Some("boom\n now".into());
        assert_eq!(tool_summary_line(&failed, "/w", None), "- 失败: boom now · Read");
        assert_eq!(tool_summary_line(&tool("", "stopped", Value::Null), "/w", None), "- 等待中 · tool");
        assert_eq!(
            tool_reply(&tool("Bash", "completed", json!({"command": "pwd"})), "/w", None),
            "工具调用：\n- 完成 · Bash · `pwd`"
        );
        let long = "x".repeat(200);
        let line = tool_summary_line(&tool("Bash", "completed", json!({"command": long})), "/w", None);
        assert!(line.contains(" ... "), "{line}");
        assert_eq!(inline_code("a`b\\c"), "`a\\`b\\\\c`");
    }

    #[test]
    fn long_replies_split_on_breaks() {
        let text = format!("{}\n{}", "a".repeat(3000), "b".repeat(1000));
        let chunks = split_long_reply(&text);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 3000);
        assert_eq!(chunks[1].len(), 1000);
        assert_eq!(split_long_reply("  hi  "), vec!["hi"]);
        assert!(split_long_reply("   ").is_empty());
        assert_eq!(extract_messages("a\r\nb", false), (Vec::new(), "a\nb".to_owned()));
        assert_eq!(extract_messages("a\r\nb", true), (vec!["a\nb".to_owned()], String::new()));
    }
}
