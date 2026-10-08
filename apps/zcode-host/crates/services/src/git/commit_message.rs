//! `git.generateCommitMessage`（gitService.ts、commitMessageFileScope.ts、
//! gitCommitMessageGenerator.ts）。prompt、裁剪上限与校验规则逐字移植；模型调用经
//! [`TextGenerator`] 回到 Agent（CLI `workspace/generateText`）。
use std::collections::HashSet;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use zcode_host_kit::{RpcError, log};

use super::parse::{normalize_git_path, normalize_workspace_in_repo_path};
use super::repo::GitRepo;
use super::{FileChange, changes_for_source};
use crate::support::path;

const MAX_PROMPT_FILES: usize = 20;
const MAX_DIFF_FILES: usize = 8;
const MAX_DIFF_CHARS: usize = 12_000;
const MAX_DIFF_CHARS_PER_FILE: usize = 2_000;
const MAX_CONVERSATION_CONTEXT_MESSAGES: usize = 12;
const MAX_CONVERSATION_CONTEXT_CHARS: usize = 4_000;
const MAX_CONVERSATION_CONTEXT_CHARS_PER_MESSAGE: usize = 600;
const MAX_COMMIT_MESSAGE_CHARS: usize = 1_000;
const COMMIT_MESSAGE_DIFF_FILE_LIMIT: usize = 8;
pub const COMMIT_MESSAGE_QUERY_SOURCE: &str = "git_commit_message";

/// 读取当前模型、调用模型生成文本；由 composition root 用 Agent 与 provider 运行时实现。
pub trait TextGenerator: Send + Sync {
    /// `model-selection` 视图的 `preferredSelection`（Node `readCurrentModel`）。
    fn current_model(&self) -> BoxFuture<'static, Option<Value>>;
    /// CLI `workspace/generateText` 的参数与结果（`ZCodeWorkspaceGenerateText*`）。
    fn generate_text(&self, params: Value) -> BoxFuture<'static, Result<Value, RpcError>>;
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateRequest {
    pub workspace_path: String,
    pub workspace_identity: Option<String>,
    pub locale: Option<String>,
    pub include_unstaged: Option<bool>,
    pub current_session_file_paths: Option<Vec<String>>,
    pub conversation_context: Option<ConversationContext>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationContext {
    #[serde(default)]
    pub messages: Vec<ConversationMessage>,
    pub omitted_message_count: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct ConversationMessage {
    pub role: String,
    pub content: String,
}

fn error(message: &str) -> RpcError {
    RpcError::new("Error", message)
}

// ── commitMessageFileScope.ts ──

fn normalize_scope_path(value: &str) -> String {
    let normalized = normalize_git_path(value.trim());
    let stripped = normalized
        .strip_prefix("./")
        .or_else(|| normalized.strip_prefix('/'))
        .unwrap_or(&normalized);
    stripped.trim_end_matches('/').to_owned()
}

fn relative_inside(relative: &str) -> bool {
    let normalized = normalize_git_path(relative);
    !normalized.is_empty()
        && normalized != ".."
        && !normalized.starts_with("../")
        && !path::is_absolute(&normalized)
}

fn add_candidate(scope: &mut HashSet<String>, value: &str) {
    let normalized = normalize_scope_path(value);
    if !normalized.is_empty() {
        scope.insert(normalized);
    }
}

fn file_scope(
    workspace_path: &str,
    repo_root: &str,
    workspace_in_repo_path: &str,
    session_paths: Option<&[String]>,
) -> Option<HashSet<String>> {
    let sources: Vec<&str> = session_paths?
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .collect();
    if sources.is_empty() {
        return None;
    }
    let workspace_in_repo = normalize_workspace_in_repo_path(workspace_in_repo_path);
    let mut scope = HashSet::new();
    for source in sources {
        add_candidate(&mut scope, source);
        if path::is_absolute(source) {
            for base in [repo_root, workspace_path] {
                let relative = path::relative(base, source);
                if relative_inside(&relative) {
                    add_candidate(&mut scope, &relative);
                }
            }
            continue;
        }
        let normalized = normalize_scope_path(source);
        if workspace_in_repo != "." && !normalized.starts_with(&format!("{workspace_in_repo}/")) {
            add_candidate(&mut scope, &format!("{workspace_in_repo}/{normalized}"));
        }
    }
    (!scope.is_empty()).then_some(scope)
}

/// `filterCommitMessageFilesByCurrentSession`。
fn filter_by_session(
    files: Vec<FileChange>,
    workspace_path: &str,
    repo_root: &str,
    workspace_in_repo_path: &str,
    session_paths: Option<&[String]>,
) -> Vec<FileChange> {
    let Some(scope) = file_scope(
        workspace_path,
        repo_root,
        workspace_in_repo_path,
        session_paths,
    ) else {
        return files;
    };
    files
        .into_iter()
        .filter(|file| {
            [
                &file.path,
                &file.repo_relative_path,
                &file.workspace_relative_path,
            ]
            .iter()
            .any(|candidate| scope.contains(&normalize_scope_path(candidate)))
        })
        .collect()
}

// ── gitCommitMessageGenerator.ts：文本工具 ──

/// JS 字符串按 UTF-16 code unit 计长；这里按 char 近似（只影响 CJK 以外的代理对字符）。
fn char_len(value: &str) -> usize {
    value.chars().count()
}

fn slice_chars(value: &str, max: usize) -> &str {
    match value.char_indices().nth(max) {
        Some((index, _)) => &value[..index],
        None => value,
    }
}

/// `clipText`。
fn clip_text(value: &str, max_chars: usize, marker: &str) -> String {
    if char_len(value) <= max_chars {
        return value.to_owned();
    }
    let keep = max_chars.saturating_sub(char_len(marker) + 1);
    format!("{}\n{marker}", slice_chars(value, keep).trim_end())
}

/// `normalizeConversationContextText`。
fn normalize_conversation_text(value: &str) -> String {
    let text = value.replace("\r\n", "\n").replace('\r', "\n");
    let trailing = Regex::new(r"[ \t]+\n").expect("static regex");
    let blank = Regex::new(r"\n{3,}").expect("static regex");
    let text = trailing.replace_all(&text, "\n");
    blank.replace_all(&text, "\n\n").trim().to_owned()
}

/// `stripGeneratedCommitMessageDecorations`。
fn strip_decorations(value: &str) -> String {
    let trimmed = value.trim();
    let fence = Regex::new(r"^```(?:[a-zA-Z0-9_-]+)?\s*([\s\S]*?)\s*```$").expect("static regex");
    let without_fence = fence
        .captures(trimmed)
        .and_then(|captures| captures.get(1))
        .map_or(trimmed, |inner| inner.as_str());
    let prefix = Regex::new(r"(?i)^commit message:\s*").expect("static regex");
    let without_prefix = prefix.replace(without_fence, "").trim().to_owned();
    let quoted = |quote: char| {
        without_prefix.len() >= 2
            && without_prefix.starts_with(quote)
            && without_prefix.ends_with(quote)
    };
    if quoted('"') || quoted('\'') {
        return without_prefix[1..without_prefix.len() - 1].to_owned();
    }
    without_prefix
}

/// `validateGeneratedGitCommitMessage`：Ok(message) 或 Err(preview)。
fn validate_message(raw: &str) -> Result<String, Option<String>> {
    let stripped = strip_decorations(raw);
    let message = slice_chars(stripped.trim(), MAX_COMMIT_MESSAGE_CHARS)
        .trim()
        .to_owned();
    if message.is_empty() {
        return Err(None);
    }
    let subject = message
        .split('\n')
        .next()
        .unwrap_or("")
        .trim_end_matches('\r')
        .trim();
    let conventional = Regex::new(
        r"^(feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert)(\([^)]+\))?!?: .{1,100}$",
    )
    .expect("static regex");
    if !conventional.is_match(subject) {
        let preview = if subject.is_empty() {
            slice_chars(&message, 120).to_owned()
        } else {
            subject.to_owned()
        };
        return Err(Some(preview));
    }
    Ok(message)
}

// ── prompt ──

fn diff_summary(diffs: &[(String, Option<String>, Option<String>)]) -> String {
    let mut total = 0usize;
    let mut chunks: Vec<String> = Vec::new();
    for (diff_path, patch, summary) in diffs.iter().take(MAX_DIFF_FILES) {
        let header = format!("--- {diff_path}");
        let body = [patch, summary]
            .into_iter()
            .flatten()
            .map(|text| text.trim())
            .find(|text| !text.is_empty())
            .unwrap_or("(diff unavailable)");
        let remaining = MAX_DIFF_CHARS.saturating_sub(total);
        if remaining == 0 {
            break;
        }
        let clipped = clip_text(
            body,
            MAX_DIFF_CHARS_PER_FILE.min(remaining),
            "...diff truncated...",
        );
        let chunk = format!("{header}\n{clipped}");
        total += char_len(&chunk);
        chunks.push(chunk);
    }
    chunks.join("\n\n")
}

fn conversation_summary(context: Option<&ConversationContext>) -> String {
    let Some(context) = context.filter(|context| !context.messages.is_empty()) else {
        return String::new();
    };
    let messages = &context.messages;
    let mut chunks: Vec<String> = Vec::new();
    let mut total = 0usize;
    let earlier = context
        .omitted_message_count
        .filter(|count| count.is_finite())
        .unwrap_or(0.0)
        + messages
            .len()
            .saturating_sub(MAX_CONVERSATION_CONTEXT_MESSAGES) as f64;
    if earlier > 0.0 {
        chunks.push(format!("- {earlier} earlier messages omitted"));
    }
    let start = messages
        .len()
        .saturating_sub(MAX_CONVERSATION_CONTEXT_MESSAGES);
    for message in &messages[start..] {
        let content = normalize_conversation_text(&message.content);
        if content.is_empty() {
            continue;
        }
        let role = if message.role == "assistant" {
            "Assistant"
        } else {
            "User"
        };
        let clipped = clip_text(
            &content,
            MAX_CONVERSATION_CONTEXT_CHARS_PER_MESSAGE,
            "...message truncated...",
        );
        let next = format!("{role}: {clipped}");
        let remaining = MAX_CONVERSATION_CONTEXT_CHARS.saturating_sub(total);
        if remaining == 0 {
            break;
        }
        let chunk = clip_text(&next, remaining, "...conversation truncated...");
        total += char_len(&chunk) + 1;
        chunks.push(chunk);
    }
    chunks.join("\n")
}

/// 缺省 locale：Node 读 `Intl` 默认 locale；Host 读进程环境的 POSIX locale 变量。
pub fn runtime_locale(vars: &std::collections::BTreeMap<String, String>) -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|key| vars.get(*key))
        .map(|value| value.trim())
        .find(|value| !value.is_empty())
        .map(str::to_owned)
}

fn kind_text(file: &FileChange) -> String {
    serde_json::to_value(file.kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn build_prompt(
    branch_name: Option<&str>,
    language: &str,
    files: &[FileChange],
    diffs: &[(String, Option<String>, Option<String>)],
    context: Option<&ConversationContext>,
) -> String {
    let branch = branch_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("(detached or unknown)");
    let visible = &files[..files.len().min(MAX_PROMPT_FILES)];
    let omitted = files.len() - visible.len();
    let file_summary = visible
        .iter()
        .map(|file| {
            format!(
                "- {} {} (+{}/-{})",
                kind_text(file),
                file.repo_relative_path,
                file.added,
                file.removed
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let diffs = diff_summary(diffs);
    let conversation = conversation_summary(context);
    let lines: Vec<String> = vec![
        "Write exactly one Git commit message for the workspace changes below.".into(),
        "Return only the commit message text.".into(),
        String::new(),
        "Hard requirements:".into(),
        "- The first line must be a valid Conventional Commit subject.".into(),
        "- Use one of: feat, fix, docs, style, refactor, perf, test, build, ci, chore, revert."
            .into(),
        "- Keep the Conventional Commit type and optional scope in English.".into(),
        "- Write the subject and any body in the current language.".into(),
        "- Keep the subject under 72 characters.".into(),
        "- Use the current session conversation context only to infer user intent.".into(),
        "- Do not mention the conversation, chat, prompt, or user request explicitly.".into(),
        "- Do not explain your reasoning.".into(),
        "- Do not repeat these instructions.".into(),
        String::new(),
        format!("Current branch: {branch}"),
        format!("Current language: {language}"),
        String::new(),
        "Current session conversation context:".into(),
        if conversation.is_empty() {
            "(not provided)".into()
        } else {
            conversation
        },
        String::new(),
        "Changed files:".into(),
        if file_summary.is_empty() {
            "- (no file summary available)".into()
        } else {
            file_summary
        },
        if omitted > 0 {
            format!("- ... {omitted} more files")
        } else {
            String::new()
        },
        String::new(),
        "Diff excerpts:".into(),
        if diffs.is_empty() {
            "(diff unavailable; infer only from the changed file summary)".into()
        } else {
            diffs
        },
    ];
    // 与 Node 一样过滤掉所有空行（包括分隔用的空行）。
    lines
        .into_iter()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

// ── 生成流程 ──

/// `generateCommitMessage`：状态 → 会话文件过滤 → 前 8 个文件的 diff → 当前模型 → 生成 → 校验。
pub async fn generate(
    repo: &GitRepo,
    generator: &Arc<dyn TextGenerator>,
    request: GenerateRequest,
    default_locale: Option<&str>,
) -> Result<Value, RpcError> {
    let workspace_path = request.workspace_path.as_str();
    let include_unstaged = request.include_unstaged.unwrap_or(true);
    let status = repo.status(workspace_path).await?;
    let mut files = if include_unstaged {
        changes_for_source(&status, false)
    } else {
        Vec::new()
    };
    files.extend(changes_for_source(&status, true));
    let files = filter_by_session(
        files,
        workspace_path,
        &status.resolution.repo_root,
        &status.resolution.workspace_in_repo_path,
        request.current_session_file_paths.as_deref(),
    );
    if files.is_empty() {
        return Err(error("There are no changes available to commit."));
    }

    let queries: Vec<(&str, &str)> = files
        .iter()
        .take(COMMIT_MESSAGE_DIFF_FILE_LIMIT)
        .filter_map(|file| match file.section {
            "staged" => Some((file.path.as_str(), "staged")),
            _ if include_unstaged => Some((file.path.as_str(), "unstaged")),
            _ => None,
        })
        .collect();
    let results = futures_util::future::join_all(
        queries
            .iter()
            .map(|(file, source)| repo.diff(workspace_path, file, None, Some(source))),
    )
    .await;
    let diffs: Vec<(String, Option<String>, Option<String>)> = results
        .into_iter()
        .filter_map(Result::ok)
        .filter(|diff| {
            diff.patch.as_deref().is_some_and(|text| !text.is_empty())
                || diff.summary.as_deref().is_some_and(|text| !text.is_empty())
        })
        .map(|diff| (diff.path, diff.patch, diff.summary))
        .collect();

    let selection = resolve_model(generator.current_model().await)?;
    let locale = request.locale.as_deref().or(default_locale);
    let language = if locale.is_some_and(|value| value.to_lowercase().starts_with("zh")) {
        "Chinese"
    } else {
        "English"
    };
    let prompt = build_prompt(
        status.summary.branch_name.as_deref(),
        language,
        &files,
        &diffs,
        request.conversation_context.as_ref(),
    );
    log::info(format!(
        "开始生成 Git 提交消息: provider={} model={} files={} diffs={}",
        selection["providerId"].as_str().unwrap_or_default(),
        selection["modelId"].as_str().unwrap_or_default(),
        files.len(),
        diffs.len()
    ));

    let mut params = json!({
        "workspacePath": workspace_path,
        "selection": selection,
        "prompt": prompt,
        "querySource": COMMIT_MESSAGE_QUERY_SOURCE,
    });
    if let Some(identity) = request
        .workspace_identity
        .as_deref()
        .filter(|identity| !identity.is_empty())
    {
        params["workspaceIdentity"] = json!(identity);
    }
    let raw = match generator.generate_text(params).await {
        Ok(result) => match result["text"].as_str().map(str::trim) {
            Some(text) if !text.is_empty() => text.to_owned(),
            _ => return Err(request_failed("模型响应缺少文本内容。")),
        },
        Err(failure) => return Err(request_failed(&failure.message)),
    };
    match validate_message(&raw) {
        Ok(message) => Ok(json!({
            "message": message,
            "providerId": selection["providerId"],
            "model": selection["modelId"],
        })),
        Err(preview) => {
            // 模型可能重复 prompt 或返回长文本：只把短 preview 记到 debug，UI 只显示通用失败。
            log::debug(format!(
                "模型生成的 Git 提交消息不合规: {}",
                preview.as_deref().unwrap_or("<empty>")
            ));
            Err(generation_error(
                "模型没有返回可用的 Conventional Commit 提交消息。",
                "invalid-output",
                preview.as_deref(),
            ))
        }
    }
}

/// `GitCommitMessageGenerationError`：保留 `reason` 与 `detail`，以 data 形式带给调用方。
fn generation_error(message: &str, reason: &str, detail: Option<&str>) -> RpcError {
    let mut data = json!({"reason": reason});
    if let Some(detail) = detail {
        data["detail"] = json!(detail);
    }
    RpcError::new("GitCommitMessageGenerationError", message).with_data(data)
}

fn request_failed(detail: &str) -> RpcError {
    generation_error("模型请求失败。", "request-failed", Some(detail))
}

/// `resolveCurrentModel`：provider 与 model 都要非空，options 原样带上。
fn resolve_model(current: Option<Value>) -> Result<Value, RpcError> {
    let current = current.unwrap_or(Value::Null);
    let pick = |key: &str| {
        current[key]
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let (Some(provider_id), Some(model_id)) = (pick("providerId"), pick("modelId")) else {
        return Err(generation_error(
            "未读取到当前模型。",
            "model-unavailable",
            None,
        ));
    };
    let mut selection = json!({"providerId": provider_id, "modelId": model_id});
    if let Some(options) = current.get("options").filter(|options| options.is_object()) {
        selection["options"] = options.clone();
    }
    Ok(selection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_strips_decorations_like_node() {
        assert_eq!(
            validate_message("```text\nfeat: add parser\n\nbody\n```"),
            Ok("feat: add parser\n\nbody".to_owned())
        );
        assert_eq!(
            validate_message("Commit message: \"fix(ui): align\""),
            Ok("fix(ui): align".to_owned())
        );
        assert_eq!(validate_message("   "), Err(None));
        assert_eq!(
            validate_message("Here is a message\nfeat: x"),
            Err(Some("Here is a message".to_owned()))
        );
        assert!(validate_message("feat!: breaking change").is_ok());
    }

    #[test]
    fn clip_and_conversation_rules_match_node() {
        assert_eq!(clip_text("abcdef", 10, "..."), "abcdef");
        // maxChars - marker.length - 1 个字符，trimEnd 后接换行与 marker。
        assert_eq!(clip_text("abcdefghij", 8, "..."), "abcd\n...");
        assert_eq!(normalize_conversation_text("a  \r\n\n\n\nb\r"), "a\n\nb");
        let context = ConversationContext {
            messages: (0..14)
                .map(|index| ConversationMessage {
                    role: if index % 2 == 0 { "user" } else { "assistant" }.into(),
                    content: format!("m{index}"),
                })
                .collect(),
            omitted_message_count: Some(3.0),
        };
        let summary = conversation_summary(Some(&context));
        assert!(summary.starts_with("- 5 earlier messages omitted\nUser: m2\nAssistant: m3"));
    }

    #[test]
    fn session_scope_accepts_absolute_and_workspace_relative_paths() {
        let root = if cfg!(windows) { r"C:\repo" } else { "/repo" };
        let workspace = if cfg!(windows) {
            r"C:\repo\app"
        } else {
            "/repo/app"
        };
        let absolute = if cfg!(windows) {
            r"C:\repo\app\src\a.ts"
        } else {
            "/repo/app/src/a.ts"
        };
        let scope = file_scope(
            workspace,
            root,
            "app",
            Some(&[absolute.to_owned(), "lib/b.ts".to_owned(), "  ".to_owned()]),
        )
        .unwrap();
        assert!(scope.contains("app/src/a.ts"));
        assert!(scope.contains("src/a.ts"));
        assert!(scope.contains("lib/b.ts"));
        assert!(scope.contains("app/lib/b.ts"));
        assert!(file_scope(workspace, root, "app", Some(&["".to_owned()])).is_none());
        assert!(file_scope(workspace, root, "app", None).is_none());
    }

    #[test]
    fn model_resolution_requires_provider_and_model() {
        let error = resolve_model(Some(json!({"providerId": " ", "modelId": "m"}))).unwrap_err();
        assert_eq!(error.message, "未读取到当前模型。");
        let selection = resolve_model(Some(json!({
            "providerId": " p ", "modelId": "m", "options": {"reasoningLevel": "high"}
        })))
        .unwrap();
        assert_eq!(
            selection,
            json!({"providerId": "p", "modelId": "m", "options": {"reasoningLevel": "high"}})
        );
    }
}
