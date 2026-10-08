//! `zcode-session` channel（packages/services/src/zcode-session/zcodeSessionService.ts）：
//! zcode-agent 之上的 desktop-continuous 门面（docs/specs/rust-host-agent.md §2.12）。
//!
//! - `readWorkspacePresentation`：每次发送前的门槛（composer 在它成功前不允许提交），透传 CLI
//!   `workspace/readPresentation`；CLI 声明 `workspaceExecutionCapabilities` 时请求附带执行能力。
//! - `initializeWorkspace`：Host 启动预热与 Renderer 预热共用，等同 `zcode-agent.initialize`。
//! - 会话生命周期成员在 agent 结果之上维护草稿表、API 重试运行态，并把快照写进 tasks-index
//!   （提交后再广播 `workspace_task_list_changed`）。
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};
use zcode_host_kit::{Reply, RpcError, ServiceBuilder, log};

use super::api::{AgentApi, format_model};
use super::extensions::UseReason;
use super::runtime::{AgentRuntime, RuntimePolicy, WorkspaceTarget};
use super::service::uuid_v4;
use crate::tasks::meta;
use crate::tasks::repo::RepoFilter;
use crate::tasks::service::TaskService;

pub const CHANNEL: &str = "zcode-session";

fn is_closed_transport(error: &RpcError) -> bool {
    error.message.starts_with("ZCode agent transport closed")
}

pub async fn read_workspace_presentation(
    runtime: &AgentRuntime,
    target: &WorkspaceTarget,
) -> Result<Value, RpcError> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let process = runtime
            .get_client(target, RuntimePolicy::StartIfNeeded)
            .await?;
        runtime
            .before_use(&process, UseReason::ReadPresentation)
            .await?;
        let mut params = Map::new();
        params.insert("workspace".into(), target.workspace_ref());
        if process
            .capabilities()
            .await
            .is_some_and(|capabilities| capabilities["workspaceExecutionCapabilities"] == true)
        {
            params.insert("includeExecutionCapabilities".into(), json!(true));
        }
        match process
            .request_value("workspace/readPresentation", &Value::Object(params))
            .await
        {
            // 进程恰好在两次请求之间退出：换一代重试一次，与 Node 的 stale client 重试相同。
            Err(error) if attempt == 1 && is_closed_transport(&error) => continue,
            Err(error) => {
                log::warn(format!(
                    "readWorkspacePresentation failed for {}: {}",
                    target.key(),
                    error.message
                ));
                return Err(error);
            }
            Ok(presentation) => return Ok(presentation),
        }
    }
}

fn session_key(workspace_key: &str, session_id: &str) -> String {
    format!("{workspace_key}\u{0}{session_id}")
}

fn target_key(params: &Value) -> Result<String, RpcError> {
    let target = WorkspaceTarget::from_value(params)?;
    let session_id = params["sessionId"].as_str().unwrap_or_default();
    Ok(session_key(&target.key(), session_id))
}

/// 快照所属会话的 key（`snapshot.session.workspace`）。
fn snapshot_key(snapshot: &Value, fallback_identity: Option<&str>) -> Option<String> {
    let session = &snapshot["session"];
    let session_id = session["sessionId"].as_str()?;
    let workspace = &session["workspace"];
    let path = workspace["workspacePath"].as_str()?;
    let identity = workspace["workspaceIdentity"]
        .as_str()
        .or(fallback_identity)
        .map(str::trim)
        .filter(|identity| !identity.is_empty());
    Some(session_key(identity.unwrap_or(path), session_id))
}

#[derive(Default)]
struct State {
    /// 未发送前的草稿会话：不进 tasks-index。
    drafts: HashSet<String>,
    /// 最近一次已知的 `runtime.apiRetry`（null 表示已清除）。
    api_retry: HashMap<String, Value>,
}

pub struct SessionService {
    api: AgentApi,
    tasks: Option<Arc<TaskService>>,
    state: Mutex<State>,
}

/// appendWorkspaceToFilesystemMcpServers：用户目录里的 filesystem MCP 只允许固定目录，
/// 本机 workspace 存在时非持久化追加当前 workspace（远程 workspace 不注入本机路径）。
pub async fn append_workspace_to_filesystem_mcp(
    servers: &Value,
    workspace_path: &str,
) -> Option<Value> {
    let list = servers.as_array().filter(|list| !list.is_empty())?;
    let workspace = workspace_path.trim();
    if workspace.is_empty() || tokio::fs::metadata(workspace).await.is_err() {
        return None;
    }
    let normalize = |value: &str| {
        let mut normalized = std::path::PathBuf::new();
        for component in std::path::Path::new(value.trim()).components() {
            match component {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    normalized.pop();
                }
                other => normalized.push(other.as_os_str()),
            }
        }
        let text = normalized.display().to_string();
        let text = text.trim_end_matches(['/', '\\']).to_owned();
        if cfg!(windows) {
            text.to_lowercase()
        } else {
            text
        }
    };
    let workspace_key = normalize(workspace);
    let mut changed = false;
    let next: Vec<Value> = list
        .iter()
        .map(|server| {
            let args = server["args"].as_array();
            let is_filesystem = server.get("command").is_some()
                && server["name"] == "filesystem"
                && args.is_some_and(|args| {
                    args.iter().any(|arg| {
                        arg.as_str().is_some_and(|arg| {
                            arg.contains("@modelcontextprotocol/server-filesystem")
                        })
                    })
                });
            let Some(args) = args.filter(|_| is_filesystem) else {
                return server.clone();
            };
            if args.iter().any(|arg| {
                arg.as_str()
                    .is_some_and(|arg| normalize(arg) == workspace_key)
            }) {
                return server.clone();
            }
            changed = true;
            let mut server = server.clone();
            let mut args = args.clone();
            args.push(json!(workspace));
            server["args"] = Value::Array(args);
            server
        })
        .collect();
    changed.then_some(Value::Array(next))
}

impl SessionService {
    pub fn new(api: AgentApi, tasks: Option<Arc<TaskService>>) -> Arc<Self> {
        Arc::new(Self {
            api,
            tasks,
            state: Mutex::new(State::default()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// withApiRetryRuntime。
    fn with_api_retry(&self, mut snapshot: Value) -> Value {
        let Some(key) = snapshot_key(&snapshot, None) else {
            return snapshot;
        };
        let mut state = self.lock();
        if let Some(retry) = snapshot["runtime"].get("apiRetry") {
            state.api_retry.insert(key, retry.clone());
            return snapshot;
        }
        if matches!(
            snapshot["session"]["status"].as_str(),
            Some("completed" | "error")
        ) {
            state.api_retry.insert(key, Value::Null);
            snapshot["runtime"]["apiRetry"] = Value::Null;
            return snapshot;
        }
        if let Some(retry) = state.api_retry.get(&key).cloned() {
            // 切回运行中的 task 时，当前 turn 底部才能继续显示重试提示。
            snapshot["runtime"]["apiRetry"] = retry;
        }
        snapshot
    }

    async fn with_resolved_mcp(&self, params: &Value) -> Value {
        let mut params = params.clone();
        if let Some(path) = params["workspacePath"].as_str()
            && let Some(servers) =
                append_workspace_to_filesystem_mcp(&params["mcpServers"], path).await
        {
            params["mcpServers"] = servers;
        }
        params
    }

    /// syncSnapshotAndBroadcast：写入 tasks-index 后再广播；还没有用户内容的快照只写库。
    async fn sync_and_broadcast(
        &self,
        snapshot: &Value,
        model_override: Option<String>,
        thought_level_override: Option<String>,
        move_to_top: bool,
        reason: &str,
    ) {
        let Some(tasks) = &self.tasks else { return };
        // 旧 child 标签页的显式 resume 仍会同步快照；只读详情不能写成主任务。
        if snapshot["session"]["sessionKind"] == "subagent_child" {
            return;
        }
        let mut task_meta = meta::meta_from_snapshot(snapshot);
        if let Some(model) = model_override.filter(|model| !model.trim().is_empty()) {
            task_meta["model"] = json!(model.trim());
        }
        if let Some(level) = thought_level_override.filter(|level| !level.trim().is_empty()) {
            task_meta["thoughtLevel"] = json!(level.trim());
        }
        let searchable = Some(meta::searchable_text(snapshot));
        let written = if move_to_top {
            tasks
                .repo()
                .sync_at_grouped_top(task_meta, searchable)
                .await
                .map(|outcome| (outcome.meta, outcome.grouped_order_initialized))
        } else {
            tasks
                .repo()
                .sync(task_meta, searchable)
                .await
                .map(|meta| (meta, false))
        };
        let (persisted, initialized) = match written {
            Ok(written) => written,
            Err(error) => {
                log::warn(format!("session snapshot sync failed: {error:#}"));
                return;
            }
        };
        let target = WorkspaceTarget {
            workspace_path: persisted["workspacePath"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            workspace_identity: persisted["workspaceIdentity"].as_str().map(str::to_owned),
            remote_session_id: None,
        };
        let task_id = persisted["taskId"].as_str().unwrap_or_default().to_owned();
        if !meta::has_user_visible_content(snapshot) {
            if initialized {
                tasks.emit_list_changed(&target, &task_id, "task_created", None, None);
            }
            return;
        }
        let reason = if initialized { "task_created" } else { reason };
        tasks.emit_list_changed(&target, &task_id, reason, Some(&persisted), None);
    }

    /// syncTaskModel：只更新已有行的 model。
    async fn sync_task_model(&self, target: &WorkspaceTarget, session_id: &str, model: &str) {
        let Some(tasks) = &self.tasks else { return };
        let rows = match tasks
            .repo()
            .list(RepoFilter {
                workspace_key: Some(target.key()),
                pinned: None,
                archived: None,
            })
            .await
        {
            Ok(rows) => rows,
            Err(error) => {
                log::warn(format!(
                    "sync task model failed for {session_id}: {error:#}"
                ));
                return;
            }
        };
        if let Some(mut row) = rows.into_iter().find(|row| row["taskId"] == session_id) {
            row["model"] = json!(model);
            if let Err(error) = tasks.repo().sync(row, None).await {
                log::warn(format!(
                    "sync task model failed for {session_id}: {error:#}"
                ));
            }
        }
    }

    pub async fn create_session(&self, params: &Value) -> Result<Value, RpcError> {
        let mut params = self.with_resolved_mcp(params).await;
        if !params["sessionTraceId"].is_string() {
            params["sessionTraceId"] = json!(uuid_v4());
        }
        let snapshot = self.api.create_session(&params).await?;
        if params["persistence"] == "deferred" {
            // 草稿只让 toolbar 与 runtime 共用状态；空会话不能进 tasks-index。
            if let Some(key) = snapshot_key(&snapshot, params["workspaceIdentity"].as_str()) {
                self.lock().drafts.insert(key);
            }
            return Ok(snapshot);
        }
        let snapshot = self.with_api_retry(snapshot);
        self.sync_and_broadcast(&snapshot, None, None, true, "task_meta_changed")
            .await;
        Ok(snapshot)
    }

    pub async fn resume_session(&self, params: &Value) -> Result<Value, RpcError> {
        let broadcast = params["broadcastSnapshot"].as_bool().unwrap_or(true);
        let mut params = self.with_resolved_mcp(params).await;
        if let Some(object) = params.as_object_mut() {
            object.remove("broadcastSnapshot");
        }
        let mut snapshot = self.with_api_retry(self.api.resume_session(&params).await?);
        let requested = params["thoughtLevel"]
            .as_str()
            .map(str::trim)
            .filter(|level| !level.is_empty())
            .map(str::to_owned);
        let mut thought_level = requested.clone();
        if let Some(level) = &requested
            && snapshot["settings"]["thoughtLevel"]["current"] != level.as_str()
        {
            if !can_replay_thought_level(&snapshot, level) {
                // 旧 task 的思考强度可能不被当前模型支持（例如 max），不能再重放给 agent。
                log::warn(format!(
                    "resumeSession skips unsupported thoughtLevel {level} for {}",
                    params["sessionId"]
                ));
                thought_level = None;
            } else {
                let mut request = Map::new();
                for key in ["workspacePath", "workspaceIdentity", "sessionId"] {
                    if let Some(value) = params.get(key).filter(|value| !value.is_null()) {
                        request.insert(key.into(), value.clone());
                    }
                }
                request.insert("thoughtLevel".into(), json!(level));
                snapshot =
                    self.with_api_retry(self.api.set_thought_level(&Value::Object(request)).await?);
            }
        }
        let model_override = params
            .get("model")
            .filter(|model| model.is_object())
            .map(format_model)
            .filter(|model| !model.is_empty());
        if broadcast {
            // 打开 / 恢复任务只是快照收敛，不改变 pin/archive/unread 归属。
            self.sync_and_broadcast(
                &snapshot,
                model_override,
                thought_level,
                false,
                "task_status_changed",
            )
            .await;
        } else if let Some(model) = model_override {
            let target = WorkspaceTarget::from_value(&params)?;
            let session_id = params["sessionId"].as_str().unwrap_or_default();
            self.sync_task_model(&target, session_id, &model).await;
        }
        Ok(snapshot)
    }

    pub async fn set_model(&self, params: &Value) -> Result<Value, RpcError> {
        let draft = self.lock().drafts.contains(&target_key(params)?);
        let snapshot = self.with_api_retry(self.api.set_model(params).await?);
        if draft {
            // 草稿只在 runtime 内存里：写库会在重启后留下无法 resume 的脏会话。
            return Ok(snapshot);
        }
        let model = format_model(&params["model"]);
        self.sync_and_broadcast(&snapshot, Some(model), None, false, "task_model_changed")
            .await;
        Ok(snapshot)
    }

    pub async fn close_session(&self, params: &Value) -> Result<(), RpcError> {
        self.lock().drafts.remove(&target_key(params)?);
        let mut request = params.clone();
        if let Some(object) = request.as_object_mut() {
            object.remove("expectedPersistence");
        }
        self.api.close_session(&request).await.map(|_| ())
    }

    pub async fn close_deferred_draft(&self, params: &Value) -> Result<bool, RpcError> {
        let key = target_key(params)?;
        let mut request = params.clone();
        request["expectedPersistence"] = json!("deferred");
        match self.api.close_session(&request).await {
            Ok(closed) => {
                if closed {
                    self.lock().drafts.remove(&key);
                }
                Ok(closed)
            }
            Err(error) => {
                // 旧 Agent 拒绝 expectedPersistence：保留旧会话，不能退回无条件 close。
                log::warn(format!(
                    "conditional close of deferred draft failed, keeping the session: {}",
                    error.message
                ));
                Ok(false)
            }
        }
    }

    pub fn promote_draft(&self, params: &Value) -> Result<(), RpcError> {
        self.lock().drafts.remove(&target_key(params)?);
        Ok(())
    }
}

/// 快照的可用档位包含它，或快照没有档位信息时才重放。
fn can_replay_thought_level(snapshot: &Value, level: &str) -> bool {
    let thought = &snapshot["settings"]["thoughtLevel"];
    if thought["enabled"] == false {
        return false;
    }
    let values: Vec<String> = thought["available"]
        .as_array()
        .map(|options| {
            options
                .iter()
                .filter_map(|option| option.as_str().or_else(|| option["value"].as_str()))
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
                .collect()
        })
        .unwrap_or_default();
    values.is_empty() || values.iter().any(|value| value == level)
}

fn args_value(args: &zcode_host_kit::Args) -> Value {
    args.raw(0).cloned().unwrap_or(Value::Null)
}

pub fn service(sessions: Arc<SessionService>) -> ServiceBuilder {
    let api = sessions.api.clone();
    let mut builder = ServiceBuilder::new(CHANNEL);
    let runtime = api.chat.clone();
    builder = builder.method("readWorkspacePresentation", move |_, args| {
        let runtime = runtime.clone();
        async move {
            let target = WorkspaceTarget::from_value(&args_value(&args))?;
            read_workspace_presentation(&runtime, &target)
                .await
                .map(Reply::Value)
        }
    });
    let runtime = api.chat.clone();
    builder = builder.method("initializeWorkspace", move |_, args| {
        let runtime = runtime.clone();
        async move {
            let target = WorkspaceTarget::from_value(&args_value(&args))?;
            Ok(Reply::Value(runtime.initialize(&target).await))
        }
    });
    macro_rules! member {
        ($name:literal, |$s:ident, $p:ident| $body:expr) => {{
            let sessions = sessions.clone();
            builder = builder.method($name, move |_, args| {
                let $s = sessions.clone();
                async move {
                    let $p = args_value(&args);
                    $body
                }
            });
        }};
    }
    member!("getWorkspaceRuntimeIdentity", |s, p| {
        let target = WorkspaceTarget::from_value(&p)?;
        s.api.runtime_identity(&target).await.map(Reply::Value)
    });
    member!("createSession", |s, p| s
        .create_session(&p)
        .await
        .map(Reply::Value));
    member!("resumeSession", |s, p| s
        .resume_session(&p)
        .await
        .map(Reply::Value));
    member!("listSessions", |s, p| s
        .api
        .list_sessions(&p)
        .await
        .map(Reply::Value));
    member!("readSession", |s, p| {
        let snapshot = s.api.read_session(&p).await?;
        Ok(Reply::Value(s.with_api_retry(snapshot)))
    });
    member!("readSessionMessages", |s, p| s
        .api
        .read_session_messages(&p)
        .await
        .map(Reply::Value));
    member!("readSessionEvents", |s, p| s
        .api
        .read_session_events(&p)
        .await
        .map(Reply::Value));
    member!("promoteDeferredDraftSession", |s, p| s
        .promote_draft(&p)
        .map(|()| Reply::Undefined));
    member!("closeSession", |s, p| s
        .close_session(&p)
        .await
        .map(|()| Reply::Undefined));
    member!("closeDeferredDraftSession", |s, p| s
        .close_deferred_draft(&p)
        .await
        .map(|closed| Reply::Value(json!(closed))));
    member!("setModel", |s, p| s.set_model(&p).await.map(Reply::Value));
    member!("setThoughtLevel", |s, p| {
        let snapshot = s.api.set_thought_level(&p).await?;
        Ok(Reply::Value(s.with_api_retry(snapshot)))
    });
    member!("setMode", |s, p| {
        let snapshot = s.api.set_mode(&p).await?;
        Ok(Reply::Value(s.with_api_retry(snapshot)))
    });
    builder
}

/// Host 启动预热（packages/desktop/src/host/index.ts warmUpZCodeAgent）：让 `existing-only` 的
/// sessions-index 订阅一启动就有进程可连。失败只记日志。
pub fn warm_up(runtime: AgentRuntime, targets: Vec<WorkspaceTarget>) {
    for target in targets {
        let runtime = runtime.clone();
        tokio::spawn(async move {
            let result = runtime.initialize(&target).await;
            let workspace = &target.workspace_path;
            if result["available"] == true {
                log::info(format!("ZCode agent warmup ready workspace={workspace}"));
            } else if result["reasonCode"] == "provider_not_ready" {
                log::info(format!(
                    "ZCode agent warmup waiting for provider/model workspace={workspace}"
                ));
            } else {
                log::warn(format!(
                    "ZCode agent warmup unavailable workspace={workspace} reason={}",
                    result["reason"].as_str().unwrap_or("unknown")
                ));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thought_level_replay_follows_available_levels() {
        let snapshot = |available: Value| json!({"settings": {"thoughtLevel": {"enabled": true, "available": available}}});
        assert!(can_replay_thought_level(&snapshot(json!([])), "max"));
        assert!(can_replay_thought_level(
            &snapshot(json!(["high", {"value": "max"}])),
            "max"
        ));
        assert!(!can_replay_thought_level(&snapshot(json!(["high"])), "max"));
        assert!(!can_replay_thought_level(
            &json!({"settings": {"thoughtLevel": {"enabled": false}}}),
            "high"
        ));
    }

    #[tokio::test]
    async fn api_retry_runtime_is_remembered_and_cleared() {
        let runtime = AgentRuntime::new(
            crate::agent::AgentRuntimeConfig {
                base_env: Default::default(),
                command_vars: Default::default(),
                search_roots: Vec::new(),
                fallback_cwd: None,
            },
            crate::settings::SettingStore::new(std::path::Path::new("/nonexistent")),
            crate::providers::ProviderRuntime::new(None, "/nonexistent/p.json".into()),
        );
        let api = AgentApi::new(
            runtime,
            std::path::Path::new("/nonexistent"),
            Arc::new(super::super::notifications::AgentEvents::default()),
            None,
        );
        let sessions = SessionService::new(api, None);
        let snapshot = |status: &str, retry: Option<Value>| {
            let mut runtime = json!({"eventSeq": 1});
            if let Some(retry) = retry {
                runtime["apiRetry"] = retry;
            }
            json!({"session": {"sessionId": "s", "status": status, "workspace": {"workspacePath": "/w"}}, "runtime": runtime})
        };
        let unknown = sessions.with_api_retry(snapshot("running", None));
        assert!(unknown["runtime"].get("apiRetry").is_none());
        let retry = json!({"attempt": 2});
        sessions.with_api_retry(snapshot("running", Some(retry.clone())));
        assert_eq!(
            sessions.with_api_retry(snapshot("running", None))["runtime"]["apiRetry"],
            retry
        );
        assert_eq!(
            sessions.with_api_retry(snapshot("completed", None))["runtime"]["apiRetry"],
            Value::Null
        );
        assert_eq!(
            sessions.with_api_retry(snapshot("running", None))["runtime"]["apiRetry"],
            Value::Null
        );
    }

    #[tokio::test]
    async fn filesystem_mcp_gets_the_local_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().display().to_string();
        let servers = json!([
            {"name": "filesystem", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]},
            {"name": "other", "command": "x", "args": []},
        ]);
        let next = append_workspace_to_filesystem_mcp(&servers, &path)
            .await
            .unwrap();
        assert_eq!(next[0]["args"][3], json!(path));
        assert_eq!(next[1], servers[1]);
        assert!(
            append_workspace_to_filesystem_mcp(&next, &path)
                .await
                .is_none()
        );
        assert!(
            append_workspace_to_filesystem_mcp(&servers, "/nonexistent/zcode-ws")
                .await
                .is_none()
        );
    }
}
