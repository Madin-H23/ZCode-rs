//! 分享导入（conversationShareService.ts 的 importShare、getImportedConversation、导入索引与
//! 启动清理，sharedContextFormatter.ts）。
//!
//! ```text
//! importShare ─► 去重（已完成 / 在途，按 shareCode + workspace）─► continuation（完整性复核）
//!   ─► <workspace>/.zcode-share/<shareId>/：marker → staging 下载（超时、Content-Length、size + sha256）
//!   ─► rename 到 shared-artifacts → 写 shared-conversation.json（原样 rows）→ 格式化 shared context
//!   ─► CLI session/create（importedHistory: sharedContext）──► 解除清理武装、删 marker、complete
//! 任一步在 createSession 之前失败：只删本次 import-owned 目录
//! ```
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::sync::Mutex as AsyncMutex;
use zcode_host_kit::{EventHub, log};

use super::client::{ShareClient, decode_rows, encode_uri_component};
use super::errors::{ServiceError, ShareError};
use super::integrity::sha256_hex;
use crate::agent::extensions::UseReason;
use crate::agent::service::uuid_v4;
use crate::agent::{AgentRuntime, WorkspaceTarget};

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const FORMAT_VERSION: i64 = 1;

/// `sanitizeFileSegment`：NFKC 之外与 TS 相同（控制字符与保留字符换成 `-`）。
pub fn sanitize_file_segment(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if (character as u32) < 32 || "<>:\"/\\|?*".contains(character) {
                '-'
            } else {
                character
            }
        })
        .collect()
}

/// `uniqueImportedFileName`。
fn unique_file_name(display_name: &str, artifact_id: &str, used: &mut HashSet<String>) -> String {
    let normalized = display_name.replace('\\', "/");
    let base_name = normalized.rsplit('/').next().unwrap_or_default();
    let trimmed = sanitize_file_segment(base_name)
        .trim_matches(|c| c == '.' || c == ' ')
        .chars()
        .take(180)
        .collect::<String>();
    let base = if trimmed.is_empty() {
        "artifact".to_owned()
    } else {
        trimmed
    };
    let mut candidate = base.clone();
    if used.contains(&candidate.to_lowercase()) {
        let cleaned: String = artifact_id
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect();
        let suffix = if cleaned.is_empty() {
            "artifact".to_owned()
        } else {
            cleaned[cleaned.len().saturating_sub(8)..].to_owned()
        };
        candidate = match base.rfind('.') {
            Some(dot) if dot > 0 => format!("{}-{suffix}{}", &base[..dot], &base[dot..]),
            _ => format!("{base}-{suffix}"),
        };
    }
    used.insert(candidate.to_lowercase());
    candidate
}

fn fenced(value: &str) -> String {
    let fence = if value.contains("```") { "````" } else { "```" };
    format!("{fence}\n{value}\n{fence}")
}

/// `formatSharedContextV1`：返回 (markdown, sha256, 认不出的 kind)。
pub fn format_shared_context(
    title: &str,
    rows: &[Value],
    installed: &[Value],
) -> Result<(String, String, Vec<String>), ServiceError> {
    let by_id: HashMap<&str, &Value> = installed
        .iter()
        .filter_map(|artifact| Some((artifact["artifactId"].as_str()?, artifact)))
        .collect();
    let mut sections = vec![format!("# Shared conversation: {title}")];
    let mut unsupported: Vec<String> = Vec::new();
    let text = |row: &Value, key: &str| row[key].as_str().unwrap_or_default().to_owned();
    for row in rows {
        match row["kind"].as_str().unwrap_or_default() {
            "turnHeader" => {}
            "userInput" => {
                let lines: Vec<String> = row["attachments"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|attachment| {
                        let id = attachment["ref"]
                            .as_str()?
                            .strip_prefix("zcode-artifact://share/")?;
                        let installed = by_id.get(id)?;
                        Some(vec![
                            format!("- {}", text(installed, "displayName")),
                            format!("  - Path: {}", text(installed, "workspaceRelativePath")),
                            format!("  - MIME: {}", text(installed, "mimeType")),
                            format!("  - SHA-256: {}", text(installed, "sha256")),
                        ])
                    })
                    .flatten()
                    .collect();
                let attachments = if lines.is_empty() {
                    String::new()
                } else {
                    format!("\n\n### Attachments\n\n{}", lines.join("\n"))
                };
                sections.push(format!("## User\n\n{}{attachments}", text(row, "text")));
            }
            "assistantText" => sections.push(format!("## Assistant\n\n{}", text(row, "text"))),
            "reasoning" => sections.push(format!("## Reasoning\n\n{}", text(row, "text"))),
            "toolCall" => {
                let output = row["output"]["text"]
                    .as_str()
                    .filter(|output| !output.is_empty())
                    .map(|output| format!("\n\n### Output\n\n{}", fenced(output)))
                    .unwrap_or_default();
                sections.push(format!(
                    "## Tool: {}\n\n### Input\n\n{}{output}",
                    text(row, "toolName"),
                    fenced(&text(row, "inputText"))
                ));
            }
            "timelineMarker" => sections.push(format!(
                "## Timeline\n\n{}",
                fenced(&row["marker"].to_string())
            )),
            "artifact" => {
                let Some(artifact) =
                    by_id.get(row["artifactVersionId"].as_str().unwrap_or_default())
                else {
                    return Err(ServiceError::new(
                        "invalid_contract",
                        "Shared context artifact was not installed",
                    ));
                };
                sections.push(
                    [
                        format!("## Artifact: {}", text(artifact, "displayName")),
                        format!("- Path: {}", text(artifact, "workspaceRelativePath")),
                        format!("- MIME: {}", text(artifact, "mimeType")),
                        format!("- SHA-256: {}", text(artifact, "sha256")),
                    ]
                    .join("\n"),
                );
            }
            kind @ ("subagent" | "hookInvocation") => {
                return Err(ServiceError::new(
                    "invalid_contract",
                    format!("Unsupported shared context row: {kind}"),
                ));
            }
            other => {
                let label = if other.is_empty() { "unknown" } else { other };
                if !unsupported.iter().any(|kind| kind == label) {
                    unsupported.push(label.to_owned());
                }
            }
        }
    }
    let markdown = format!("{}\n", sections.join("\n\n"));
    let sha = sha256_hex(markdown.as_bytes());
    Ok((markdown, sha, unsupported))
}

/// `ImportConversationShareInput`。
#[derive(Debug, Clone, Default)]
pub struct ImportInput {
    pub share_code: String,
    pub client_request_id: String,
    pub target_workspace_path: Option<String>,
    pub target_workspace_identity: Option<String>,
    pub target_workspace_kind: Option<String>,
    pub locale: Option<String>,
}

fn workspace_key_of(path: Option<&str>, identity: Option<&str>) -> String {
    identity
        .map(str::trim)
        .filter(|identity| !identity.is_empty())
        .or_else(|| path.map(str::trim).filter(|path| !path.is_empty()))
        .unwrap_or("__default_conversation_workspace__")
        .to_owned()
}

fn dedupe_key(share_code: &str, workspace_key: &str) -> String {
    format!("{share_code}\u{0}{workspace_key}")
}

fn title_prefix(locale: Option<&str>) -> &'static str {
    match locale {
        Some("en-US") => "From Share: ",
        _ => "来自分享：",
    }
}

type InFlight = tokio::sync::watch::Receiver<Option<Result<Value, ShareError>>>;

pub struct Importer {
    agent: AgentRuntime,
    client: Arc<ShareClient>,
    conversation_root: PathBuf,
    share_web_url: String,
    index_path: PathBuf,
    completed: Mutex<(HashMap<String, Value>, HashMap<String, Value>)>,
    in_flight: Mutex<HashMap<String, InFlight>>,
    index_lock: AsyncMutex<()>,
    loaded: tokio::sync::OnceCell<()>,
    progress: EventHub<Value>,
}

impl Importer {
    pub fn new(
        agent: AgentRuntime,
        client: Arc<ShareClient>,
        conversation_root: PathBuf,
        share_web_url: String,
    ) -> Arc<Self> {
        let index_path = conversation_root.join(".zcode-share-imports.json");
        Arc::new(Self {
            agent,
            client,
            conversation_root,
            share_web_url: share_web_url.trim_end_matches('/').to_owned(),
            index_path,
            completed: Mutex::new((HashMap::new(), HashMap::new())),
            in_flight: Mutex::new(HashMap::new()),
            index_lock: AsyncMutex::new(()),
            loaded: tokio::sync::OnceCell::new(),
            progress: EventHub::new(),
        })
    }

    pub fn progress(&self) -> EventHub<Value> {
        self.progress.clone()
    }

    fn report(&self, operation_id: &str, phase: &str, completed: usize, total: usize) {
        self.progress.emit(json!({
            "operationId": operation_id,
            "phase": phase,
            "completedArtifacts": completed,
            "totalArtifacts": total,
        }));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, (HashMap<String, Value>, HashMap<String, Value>)> {
        self.completed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    async fn ensure_loaded(&self) {
        self.loaded.get_or_init(|| self.load_index()).await;
    }

    /// `loadCompletedImportIndex`：只收下仍存在的 workspace 的记录。
    async fn load_index(&self) {
        let Ok(text) = tokio::fs::read_to_string(&self.index_path).await else {
            return;
        };
        let Ok(Value::Object(raw)) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        for (key, record) in raw {
            let fields = [
                "workspacePath",
                "sessionId",
                "title",
                "contextId",
                "shareUrl",
            ];
            if fields.iter().any(|field| !record[*field].is_string()) {
                continue;
            }
            let path = record["workspacePath"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            if !tokio::fs::metadata(&path)
                .await
                .is_ok_and(|meta| meta.is_dir())
            {
                continue;
            }
            let mut parts = key.split('\u{0}');
            let share_code = parts.next().unwrap_or_default().to_owned();
            let workspace_key = parts.next().map(str::to_owned).unwrap_or_else(|| {
                workspace_key_of(Some(&path), record["workspaceIdentity"].as_str())
            });
            let mut result = Map::new();
            result.insert("workspacePath".into(), json!(path));
            if let Some(identity) = record["workspaceIdentity"]
                .as_str()
                .filter(|value| !value.is_empty())
            {
                result.insert("workspaceIdentity".into(), json!(identity));
            }
            for field in ["sessionId", "contextId", "shareUrl", "title"] {
                result.insert(field.into(), record[field].clone());
            }
            result.insert("reused".into(), json!(true));
            let result = Value::Object(result);
            let mut completed = self.lock();
            completed.0.insert(key, result.clone());
            completed
                .1
                .insert(dedupe_key(&share_code, &workspace_key), result);
        }
    }

    /// `persistCompletedImportIndex`：串行写，临时文件 + rename。
    async fn persist_index(&self) -> std::io::Result<()> {
        let _guard = self.index_lock.lock().await;
        let data: Map<String, Value> = self.lock().0.clone().into_iter().collect();
        let temporary = self
            .index_path
            .with_file_name(format!(".zcode-share-imports.json.{}.tmp", uuid_v4()));
        if let Some(parent) = self.index_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let result = async {
            tokio::fs::write(&temporary, Value::Object(data).to_string()).await?;
            tokio::fs::rename(&temporary, &self.index_path).await
        }
        .await;
        let _ = tokio::fs::remove_file(&temporary).await;
        result
    }

    /// `importShare`：已完成的直接复用（`reused:true`），在途的合并到同一个结果。
    pub async fn import(
        self: &Arc<Self>,
        input: ImportInput,
        operation_id: String,
    ) -> Result<Value, ShareError> {
        self.ensure_loaded().await;
        let workspace_key = workspace_key_of(
            input.target_workspace_path.as_deref(),
            input.target_workspace_identity.as_deref(),
        );
        let keyed_share = dedupe_key(&input.share_code, &workspace_key);
        let key = format!("{keyed_share}\u{0}{}", input.client_request_id);
        {
            let completed = self.lock();
            if let Some(result) = completed
                .0
                .get(&key)
                .or_else(|| completed.1.get(&keyed_share))
            {
                log::info(format!(
                    "conversation share import reused operationId={operation_id} phase=complete"
                ));
                let mut result = result.clone();
                result["reused"] = json!(true);
                return Ok(result);
            }
        }
        let existing = {
            let in_flight = self
                .in_flight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            in_flight
                .get(&key)
                .or_else(|| in_flight.get(&keyed_share))
                .cloned()
        };
        if let Some(mut receiver) = existing {
            loop {
                if let Some(result) = receiver.borrow().clone() {
                    return result;
                }
                if receiver.changed().await.is_err() {
                    return Err(ServiceError::new(
                        "unknown",
                        "Conversation share import was interrupted",
                    )
                    .into());
                }
            }
        }
        let (sender, receiver) = tokio::sync::watch::channel(None);
        {
            let mut in_flight = self
                .in_flight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            in_flight.insert(key.clone(), receiver.clone());
            in_flight.insert(keyed_share.clone(), receiver);
        }
        let result = self.import_internal(&input, &operation_id).await;
        if let Ok(value) = &result {
            {
                let mut completed = self.lock();
                completed.0.insert(key.clone(), value.clone());
                completed.1.insert(keyed_share.clone(), value.clone());
            }
            let _ = self.persist_index().await;
        }
        let _ = sender.send(Some(result.clone()));
        {
            let mut in_flight = self
                .in_flight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            in_flight.remove(&key);
            in_flight.remove(&keyed_share);
        }
        result
    }

    async fn list_sessions(&self, workspace_path: &str) -> Result<Vec<Value>, ShareError> {
        let target = WorkspaceTarget::local(workspace_path);
        let process = self
            .agent
            .get_client(&target, crate::agent::RuntimePolicy::StartIfNeeded)
            .await?;
        let result = process
            .request_value(
                "session/list",
                &json!({"workspace": target.workspace_ref(), "includeArchived": false, "limit": 100}),
            )
            .await?;
        Ok(result["sessions"].as_array().cloned().unwrap_or_default())
    }

    fn marker(
        &self,
        input: &ImportInput,
        ids: &ImportIds,
        workspace_key: &str,
        phase: &str,
    ) -> String {
        json!({
            "importId": ids.import_id,
            "shareCode": input.share_code,
            "clientRequestId": input.client_request_id,
            "sessionId": ids.session_id,
            "shareId": ids.share_id,
            "contextId": ids.context_id,
            "shareUrl": ids.share_url,
            "workspaceKey": workspace_key,
            "phase": phase,
            "createdAt": now_ms(),
        })
        .to_string()
    }

    async fn import_internal(
        &self,
        input: &ImportInput,
        operation_id: &str,
    ) -> Result<Value, ShareError> {
        let continuation = self
            .client
            .continuation(&input.share_code, &input.client_request_id)
            .await?;
        let remote = input.target_workspace_kind.as_deref() == Some("remote")
            || input
                .target_workspace_identity
                .as_deref()
                .is_some_and(|identity| !identity.is_empty());
        let workspace_path = match input
            .target_workspace_path
            .as_deref()
            .filter(|path| !path.is_empty())
        {
            Some(path) if !remote => path.to_owned(),
            _ => self.conversation_root.to_string_lossy().into_owned(),
        };
        let workspace_identity = input
            .target_workspace_identity
            .clone()
            .filter(|identity| !identity.is_empty() && !remote);
        let share_id = continuation["share"]["share_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let share_title = continuation["share"]["title"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let share_root = Path::new(&workspace_path).join(".zcode-share");
        let import_root = share_root.join(sanitize_file_segment(&share_id));
        let marker_path = import_root.join(".zcode-share-import.json");
        let staging = import_root.join(".share-import-staging");
        let final_artifacts = import_root.join("shared-artifacts");
        let conversation_path = import_root.join("shared-conversation.json");
        let ids = ImportIds {
            import_id: uuid_v4(),
            context_id: format!("shared-context-{}", uuid_v4()),
            session_id: format!("share-import-{}", uuid_v4()),
            share_id: share_id.clone(),
            share_url: format!(
                "{}/{}",
                self.share_web_url,
                encode_uri_component(&input.share_code)
            ),
        };
        let workspace_key = workspace_key_of(Some(&workspace_path), workspace_identity.as_deref());
        tokio::fs::create_dir_all(&share_root).await?;
        if let Some(reused) = self
            .reuse_existing(
                &marker_path,
                &import_root,
                input,
                &workspace_path,
                workspace_identity.as_deref(),
            )
            .await
        {
            return Ok(reused);
        }
        if let Err(error) = tokio::fs::create_dir(&import_root).await {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                return Err(ServiceError::new(
                    "unknown",
                    "Conversation share import is already in progress",
                )
                .into());
            }
            return Err(error.into());
        }
        let mut cleanup_armed = true;
        if let Err(error) = tokio::fs::write(
            &marker_path,
            self.marker(input, &ids, &workspace_key, "preparing"),
        )
        .await
        {
            let _ = tokio::fs::remove_dir_all(&import_root).await;
            return Err(error.into());
        }
        let result = self
            .install(
                input,
                operation_id,
                &continuation,
                &ids,
                &workspace_key,
                &workspace_path,
                workspace_identity.as_deref(),
                (&marker_path, &staging, &final_artifacts, &conversation_path),
                &share_title,
                &mut cleanup_armed,
            )
            .await;
        match result {
            Ok(session_id) => {
                let mut out = Map::new();
                out.insert("workspacePath".into(), json!(workspace_path));
                if let Some(identity) = &workspace_identity {
                    out.insert("workspaceIdentity".into(), json!(identity));
                }
                out.insert("sessionId".into(), json!(session_id));
                out.insert("contextId".into(), json!(ids.context_id));
                out.insert("shareUrl".into(), json!(ids.share_url));
                out.insert("title".into(), json!(share_title));
                out.insert("reused".into(), json!(false));
                if remote {
                    out.insert("fallbackReason".into(), json!("remote_workspace"));
                } else if input
                    .target_workspace_path
                    .as_deref()
                    .is_none_or(str::is_empty)
                {
                    out.insert("fallbackReason".into(), json!("default_workspace"));
                }
                Ok(Value::Object(out))
            }
            Err(error) => {
                // 失败清理只能触及本次 import-owned 子目录。
                if cleanup_armed {
                    let _ = tokio::fs::remove_dir_all(&import_root).await;
                }
                Err(error)
            }
        }
    }

    /// 已有同 share 的 marker 且会话仍在：直接复用；会话没了则删掉旧目录重来。
    async fn reuse_existing(
        &self,
        marker_path: &Path,
        import_root: &Path,
        input: &ImportInput,
        workspace_path: &str,
        workspace_identity: Option<&str>,
    ) -> Option<Value> {
        let text = tokio::fs::read_to_string(marker_path).await.ok()?;
        let marker: Value = serde_json::from_str(&text).ok()?;
        let session_id = marker["sessionId"]
            .as_str()
            .filter(|id| id.starts_with("share-import-"))?;
        if marker["shareCode"].as_str() != Some(input.share_code.as_str()) {
            return None;
        }
        let sessions = self.list_sessions(workspace_path).await.ok()?;
        let existing = sessions
            .iter()
            .find(|session| session["sessionId"] == session_id);
        if let (Some(session), Some(context), Some(url)) = (
            existing,
            marker["contextId"].as_str(),
            marker["shareUrl"].as_str(),
        ) {
            let mut out = Map::new();
            out.insert("workspacePath".into(), json!(workspace_path));
            if let Some(identity) = workspace_identity {
                out.insert("workspaceIdentity".into(), json!(identity));
            }
            out.insert("sessionId".into(), session["sessionId"].clone());
            out.insert("contextId".into(), json!(context));
            out.insert("shareUrl".into(), json!(url));
            out.insert("title".into(), session["title"].clone());
            out.insert("reused".into(), json!(true));
            return Some(Value::Object(out));
        }
        let _ = tokio::fs::remove_dir_all(import_root).await;
        None
    }
}

struct ImportIds {
    import_id: String,
    context_id: String,
    session_id: String,
    share_id: String,
    share_url: String,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

fn artifact_issue(artifact: &Value, code: &str, actual: Option<u64>, limit: Option<u64>) -> Value {
    let mut issue = json!({
        "code": code,
        "scope": "artifact",
        "artifactDisplayName": artifact["display_name"],
        "artifactType": artifact["artifact_type"],
        "extension": artifact["extension"],
        "mimeType": artifact["mime_type"],
        "phase": "downloading",
    });
    if let Some(actual) = actual {
        issue["actual"] = json!(actual);
    }
    if let Some(limit) = limit {
        issue["limit"] = json!(limit);
    }
    issue
}

impl Importer {
    /// `downloadArtifactBytes`：单请求超时（含读 body）与 Content-Length 预检。
    async fn download(&self, artifact: &Value) -> Result<(Vec<u8>, Option<String>), ServiceError> {
        let size = artifact["size_bytes"].as_u64().unwrap_or(0);
        let url = artifact["download_url"].as_str().unwrap_or_default();
        let timed_out = || {
            ServiceError::new("network", "Conversation artifact download timed out")
                .issues(vec![artifact_issue(artifact, "unknown", None, None)])
        };
        let work = async {
            let response = self
                .client
                .download(url, DOWNLOAD_TIMEOUT)
                .await
                .map_err(|_| timed_out())?;
            if !response.status().is_success() {
                return Err(
                    ServiceError::new("network", "Conversation artifact download failed")
                        .issues(vec![artifact_issue(artifact, "unknown", None, None)]),
                );
            }
            if let Some(declared) = response.content_length()
                && declared > size
            {
                return Err(ServiceError::new(
                    "invalid_contract",
                    "Conversation artifact integrity check failed",
                )
                .issues(vec![artifact_issue(
                    artifact,
                    "artifact_changed",
                    Some(declared),
                    Some(size),
                )]));
            }
            let mime = response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(';').next())
                .map(|value| value.trim().to_owned());
            let bytes = response.bytes().await.map_err(|_| timed_out())?;
            Ok((bytes.to_vec(), mime))
        };
        tokio::time::timeout(DOWNLOAD_TIMEOUT, work)
            .await
            .map_err(|_| timed_out())?
    }

    #[allow(clippy::too_many_arguments)]
    async fn install(
        &self,
        input: &ImportInput,
        operation_id: &str,
        continuation: &Value,
        ids: &ImportIds,
        workspace_key: &str,
        workspace_path: &str,
        workspace_identity: Option<&str>,
        paths: (&Path, &Path, &Path, &Path),
        share_title: &str,
        cleanup_armed: &mut bool,
    ) -> Result<String, ShareError> {
        let (marker_path, staging, final_artifacts, conversation_path) = paths;
        tokio::fs::create_dir(staging).await?;
        let artifacts = continuation["artifacts"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let total = artifacts.len();
        let mut installed: Vec<Value> = Vec::new();
        let mut used = HashSet::new();
        self.report(operation_id, "downloading", 0, total);
        tokio::fs::write(
            marker_path,
            self.marker(input, ids, workspace_key, "downloading"),
        )
        .await?;
        let share_segment = sanitize_file_segment(&ids.share_id);
        for artifact in &artifacts {
            let (bytes, mime) = self.download(artifact).await?;
            let sha = sha256_hex(&bytes);
            if bytes.len() as u64 != artifact["size_bytes"].as_u64().unwrap_or(u64::MAX)
                || Some(sha.as_str()) != artifact["sha256"].as_str()
            {
                return Err(ServiceError::new(
                    "invalid_contract",
                    "Conversation artifact integrity check failed",
                )
                .issues(vec![artifact_issue(
                    artifact,
                    "artifact_changed",
                    Some(bytes.len() as u64),
                    artifact["size_bytes"].as_u64(),
                )])
                .into());
            }
            let expected_mime = artifact["mime_type"].as_str().unwrap_or_default();
            if let Some(mime) = mime.filter(|mime| !mime.eq_ignore_ascii_case(expected_mime)) {
                log::info(format!(
                    "conversation share artifact content-type differs expected={expected_mime} response={mime}"
                ));
            }
            let file_name = unique_file_name(
                artifact["display_name"].as_str().unwrap_or_default(),
                artifact["artifact_id"].as_str().unwrap_or_default(),
                &mut used,
            );
            tokio::fs::write(staging.join(&file_name), &bytes).await?;
            installed.push(json!({
                "artifactId": artifact["artifact_id"],
                "workspaceRelativePath": format!(".zcode-share/{share_segment}/shared-artifacts/{file_name}"),
                "displayName": artifact["display_name"],
                "mimeType": artifact["mime_type"],
                "sha256": artifact["sha256"],
            }));
            self.report(operation_id, "downloading", installed.len(), total);
        }
        self.report(operation_id, "installing", installed.len(), total);
        tokio::fs::write(
            marker_path,
            self.marker(input, ids, workspace_key, "installing"),
        )
        .await?;
        tokio::fs::rename(staging, final_artifacts).await?;
        let copy_artifacts: Vec<Value> = artifacts
            .iter()
            .map(|artifact| {
                let mut entry = Map::new();
                entry.insert("artifactId".into(), artifact["artifact_id"].clone());
                entry.insert("displayName".into(), artifact["display_name"].clone());
                entry.insert("mimeType".into(), artifact["mime_type"].clone());
                if let Some(path) = installed
                    .iter()
                    .find(|item| item["artifactId"] == artifact["artifact_id"])
                    .map(|item| item["workspaceRelativePath"].clone())
                {
                    entry.insert("workspaceRelativePath".into(), path);
                }
                Value::Object(entry)
            })
            .collect();
        tokio::fs::write(
            conversation_path,
            json!({
                "formatVersion": FORMAT_VERSION,
                "shareId": ids.share_id,
                "contextId": ids.context_id,
                "title": share_title,
                "rows": continuation["rawRows"],
                "artifacts": copy_artifacts,
            })
            .to_string(),
        )
        .await?;
        let rows = continuation["rows"].as_array().cloned().unwrap_or_default();
        let (markdown, markdown_sha, unsupported) =
            format_shared_context(share_title, &rows, &installed)?;
        if !unsupported.is_empty() {
            log::info(format!(
                "shared context skipped row kinds this build cannot format kinds={unsupported:?}"
            ));
        }
        self.report(operation_id, "committing", installed.len(), total);
        tokio::fs::write(
            marker_path,
            self.marker(input, ids, workspace_key, "committing"),
        )
        .await?;
        let target = WorkspaceTarget {
            workspace_path: workspace_path.to_owned(),
            workspace_identity: workspace_identity.map(str::to_owned),
            remote_session_id: None,
        };
        let process = self.agent.get_ready_client(&target).await?;
        self.agent
            .before_use(&process, UseReason::CreateSession)
            .await?;
        let mut params = Map::new();
        params.insert("sessionId".into(), json!(ids.session_id));
        params.insert("workspace".into(), target.workspace_ref());
        params.insert("persistence".into(), json!("immediate"));
        params.insert(
            "importedHistory".into(),
            json!({
                "source": "sharedContext",
                "title": format!("{}{}", title_prefix(input.locale.as_deref()), share_title.trim()),
                "markdown": markdown,
                "provenance": {
                    "shareId": ids.share_id,
                    "contextId": ids.context_id,
                    "shareUrl": ids.share_url,
                    "status": "pending",
                    "projectionSha256": continuation["integrity"]["projection_sha256"],
                    "artifactSetSha256": continuation["integrity"]["artifact_set_sha256"],
                    "formatterVersion": 1,
                    "markdownSha256": markdown_sha,
                    "installedArtifacts": installed.iter().map(|artifact| json!({
                        "artifactId": artifact["artifactId"],
                        "workspaceRelativePath": artifact["workspaceRelativePath"],
                    })).collect::<Vec<_>>(),
                },
            }),
        );
        if crate::off_peak::dispatch::tool_supported(&target) {
            params.insert("offPeakToolEnabled".into(), json!(true));
        }
        let snapshot = process
            .request_value("session/create", &Value::Object(params))
            .await?;
        // 会话已提交且引用了已安装的 artifacts：此后任何失败都不能再删 importRoot。
        *cleanup_armed = false;
        let _ = tokio::fs::remove_file(marker_path).await;
        self.report(operation_id, "complete", installed.len(), total);
        Ok(snapshot["session"]["sessionId"]
            .as_str()
            .unwrap_or(&ids.session_id)
            .to_owned())
    }

    /// `getImportedConversation`：扫描 `.zcode-share/*/shared-conversation.json` 找 contextId。
    pub async fn imported_conversation(&self, workspace_path: &str, context_id: &str) -> Value {
        let share_root = Path::new(workspace_path).join(".zcode-share");
        let Ok(mut entries) = tokio::fs::read_dir(&share_root).await else {
            return Value::Null;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            if !entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let Ok(text) =
                tokio::fs::read_to_string(entry.path().join("shared-conversation.json")).await
            else {
                continue;
            };
            let Ok(parsed) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let valid = parsed["formatVersion"]
                .as_i64()
                .is_some_and(|version| version > 0)
                && parsed["shareId"]
                    .as_str()
                    .is_some_and(|id| !id.trim().is_empty())
                && parsed["contextId"]
                    .as_str()
                    .is_some_and(|id| !id.trim().is_empty())
                && parsed["title"].is_string()
                && parsed["rows"].is_array()
                && parsed["artifacts"].as_array().is_some_and(|artifacts| {
                    artifacts.iter().all(|artifact| {
                        artifact["artifactId"]
                            .as_str()
                            .is_some_and(|id| !id.trim().is_empty())
                            && artifact["displayName"].is_string()
                    })
                });
            if !valid || parsed["contextId"].as_str().map(str::trim) != Some(context_id) {
                continue;
            }
            let (rows, unsupported, _) = decode_rows(
                parsed["rows"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            );
            let newer = i64::from(parsed["formatVersion"].as_i64().unwrap_or(0) > FORMAT_VERSION);
            let artifacts: Vec<Value> = parsed["artifacts"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|artifact| {
                    let mut entry = Map::new();
                    entry.insert(
                        "artifactId".into(),
                        json!(artifact["artifactId"].as_str().unwrap_or_default().trim()),
                    );
                    entry.insert("displayName".into(), artifact["displayName"].clone());
                    for key in ["mimeType", "workspaceRelativePath"] {
                        if let Some(value) = artifact[key].as_str() {
                            entry.insert(key.into(), json!(value));
                        }
                    }
                    Value::Object(entry)
                })
                .collect();
            return json!({
                "shareId": parsed["shareId"].as_str().unwrap_or_default().trim(),
                "contextId": parsed["contextId"].as_str().unwrap_or_default().trim(),
                "title": parsed["title"],
                "rows": rows,
                "artifacts": artifacts,
                "unsupportedRowCount": unsupported as i64 + newer,
            });
        }
        Value::Null
    }

    /// `cleanupAbandonedImports`：只扫默认 conversation workspace；无法证明会话状态时保留。
    pub async fn cleanup_abandoned(&self) {
        self.ensure_loaded().await;
        let share_root = self.conversation_root.join(".zcode-share");
        let Ok(mut entries) = tokio::fs::read_dir(&share_root).await else {
            return;
        };
        let root = self.conversation_root.to_string_lossy().into_owned();
        while let Ok(Some(entry)) = entries.next_entry().await {
            if !entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let import_root = entry.path();
            let marker_path = import_root.join(".zcode-share-import.json");
            let Ok(text) = tokio::fs::read_to_string(&marker_path).await else {
                continue;
            };
            let Ok(marker) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let Some(session_id) = marker["sessionId"]
                .as_str()
                .filter(|id| id.starts_with("share-import-"))
            else {
                continue;
            };
            let Ok(sessions) = self.list_sessions(&root).await else {
                continue;
            };
            match sessions
                .iter()
                .find(|session| session["sessionId"] == session_id)
            {
                Some(session) => {
                    if let (Some(code), Some(request), Some(context), Some(url)) = (
                        marker["shareCode"].as_str(),
                        marker["clientRequestId"].as_str(),
                        marker["contextId"].as_str(),
                        marker["shareUrl"].as_str(),
                    ) {
                        let workspace_key =
                            marker["workspaceKey"].as_str().unwrap_or(&root).to_owned();
                        let keyed = dedupe_key(code, &workspace_key);
                        let result = json!({
                            "workspacePath": root,
                            "sessionId": session["sessionId"],
                            "contextId": context,
                            "shareUrl": url,
                            "title": session["title"],
                            "reused": true,
                        });
                        {
                            let mut completed = self.lock();
                            completed
                                .0
                                .insert(format!("{keyed}\u{0}{request}"), result.clone());
                            completed.1.insert(keyed, result);
                        }
                        let _ = self.persist_index().await;
                    }
                    let _ = tokio::fs::remove_file(&marker_path).await;
                }
                None => {
                    let _ = tokio::fs::remove_dir_all(&import_root).await;
                }
            }
        }
    }
}
