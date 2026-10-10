//! `zcode-task` channel 的侧栏成员（zcodeTaskServiceAdapter.ts）。
//!
//! 读：三个持久分区（listTasks / listPinnedTasks / listArchivedTasks）、全局置顶 id、删除墓碑。
//! 写：未读、置顶、归档、删除、改名；每次写入提交后才发 `workspace_task_list_changed`
//! （事件先于提交会让 UI 用旧 membership 重拉）。其余 legacy 成员继续由契约 stub 应答。
use std::sync::Arc;

use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use zcode_host_kit::{Args, EventHub, Overflow, Reply, RpcError, ServiceBuilder};

use super::repo::{RepoFilter, TaskRepo};
use super::{Partitions, SessionSignals, TaskSource, WorkspaceEvent, WorkspaceEvents};
use crate::agent::{AgentRuntime, WorkspaceTarget};

pub const CHANNEL: &str = "zcode-task";

fn repo_error(error: anyhow::Error) -> RpcError {
    RpcError::new("Error", format!("{error:#}"))
}

fn not_found(task_id: &str) -> RpcError {
    RpcError::new("Error", format!("Task not found: {task_id}"))
}

fn invalid(name: &str) -> RpcError {
    RpcError::new("Error", format!("invalid argument {name}: is required"))
        .with_code("fault.invalidArgument")
}

pub struct TaskService {
    repo: Arc<dyn TaskRepo>,
    events: WorkspaceEvents,
    signals: SessionSignals,
    runtime: AgentRuntime,
}

/// 调用参数里的 `{taskId, workspacePath, workspaceIdentity?}`。
fn task_target(params: &Value) -> Result<(WorkspaceTarget, String), RpcError> {
    let target = WorkspaceTarget::from_value(params)?;
    let task_id = params["taskId"]
        .as_str()
        .filter(|task_id| !task_id.trim().is_empty())
        .ok_or_else(|| invalid("taskId"))?
        .to_owned();
    Ok((target, task_id))
}

impl TaskService {
    pub fn new(
        repo: Arc<dyn TaskRepo>,
        events: WorkspaceEvents,
        runtime: AgentRuntime,
    ) -> Arc<Self> {
        Arc::new(Self {
            repo,
            events,
            // 会话终态迁移，订阅者是 TaskLifecycle（同步转成终态 / ready 事件）。落后 1024 条说明它卡住，
            // 结束它的订阅（docs/specs/rust-u2-task-scope.md §6.2）。
            signals: EventHub::new("task-session-signals", 1024, Overflow::CloseSubscriber),
            runtime,
        })
    }

    pub fn repo(&self) -> &Arc<dyn TaskRepo> {
        &self.repo
    }

    pub fn events(&self) -> &WorkspaceEvents {
        &self.events
    }

    /// 会话终态迁移（syncer 发出，lifecycle 消费）。
    pub fn signals(&self) -> &SessionSignals {
        &self.signals
    }

    /// `workspace_task_list_changed`（zcodeTaskIndexSyncer.ts emitWorkspaceTaskListChanged）。
    pub fn emit_list_changed(
        &self,
        target: &WorkspaceTarget,
        task_id: &str,
        reason: &str,
        meta: Option<&Value>,
        unread_signal: Option<&str>,
    ) {
        let mut event = Map::new();
        event.insert("type".into(), json!("workspace_task_list_changed"));
        event.insert("workspacePath".into(), json!(target.workspace_path));
        if let Some(identity) = target
            .workspace_identity
            .as_deref()
            .map(str::trim)
            .filter(|identity| !identity.is_empty())
        {
            event.insert("workspaceIdentity".into(), json!(identity));
        }
        event.insert("taskId".into(), json!(task_id));
        event.insert("reason".into(), json!(reason));
        if let Some(meta) = meta {
            event.insert("taskMeta".into(), meta.clone());
        }
        if let Some(signal) = unread_signal {
            event.insert("unreadSignal".into(), json!(signal));
        }
        self.events.emit(Arc::new(WorkspaceEvent {
            workspace_key: target.key(),
            event: Value::Object(event),
        }));
    }

    async fn list(
        &self,
        target: &WorkspaceTarget,
        pinned: Option<bool>,
        archived: Option<bool>,
    ) -> Result<Vec<Value>, RpcError> {
        self.repo
            .list(RepoFilter {
                workspace_key: Some(target.key()),
                pinned,
                archived,
            })
            .await
            .map_err(repo_error)
    }

    pub async fn set_unread(
        &self,
        target: &WorkspaceTarget,
        task_id: &str,
        unread: bool,
        expected_unread_at: Option<i64>,
    ) -> Result<Value, RpcError> {
        let outcome = self
            .repo
            .set_unread(target.key(), task_id.to_owned(), unread, expected_unread_at)
            .await
            .map_err(repo_error)?;
        let meta = outcome.meta.ok_or_else(|| not_found(task_id))?;
        // CAS 清除只在真的清掉时广播；无条件写入照常广播。
        if outcome.changed || expected_unread_at.is_none() {
            self.emit_list_changed(target, task_id, "task_meta_changed", Some(&meta), None);
        }
        Ok(meta)
    }

    async fn membership_write(
        &self,
        target: &WorkspaceTarget,
        task_id: &str,
        reason: &str,
        write: impl std::future::Future<Output = anyhow::Result<Option<Value>>>,
    ) -> Result<Value, RpcError> {
        let meta = write
            .await
            .map_err(repo_error)?
            .ok_or_else(|| not_found(task_id))?;
        self.emit_list_changed(target, task_id, reason, Some(&meta), None);
        Ok(meta)
    }

    pub async fn set_pinned(
        &self,
        target: &WorkspaceTarget,
        task_id: &str,
        pinned: bool,
    ) -> Result<Value, RpcError> {
        let reason = if pinned {
            "task_pinned"
        } else {
            "task_unpinned"
        };
        let write = self
            .repo
            .set_pinned(target.key(), task_id.to_owned(), pinned);
        self.membership_write(target, task_id, reason, write).await
    }

    pub async fn set_archived(
        &self,
        target: &WorkspaceTarget,
        task_id: &str,
        archived: bool,
    ) -> Result<Value, RpcError> {
        let reason = if archived {
            "task_archived"
        } else {
            "task_unarchived"
        };
        let write = self
            .repo
            .set_archived(target.key(), task_id.to_owned(), archived);
        self.membership_write(target, task_id, reason, write).await
    }

    pub async fn delete(&self, target: &WorkspaceTarget, task_id: &str) -> Result<Value, RpcError> {
        let write = self.repo.mark_deleted(target.key(), task_id.to_owned());
        self.membership_write(target, task_id, "task_deleted", write)
            .await
    }

    pub async fn rename(
        &self,
        target: &WorkspaceTarget,
        task_id: &str,
        title: String,
    ) -> Result<Value, RpcError> {
        let write = self.repo.rename(target.key(), task_id.to_owned(), title);
        self.membership_write(target, task_id, "task_title_changed", write)
            .await
    }
}

impl TaskSource for Arc<TaskService> {
    fn partitions(
        &self,
        target: WorkspaceTarget,
    ) -> BoxFuture<'static, Result<Partitions, RpcError>> {
        let service = self.clone();
        Box::pin(async move {
            Ok(Partitions {
                active: service.list(&target, Some(false), Some(false)).await?,
                pinned: service.list(&target, Some(true), Some(false)).await?,
                archived: service.list(&target, None, Some(true)).await?,
            })
        })
    }

    fn mutate(
        &self,
        target: WorkspaceTarget,
        task_id: String,
        mutation: Value,
    ) -> BoxFuture<'static, Result<(), RpcError>> {
        let service = self.clone();
        Box::pin(async move {
            match mutation["kind"].as_str() {
                Some("pin") => {
                    service
                        .set_pinned(&target, &task_id, mutation["pinned"] == true)
                        .await?;
                }
                Some("archive") => {
                    service
                        .set_archived(&target, &task_id, mutation["archived"] == true)
                        .await?;
                }
                Some("delete") => {
                    service.delete(&target, &task_id).await?;
                }
                Some("mark-read") => {
                    let expected = mutation["expectedUnreadAt"].as_i64();
                    service
                        .set_unread(&target, &task_id, false, expected)
                        .await?;
                }
                Some("mark-unread") => {
                    service.set_unread(&target, &task_id, true, None).await?;
                }
                Some("open" | "resume") => {}
                other => {
                    return Err(RpcError::new(
                        "Error",
                        format!("unsupported task mutation {}", other.unwrap_or("<missing>")),
                    ));
                }
            }
            Ok(())
        })
    }
}

fn first(args: &Args) -> Value {
    args.raw(0).cloned().unwrap_or(Value::Null)
}

/// `onDynamicWorkspaceEvent` 的参数：workspacePath 字符串，或 `{workspacePath, workspaceIdentity?}`。
fn event_key(arg: &Value) -> Result<String, RpcError> {
    match arg {
        Value::String(path) if !path.trim().is_empty() => Ok(path.clone()),
        other => WorkspaceTarget::from_value(other).map(|target| target.key()),
    }
}

pub fn service(task: Arc<TaskService>) -> ServiceBuilder {
    let (list, pinned_ids, pinned, archived, deleted) = (
        task.clone(),
        task.clone(),
        task.clone(),
        task.clone(),
        task.clone(),
    );
    let (unread, pin, archive, unarchive, delete, rename, restart, events) = (
        task.clone(),
        task.clone(),
        task.clone(),
        task.clone(),
        task.clone(),
        task.clone(),
        task.clone(),
        task,
    );
    ServiceBuilder::new(CHANNEL)
        .method("listTasks", move |_, args| {
            let task = list.clone();
            async move {
                let target = WorkspaceTarget::from_value(&first(&args))?;
                Ok(Reply::Value(Value::Array(
                    task.list(&target, Some(false), Some(false)).await?,
                )))
            }
        })
        .method("listPinnedTaskIds", move |_, _| {
            let task = pinned_ids.clone();
            async move {
                let rows = task
                    .repo
                    .list(RepoFilter {
                        workspace_key: None,
                        pinned: Some(true),
                        archived: Some(false),
                    })
                    .await
                    .map_err(repo_error)?;
                let ids: Vec<Value> = rows.into_iter().map(|row| row["taskId"].clone()).collect();
                Ok(Reply::Value(Value::Array(ids)))
            }
        })
        .method("listPinnedTasks", move |_, args| {
            let task = pinned.clone();
            async move {
                let target = WorkspaceTarget::from_value(&first(&args))?;
                Ok(Reply::Value(Value::Array(
                    task.list(&target, Some(true), Some(false)).await?,
                )))
            }
        })
        .method("listArchivedTasks", move |_, args| {
            let task = archived.clone();
            async move {
                let target = WorkspaceTarget::from_value(&first(&args))?;
                Ok(Reply::Value(Value::Array(
                    task.list(&target, None, Some(true)).await?,
                )))
            }
        })
        .method("listDeletedTaskIds", move |_, args| {
            let task = deleted.clone();
            async move {
                let target = WorkspaceTarget::from_value(&first(&args))?;
                let ids = task
                    .repo
                    .deleted_ids(target.key())
                    .await
                    .map_err(repo_error)?;
                Ok(Reply::Value(json!(ids)))
            }
        })
        .method("setTaskUnread", move |_, args| {
            let task = unread.clone();
            async move {
                let params = first(&args);
                let (target, task_id) = task_target(&params)?;
                let unread = params["unread"]
                    .as_bool()
                    .ok_or_else(|| invalid("unread"))?;
                let expected = params["expectedUnreadAt"].as_i64();
                task.set_unread(&target, &task_id, unread, expected)
                    .await
                    .map(Reply::Value)
            }
        })
        .method("setTaskPinned", move |_, args| {
            let task = pin.clone();
            async move {
                let params = first(&args);
                let (target, task_id) = task_target(&params)?;
                let pinned = params["pinned"]
                    .as_bool()
                    .ok_or_else(|| invalid("pinned"))?;
                task.set_pinned(&target, &task_id, pinned)
                    .await
                    .map(Reply::Value)
            }
        })
        .method("archiveTask", move |_, args| {
            let task = archive.clone();
            async move {
                let (target, task_id) = task_target(&first(&args))?;
                task.set_archived(&target, &task_id, true)
                    .await
                    .map(Reply::Value)
            }
        })
        .method("unarchiveTask", move |_, args| {
            let task = unarchive.clone();
            async move {
                let (target, task_id) = task_target(&first(&args))?;
                task.set_archived(&target, &task_id, false)
                    .await
                    .map(Reply::Value)
            }
        })
        .method("deleteTask", move |_, args| {
            let task = delete.clone();
            async move {
                let (target, task_id) = task_target(&first(&args))?;
                task.delete(&target, &task_id)
                    .await
                    .map(|_| Reply::Undefined)
            }
        })
        .method("renameTask", move |_, args| {
            let task = rename.clone();
            async move {
                let params = first(&args);
                let (target, task_id) = task_target(&params)?;
                let title = params["title"]
                    .as_str()
                    .map(str::trim)
                    .filter(|title| !title.is_empty())
                    .ok_or_else(|| invalid("title"))?
                    .to_owned();
                task.rename(&target, &task_id, title)
                    .await
                    .map(Reply::Value)
            }
        })
        // 预热只是启动 CLI；Rust Host 不为切走的 tab 回收进程，与 Node 对非空闲进程的处理一致。
        .method("releaseWorkspacePreparation", |_, _| async {
            Ok(Reply::Undefined)
        })
        .method("restartWorkspaceProcess", move |_, args| {
            let task = restart.clone();
            async move {
                let target = WorkspaceTarget::from_value(&first(&args))?;
                task.runtime.dispose_workspace(&target).await;
                Ok(Reply::Undefined)
            }
        })
        .event("onDynamicWorkspaceEvent", move |_, arg| {
            let key = event_key(&arg)?;
            let stream = events
                .events
                .subscribe_filtered(move |event| event.workspace_key == key)
                .map(|event| Reply::Value(event.event.clone()));
            Ok(Box::pin(stream))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::repo::memory::MemoryRepo;

    fn runtime() -> AgentRuntime {
        AgentRuntime::new(
            crate::agent::AgentRuntimeConfig {
                base_env: Default::default(),
                command_vars: Default::default(),
                fallback_cwd: None,
            },
            crate::settings::SettingStore::new(
                &zcode_host_base::testing::spawner(),
                std::path::Path::new("/nonexistent"),
            ),
            crate::providers::ProviderRuntime::new(
                &zcode_host_base::testing::spawner(),
                None,
                "/nonexistent/p.json".into(),
            ),
        )
    }

    #[tokio::test]
    async fn partitions_and_mutations_emit_after_write() {
        let repo = Arc::new(MemoryRepo::default());
        repo.seed_if_missing(vec![
            json!({"taskId": "a", "workspacePath": "/w", "title": "A"}),
            json!({"taskId": "b", "workspacePath": "/w", "title": "B"}),
            json!({"taskId": "c", "workspacePath": "/other", "title": "C"}),
        ])
        .await
        .unwrap();
        let events = crate::tasks::workspace_events();
        let mut seen = Box::pin(events.subscribe_filtered(|event| event.workspace_key == "/w"));
        let task = TaskService::new(repo.clone(), events, runtime());
        let target = WorkspaceTarget::local("/w");

        task.set_pinned(&target, "b", true).await.unwrap();
        let event = seen.next().await.unwrap();
        assert_eq!(event.event["reason"], "task_pinned");
        assert_eq!(event.event["taskMeta"]["taskId"], "b");

        let partitions = task.partitions(target.clone()).await.unwrap();
        assert_eq!(partitions.active.len(), 1);
        assert_eq!(partitions.pinned[0]["taskId"], "b");
        assert!(partitions.archived.is_empty());

        task.mutate(
            target.clone(),
            "a".into(),
            json!({"kind": "archive", "archived": true}),
        )
        .await
        .unwrap();
        assert_eq!(seen.next().await.unwrap().event["reason"], "task_archived");
        let missing = task
            .set_unread(&target, "zzz", true, None)
            .await
            .unwrap_err();
        assert_eq!(missing.message, "Task not found: zzz");
        assert_eq!(event_key(&json!("/w")).unwrap(), "/w");
        assert_eq!(
            event_key(&json!({"workspacePath": "/w", "workspaceIdentity": "id"})).unwrap(),
            "id"
        );
    }
}
