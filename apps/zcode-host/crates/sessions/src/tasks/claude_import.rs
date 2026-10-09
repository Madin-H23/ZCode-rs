//! Claude Code 会话的扫描与导入（claude-native/claudeNativeSessionImportRepo.ts、
//! claudeNativeSessionImportService.ts、persistImportedClaudeTask.ts、buildImportedClaudeTaskFile.ts）。
//!
//! 只读 `~/.claude/projects`；导入时把 jsonl 原子复制到
//! `<dataBaseDir>/.zcode/v2/agent-config/claude/<wsHash>/projects/…`，经 CLI `session/create`
//! （immediate、importedHistory）建成真实会话，写 legacy 快照文件与 tasks-index，再广播。
use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};
use zcode_host_kit::{RpcError, log};

use super::claude_parse::{self as parse, ImportedSource, Record};
use super::context::{TaskContext, repo_error, target_of};
use super::lifecycle::{TaskLifecycle, now_ms};
use crate::agent::WorkspaceTarget;

const HEAD_LINES: usize = 16;
const SEARCH_TEXT_MAX: usize = 200_000;
const IGNORED_DIRS: &[&str] = &["subagents", "worktree", "worktrees"];

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `getWorkspaceHash`：workspace key 的 sha256 前 12 位。
pub fn workspace_hash(workspace_path: &str, workspace_identity: Option<&str>) -> String {
    let key = workspace_identity
        .map(str::trim)
        .filter(|identity| !identity.is_empty())
        .unwrap_or(workspace_path);
    sha256_hex(key)[..12].to_owned()
}

/// `buildImportedClaudeTaskId`。
pub fn imported_task_id(workspace_path: &str, session_id: &str) -> String {
    format!(
        "claude-import-{}",
        &sha256_hex(&format!("claude:{workspace_path}:{session_id}"))[..24]
    )
}

/// `normalize(resolve(path))`（Windows 上再转小写）。
fn normalize_for_comparison(path: &str) -> String {
    let path = Path::new(path);
    #[expect(clippy::disallowed_methods, reason = "存量，U3 改为显式传入")]
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    let text = out.to_string_lossy().into_owned();
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

fn is_worktree_workspace(path: &str) -> bool {
    let normalized = normalize_for_comparison(path).replace('\\', "/");
    let segments: Vec<String> = normalized.split('/').map(str::to_lowercase).collect();
    segments.iter().enumerate().any(|(index, segment)| {
        segment == ".claude"
            && segments
                .get(index + 1)
                .is_some_and(|next| next == "worktree" || next == "worktrees")
    })
}

/// 去重后的 `<home>/.claude/projects`：os.homedir()、`HOME`、dataBaseDir。
fn projects_roots(context: &TaskContext) -> Vec<PathBuf> {
    let mut homes = vec![context.paths.user_home.clone()];
    if !homes.contains(&context.paths.data_base_dir) {
        homes.push(context.paths.data_base_dir.clone());
    }
    homes
        .into_iter()
        .map(|home| home.join(".claude").join("projects"))
        .collect()
}

async fn collect_jsonl(root: &Path, out: &mut Vec<PathBuf>) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        let mut children = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            children.push(entry);
        }
        // 深度优先、目录内按读出顺序（同 Node 的递归 readdir）。
        for entry in children.into_iter().rev() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(kind) = entry.file_type().await else {
                continue;
            };
            if kind.is_dir() {
                if !IGNORED_DIRS.contains(&name.to_lowercase().as_str()) {
                    stack.push(entry.path());
                }
            } else if kind.is_file() && name.ends_with(".jsonl") {
                out.push(entry.path());
            }
        }
    }
}

/// 读前 16 条非空记录；任何一行不是 JSON 都算失败（`readJsonLinesFileHead`）。
async fn read_head(path: &Path) -> Result<Vec<Record>, String> {
    use tokio::io::AsyncBufReadExt as _;
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| e.to_string())?;
    let mut lines = tokio::io::BufReader::new(file).lines();
    let mut records = Vec::new();
    let mut index = 0;
    while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
        index += 1;
        let line = line.strip_suffix('\r').unwrap_or(&line).to_owned();
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&line).map_err(|error| {
            format!(
                "[claude-native] 解析 JSONL 失败 {}:{index} {error}",
                path.display()
            )
        })?;
        records.push(value.as_object().cloned().unwrap_or_default());
        if records.len() >= HEAD_LINES {
            break;
        }
    }
    Ok(records)
}

async fn read_all(path: &Path) -> Result<Vec<Record>, String> {
    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| e.to_string())?;
    let mut records = Vec::new();
    for (index, line) in text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .filter(|l| !l.trim().is_empty())
        .enumerate()
    {
        let value: Value = serde_json::from_str(line).map_err(|error| {
            format!(
                "[claude-native] 解析 JSONL 失败 {}:{} {error}",
                path.display(),
                index + 1
            )
        })?;
        records.push(value.as_object().cloned().unwrap_or_default());
    }
    Ok(records)
}

fn mtime_ms(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// 一个候选（`ZCodeImportableSessionCandidate`）；workspace 不符或不是可导入会话时为 None。
async fn candidate(path: &Path, filter: Option<&str>) -> Result<Option<Value>, String> {
    let metadata = tokio::fs::metadata(path).await.map_err(|e| e.to_string())?;
    let head = read_head(path).await?;
    if parse::has_sidechain_marker(&head) {
        return Ok(None);
    }
    let info = parse::head_info(&head);
    let Some(workspace) = info.workspace_path else {
        return Ok(None);
    };
    if is_worktree_workspace(&workspace)
        || filter.is_some_and(|filter| normalize_for_comparison(&workspace) != filter)
    {
        return Ok(None);
    }
    let session_id = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut item = Map::new();
    item.insert("provider".into(), json!("claude"));
    item.insert("sessionId".into(), json!(session_id));
    item.insert("workspacePath".into(), json!(workspace));
    item.insert("sourcePath".into(), json!(path.to_string_lossy()));
    item.insert("updatedAt".into(), json!(mtime_ms(&metadata)));
    if let Some(created) = info.created_at.filter(|c| *c != 0) {
        item.insert("createdAt".into(), json!(created));
    }
    if let Some(title) = info.preview_title.filter(|t| !t.is_empty()) {
        item.insert("previewTitle".into(), json!(title));
    }
    Ok(Some(Value::Object(item)))
}

pub async fn scan(context: &TaskContext, params: &Value) -> Value {
    let filter = params["workspacePath"]
        .as_str()
        .filter(|p| !p.is_empty())
        .map(normalize_for_comparison);
    let mut files = Vec::new();
    for root in projects_roots(context) {
        collect_jsonl(&root, &mut files).await;
    }
    let mut seen = std::collections::HashSet::new();
    files.retain(|file| seen.insert(file.clone()));
    let modified_since = params["modifiedSince"].as_f64().filter(|v| *v != 0.0);
    let mut candidates = Vec::new();
    for file in &files {
        let Ok(metadata) = tokio::fs::metadata(file).await else {
            continue;
        };
        if modified_since.is_some_and(|since| (mtime_ms(&metadata) as f64) < since) {
            continue;
        }
        match candidate(file, filter.as_deref()).await {
            Ok(Some(item)) => candidates.push(item),
            Ok(None) => {}
            Err(error) => log::warn(format!(
                "扫描 Claude 原生 session 失败 path={}: {error}",
                file.display()
            )),
        }
    }
    candidates.sort_by_key(|item| std::cmp::Reverse(item["updatedAt"].as_i64().unwrap_or(0)));
    if let Some(limit) = params["limit"].as_f64().filter(|l| *l > 0.0) {
        candidates.truncate(limit as usize);
    }
    log::info(format!(
        "Claude 原生 session 扫描完成 workspaceFilter={} fileCount={} candidateCount={}",
        params["workspacePath"].as_str().unwrap_or("all"),
        files.len(),
        candidates.len()
    ));
    Value::Array(candidates)
}

/// `findImportableSession`：按 `<sessionId>.jsonl` 在各 root 下递归查找。
async fn find(context: &TaskContext, session_id: &str, filter: Option<&str>) -> Option<Value> {
    let file_name = format!("{session_id}.jsonl");
    for root in projects_roots(context) {
        let mut files = Vec::new();
        collect_jsonl(&root, &mut files).await;
        let Some(path) = files.into_iter().find(|f| {
            f.file_name()
                .is_some_and(|n| n.to_string_lossy() == file_name)
        }) else {
            continue;
        };
        // 侧链或 worktree 会话直接判定不可导入；workspace 不符时继续找下一个 root。
        let head = match read_head(&path).await {
            Ok(head) => head,
            Err(error) => {
                log::warn(format!(
                    "查找 Claude 原生 session 失败 sessionId={session_id}: {error}"
                ));
                continue;
            }
        };
        if parse::has_sidechain_marker(&head) {
            return None;
        }
        let Some(workspace) = parse::head_info(&head).workspace_path else {
            continue;
        };
        if is_worktree_workspace(&workspace) {
            return None;
        }
        match candidate(&path, filter).await {
            Ok(Some(found)) => return Some(found),
            Ok(None) => continue,
            Err(error) => log::warn(format!(
                "查找 Claude 原生 session 失败 sessionId={session_id}: {error}"
            )),
        }
    }
    None
}

fn temp_suffix() -> String {
    format!(
        "{}.{}.tmp",
        std::process::id(),
        crate::tasks::agent_calls::uuid_v7()
    )
}

/// `copySessionFileToWorkspace`：原子复制；返回本次新建的文件（失败时清理用）。
async fn copy_to_workspace(
    context: &TaskContext,
    workspace: &str,
    identity: Option<&str>,
    source: &str,
) -> Result<Vec<PathBuf>, String> {
    let marker = format!("{0}projects{0}", std::path::MAIN_SEPARATOR);
    let relative = source
        .rfind(&marker)
        .map(|index| &source[index + marker.len()..])
        .ok_or_else(|| format!("[claude-native] Claude 原生 session 路径非法: {source}"))?;
    let output = context
        .paths
        .data_base_dir
        .join(".zcode/v2/agent-config/claude")
        .join(workspace_hash(workspace, identity))
        .join("projects")
        .join(relative);
    let existed = tokio::fs::try_exists(&output).await.unwrap_or(false);
    let temp = PathBuf::from(format!("{}.{}", output.display(), temp_suffix()));
    if let Some(parent) = output.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| e.to_string())?;
    }
    tokio::fs::copy(source, &temp)
        .await
        .map_err(|e| e.to_string())?;
    tokio::fs::rename(&temp, &output)
        .await
        .map_err(|e| e.to_string())?;
    Ok(if existed { Vec::new() } else { vec![output] })
}

/// `buildSearchableTextFromMessages`：非空内容以换行连接，截到 200k 个 UTF-16 单元。
fn searchable_text(messages: &[Value]) -> String {
    let mut out = String::new();
    let mut total = 0usize;
    for message in messages {
        let content = message["content"].as_str().map(str::trim).unwrap_or("");
        if content.is_empty() {
            continue;
        }
        let next = if total > 0 {
            format!("\n{content}")
        } else {
            content.to_owned()
        };
        let units = next.encode_utf16().count();
        if total + units > SEARCH_TEXT_MAX {
            out.push_str(zcode_cli_domain::js_string::utf16_prefix(
                &next,
                SEARCH_TEXT_MAX - total,
            ));
            break;
        }
        out.push_str(&next);
        total += units;
    }
    out
}

/// legacy 快照（`~/.zcode/v2/sessions/<wsHash>/<taskId>.json`）：按清洗规则去掉 meta.mode、
/// meta.model、meta.provider 与 messages[].model，pretty JSON 加换行，原子写入。
async fn write_legacy_snapshot(
    context: &TaskContext,
    source: &ImportedSource,
    task_id: &str,
    identity: Option<&str>,
) -> Result<(), String> {
    let mut meta = Map::new();
    meta.insert("taskId".into(), json!(task_id));
    meta.insert("traceId".into(), json!(crate::agent::service::uuid_v4()));
    let title = Some(source.title.trim())
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            source
                .messages
                .iter()
                .find(|m| {
                    m["role"] == "user"
                        && m["content"].as_str().is_some_and(|c| !c.trim().is_empty())
                })
                .and_then(|m| m["content"].as_str())
                .map(parse::derive_session_title)
                .unwrap_or_else(|| "Imported session".to_owned())
        });
    meta.insert("title".into(), json!(title));
    meta.insert("workspacePath".into(), json!(source.workspace_path));
    meta.insert("createdAt".into(), json!(source.created_at));
    meta.insert(
        "updatedAt".into(),
        json!(source.created_at.max(source.updated_at)),
    );
    meta.insert("migrationSource".into(), json!("claudeCode"));
    meta.insert("status".into(), json!("completed"));
    if let Some(identity) = identity {
        meta.insert("workspaceIdentity".into(), json!(identity));
    }
    let messages: Vec<Value> = source
        .messages
        .iter()
        .map(|message| {
            let mut message = message.clone();
            if let Some(object) = message.as_object_mut() {
                object.shift_remove("model");
            }
            message
        })
        .collect();
    let file = json!({"meta": meta, "messages": messages});
    let path = context
        .paths
        .data_base_dir
        .join(".zcode/v2/sessions")
        .join(workspace_hash(&source.workspace_path, identity))
        .join(format!("{task_id}.json"));
    let text = format!(
        "{}\n",
        serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?
    );
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| e.to_string())?;
    }
    let temp = PathBuf::from(format!("{}.{}", path.display(), temp_suffix()));
    tokio::fs::write(&temp, text)
        .await
        .map_err(|e| e.to_string())?;
    tokio::fs::rename(&temp, &path)
        .await
        .map_err(|e| e.to_string())
}

fn entry(session_id: &str, workspace: Option<&str>, extra: &[(&str, Value)]) -> Value {
    let mut item = Map::new();
    item.insert("provider".into(), json!("claude"));
    item.insert("sessionId".into(), json!(session_id));
    for (key, value) in extra {
        item.insert((*key).into(), value.clone());
    }
    if let Some(workspace) = workspace {
        item.insert("workspacePath".into(), json!(workspace));
    }
    Value::Object(item)
}

impl TaskLifecycle {
    /// `scanImportableClaudeSessions`（workspaceIdentity 只影响导入落盘目录，扫描不看它）。
    pub async fn scan_claude_sessions(&self, params: &Value) -> Value {
        scan(&self.context, params).await
    }

    /// 一个会话：建 CLI 会话 → 写索引 → legacy 快照 → 再写索引（解除归档 / 删除）→ 广播。
    async fn import_one(
        &self,
        source: &ImportedSource,
        identity: Option<&str>,
    ) -> Result<Value, RpcError> {
        let target = WorkspaceTarget {
            workspace_path: source.workspace_path.clone(),
            workspace_identity: identity.map(str::to_owned),
            remote_session_id: None,
        };
        let task_id = imported_task_id(&source.workspace_path, &source.session_id);
        let history: Vec<Value> = source
            .messages
            .iter()
            .map(|m| json!({"role": m["role"], "content": m["content"], "timestamp": m["timestamp"]}))
            .collect();
        let create = json!({
            "sessionId": task_id,
            "workspace": target.workspace_ref(),
            "persistence": "immediate",
            "importedHistory": {
                "source": "claudeCode",
                "title": source.title,
                "createdAt": source.created_at,
                "updatedAt": source.updated_at,
                "messages": history,
            },
        });
        let snapshot =
            super::agent_calls::request(&self.context.runtime, &target, "session/create", create)
                .await?;
        let meta = self
            .sync_snapshot(&snapshot, Some(json!({"migrationSource": "claudeCode"})))
            .await?;
        let task_id = meta["taskId"].as_str().unwrap_or(&task_id).to_owned();
        write_legacy_snapshot(&self.context, source, &task_id, identity)
            .await
            .map_err(|message| RpcError::new("Error", message))?;
        let mut final_meta = meta.clone();
        final_meta["migrationSource"] = json!("claudeCode");
        if let Some(identity) = identity {
            final_meta["workspaceIdentity"] = json!(identity);
        }
        let index = &self.context.index;
        let persisted = index
            .sync_task_meta(final_meta, Some(searchable_text(&source.messages)))
            .await
            .map_err(repo_error)?;
        // 重新导入要让以前归档 / 删除过的同一会话重新可见（TS 同一次写入带 archived:false、deleted:false）。
        let key = target_of(&persisted).key();
        let persisted = index
            .restore_task_visibility(key, task_id.clone())
            .await
            .map_err(repo_error)?
            .unwrap_or(persisted);
        self.context.remember(&persisted);
        self.context.emit_list_changed(
            &target_of(&persisted),
            Some(&task_id),
            "task_meta_changed",
            Some(&persisted),
        );
        Ok(persisted)
    }

    pub async fn import_claude_sessions(&self, params: &Value) -> Value {
        let workspace = params["workspacePath"].as_str().filter(|p| !p.is_empty());
        let identity = workspace
            .and(params["workspaceIdentity"].as_str())
            .filter(|i| !i.is_empty());
        let filter = workspace.map(normalize_for_comparison);
        let mut ids: Vec<String> = Vec::new();
        for id in params["sessionIds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let id = id.trim();
            if !id.is_empty() && !ids.iter().any(|existing| existing == id) {
                ids.push(id.to_owned());
            }
        }
        let (mut imported, mut skipped, mut failed) = (Vec::new(), Vec::new(), Vec::new());
        for id in &ids {
            let Some(found) = find(&self.context, id, filter.as_deref()).await else {
                skipped.push(entry(
                    id,
                    workspace,
                    &[("reason", json!("session_not_found_or_workspace_mismatch"))],
                ));
                continue;
            };
            let found_workspace = found["workspacePath"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            if !tokio::fs::try_exists(&found_workspace)
                .await
                .unwrap_or(false)
            {
                skipped.push(entry(
                    id,
                    Some(&found_workspace),
                    &[("reason", json!("workspace_path_missing"))],
                ));
                continue;
            }
            let source_path = found["sourcePath"].as_str().unwrap_or_default().to_owned();
            let mut created = Vec::new();
            let result: Result<Value, String> = async {
                created =
                    copy_to_workspace(&self.context, &found_workspace, identity, &source_path)
                        .await?;
                let records = read_all(Path::new(&source_path)).await?;
                let metadata = tokio::fs::metadata(&source_path)
                    .await
                    .map_err(|e| e.to_string())?;
                let birth = metadata
                    .created()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64);
                let source = parse::parse_records(
                    &records,
                    &found_workspace,
                    id,
                    &source_path,
                    found["createdAt"].as_i64().or(birth),
                    found["updatedAt"].as_i64().or(Some(mtime_ms(&metadata))),
                    now_ms() as i64,
                )?;
                self.import_one(&source, identity)
                    .await
                    .map_err(|error| error.message)
            }
            .await;
            match result {
                Ok(meta) => imported.push(json!({
                    "provider": "claude", "sessionId": id, "taskId": meta["taskId"], "workspacePath": meta["workspacePath"],
                })),
                Err(reason) => {
                    for path in &created {
                        let _ = tokio::fs::remove_file(path).await;
                    }
                    failed.push(entry(id, Some(&found_workspace), &[("reason", json!(reason))]));
                }
            }
        }
        log::info(format!(
            "Claude 原生 session 导入完成 imported={} skipped={} failed={}",
            imported.len(),
            skipped.len(),
            failed.len()
        ));
        json!({"imported": imported, "skipped": skipped, "failed": failed})
    }
}
