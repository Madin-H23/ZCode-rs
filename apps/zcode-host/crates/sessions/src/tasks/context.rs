//! zcode-task 全量成员共用的上下文：tasks-index、侧栏 service（广播）、CLI runtime、设置与
//! 任务 target 登记（rust-host-tasks.md 第 3 节）。
//!
//! target 登记对应 Node adapter 的 `taskTargets`（按裸 taskId）：创建、恢复、列表读取都会登记；
//! 没登记时 Rust 再按 taskId 查一次 tasks-index（见第 5 节差异），都没有才报
//! `ZCODE_SESSION_TARGET_NOT_FOUND`。
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use zcode_host_kit::RpcError;

use super::service::TaskService;
use super::{Partitions, TaskSource, WorkspaceEvent};
use crate::agent::{AgentRuntime, WorkspaceTarget};
use crate::settings::SettingStore;
use crate::tasks_index::TasksIndex;

pub const GLM_PROVIDER: &str = "glm";

pub fn repo_error(error: anyhow::Error) -> RpcError {
    RpcError::new("Error", format!("{error:#}"))
}

pub fn invalid(name: &str) -> RpcError {
    RpcError::new("Error", format!("invalid argument {name}: is required"))
        .with_code("fault.invalidArgument")
}

/// meta / 参数里的 `{workspacePath, workspaceIdentity?}`。
pub fn target_of(value: &Value) -> WorkspaceTarget {
    WorkspaceTarget {
        workspace_path: value["workspacePath"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        workspace_identity: value["workspaceIdentity"].as_str().map(str::to_owned),
        remote_session_id: None,
    }
}

/// 读文件类成员用到的路径（Node `os.homedir()`、`getDataBaseDir()`、`ZCODE_LOG_DIR`）。
#[derive(Debug, Clone, Default)]
pub struct TaskPaths {
    pub user_home: std::path::PathBuf,
    pub data_base_dir: std::path::PathBuf,
    pub log_dir: Option<std::path::PathBuf>,
}

pub struct TaskContext {
    pub paths: TaskPaths,
    pub index: TasksIndex,
    pub tasks: Arc<TaskService>,
    pub runtime: AgentRuntime,
    pub settings: SettingStore,
    targets: Mutex<HashMap<String, WorkspaceTarget>>,
}

impl TaskContext {
    pub fn new(
        paths: TaskPaths,
        index: TasksIndex,
        tasks: Arc<TaskService>,
        runtime: AgentRuntime,
        settings: SettingStore,
    ) -> Arc<Self> {
        Arc::new(Self {
            paths,
            index,
            tasks,
            runtime,
            settings,
            targets: Mutex::new(HashMap::new()),
        })
    }

    fn lock_targets(&self) -> std::sync::MutexGuard<'_, HashMap<String, WorkspaceTarget>> {
        self.targets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `rememberTaskTarget` / `rememberIndexedTaskMeta`。
    pub fn remember(&self, meta: &Value) {
        if let Some(task_id) = meta["taskId"].as_str().filter(|id| !id.is_empty()) {
            self.lock_targets()
                .insert(task_id.to_owned(), target_of(meta));
        }
    }

    pub fn remember_target(&self, task_id: &str, target: &WorkspaceTarget) {
        self.lock_targets()
            .insert(task_id.to_owned(), target.clone());
    }

    /// `getTaskTarget(taskId)`。
    pub async fn task_target(&self, task_id: &str) -> Result<WorkspaceTarget, RpcError> {
        if let Some(target) = self.lock_targets().get(task_id).cloned() {
            return Ok(target);
        }
        if let Some(meta) = self
            .index
            .find_task_meta_by_id(task_id.to_owned())
            .await
            .map_err(repo_error)?
        {
            self.remember(&meta);
            return Ok(target_of(&meta));
        }
        Err(RpcError::new(
            "Error",
            format!("ZCode session target is not loaded: {task_id}"),
        )
        .with_code("ZCODE_SESSION_TARGET_NOT_FOUND"))
    }

    /// 发一条 workspace 事件（`onDynamicWorkspaceEvent`）。
    pub fn emit(&self, target: &WorkspaceTarget, event: Value) {
        self.tasks.events().emit(Arc::new(WorkspaceEvent {
            workspace_key: target.key(),
            event,
        }));
    }

    /// `workspace_task_list_changed`；taskId 与 taskMeta 可缺省（分组、批量操作）。
    pub fn emit_list_changed(
        &self,
        target: &WorkspaceTarget,
        task_id: Option<&str>,
        reason: &str,
        meta: Option<&Value>,
    ) {
        match task_id {
            Some(task_id) => self
                .tasks
                .emit_list_changed(target, task_id, reason, meta, None),
            None => {
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
                event.insert("reason".into(), json!(reason));
                if let Some(meta) = meta {
                    event.insert("taskMeta".into(), meta.clone());
                }
                self.emit(target, Value::Object(event));
            }
        }
    }
}

impl TaskSource for Arc<TaskContext> {
    fn partitions(
        &self,
        target: WorkspaceTarget,
    ) -> BoxFuture<'static, Result<Partitions, RpcError>> {
        self.tasks.partitions(target)
    }

    fn mutate(
        &self,
        target: WorkspaceTarget,
        task_id: String,
        mutation: Value,
    ) -> BoxFuture<'static, Result<(), RpcError>> {
        self.tasks.mutate(target, task_id, mutation)
    }

    fn search(&self, query: Value) -> BoxFuture<'static, Result<Value, RpcError>> {
        let context = self.clone();
        Box::pin(async move { super::index_members::list_task_list(&context, query).await })
    }

    fn delete_archived(
        &self,
        target: WorkspaceTarget,
        task_id: String,
    ) -> BoxFuture<'static, Result<bool, RpcError>> {
        let context = self.clone();
        Box::pin(async move {
            super::index_members::delete_archived_task(&context, &target, &task_id).await
        })
    }

    fn delete_archived_batch(
        &self,
        target: WorkspaceTarget,
        task_ids: Vec<String>,
    ) -> BoxFuture<'static, Result<Value, RpcError>> {
        let context = self.clone();
        Box::pin(async move {
            super::index_members::delete_archived_tasks(&context, &target, task_ids).await
        })
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! 测试用上下文：临时目录里的真实 tasks-index、设置文件，不拉起 CLI。
    use super::*;
    use crate::agent::{AgentRuntimeConfig, sessions_index::SessionsIndexObserver};
    use crate::tasks::{WorkspaceEvents, workspace_events};

    pub struct Fixture {
        pub context: Arc<TaskContext>,
        pub observer: Arc<SessionsIndexObserver>,
        pub events: WorkspaceEvents,
        pub home: std::path::PathBuf,
    }

    pub async fn fixture(dir: &std::path::Path) -> Fixture {
        let home = dir.join("home");
        let settings = SettingStore::new(&zcode_host_base::testing::spawner(), &home);
        let runtime = AgentRuntime::new(
            &zcode_host_base::testing::spawner(),
            AgentRuntimeConfig {
                base_env: Default::default(),
                command_vars: Default::default(),
                fallback_cwd: None,
            },
            settings.clone(),
            crate::providers::ProviderRuntime::new(
                &zcode_host_base::testing::spawner(),
                None,
                dir.join("p.json"),
            ),
        );
        let index = TasksIndex::open(dir.join("tasks-index.sqlite"), |_| {})
            .await
            .expect("tasks-index");
        let events = workspace_events();
        let tasks = TaskService::new(Arc::new(index.clone()), events.clone(), runtime.clone());
        let observer = SessionsIndexObserver::start(runtime.clone());
        Fixture {
            context: TaskContext::new(
                TaskPaths {
                    user_home: home.clone(),
                    data_base_dir: home.clone(),
                    log_dir: Some(dir.join("log")),
                },
                index,
                tasks,
                runtime,
                settings,
            ),
            observer,
            events,
            home,
        }
    }

    /// 一条完整、合法的 glm task meta。
    pub fn meta(task_id: &str, workspace: &str, extra: Value) -> Value {
        let mut meta = json!({
            "taskId": task_id,
            "traceId": format!("trace-{task_id}"),
            "title": format!("Title {task_id}"),
            "workspacePath": workspace,
            "createdAt": 100,
            "updatedAt": 200,
            "mode": "build",
            "provider": "glm",
            "status": "completed",
        });
        if let (Some(meta), Some(extra)) = (meta.as_object_mut(), extra.as_object()) {
            meta.extend(extra.clone());
        }
        meta
    }
}
