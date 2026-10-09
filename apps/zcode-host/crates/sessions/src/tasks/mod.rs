//! 侧栏任务列表：`zcode-task` 的列表成员、index syncer 与 `window-controller` 投影
//! （docs/specs/rust-host-r2.md 第 6 节）。
//!
//! 数据所有者：tasks-index.sqlite 的行（membership、未读、标题覆盖）归 Host；会话的实时活性归 CLI
//! （经 sessions-index）；window-controller 只是两者在内存里的投影，不持久化任何东西。
pub mod agent_calls;
mod claude_import;
mod claude_parse;
pub mod config_options;
pub mod context;
pub mod controller;
pub mod index_members;
pub mod lifecycle;
mod lifecycle_members;
pub mod lifecycle_service;
pub mod meta;
mod queries;
pub mod repo;
pub mod service;
pub mod session_messages;
mod sqlite;
pub mod syncer;
pub mod trajectory;

use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::Value;
use zcode_host_kit::{EventHub, RpcError};

use crate::agent::WorkspaceTarget;

/// `zcode-task.onDynamicWorkspaceEvent` 的一条事件，按 workspaceKey 分发。
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceEvent {
    pub workspace_key: String,
    pub event: Value,
}

pub type WorkspaceEvents = EventHub<Arc<WorkspaceEvent>>;

/// sessions-index 观察到的会话终态迁移（zcodeTaskIndexSyncer `emitTerminalAndReady`）。
/// 不是 UI 流，只给 host 内部的 runtime command 队列与 terminal / ready 事件使用。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSignal {
    pub target: WorkspaceTarget,
    pub session_id: String,
    pub failed: bool,
}

pub type SessionSignals = EventHub<Arc<SessionSignal>>;

/// 一个 workspace 的三个持久分区（listTasks、listPinnedTasks、listArchivedTasks），元素是 ZCodeTaskMeta。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Partitions {
    pub active: Vec<Value>,
    pub pinned: Vec<Value>,
    pub archived: Vec<Value>,
}

/// window-controller 读取与修改任务行的入口，由 zcode-task 实现。
pub trait TaskSource: Send + Sync {
    fn partitions(
        &self,
        target: WorkspaceTarget,
    ) -> BoxFuture<'static, Result<Partitions, RpcError>>;
    /// `pin`、`archive`、`delete`、`mark-read`、`mark-unread`；`open`、`resume` 由调用方当作空操作。
    fn mutate(
        &self,
        target: WorkspaceTarget,
        task_id: String,
        mutation: Value,
    ) -> BoxFuture<'static, Result<(), RpcError>>;
    /// 全文搜索：zcode-task `listTaskList`（query 只含该 source 的 scope）。
    fn search(&self, _query: Value) -> BoxFuture<'static, Result<Value, RpcError>> {
        Box::pin(async { Err(RpcError::new("Error", "task search is unavailable")) })
    }
    /// zcode-task `deleteArchivedTask`。
    fn delete_archived(
        &self,
        _target: WorkspaceTarget,
        _task_id: String,
    ) -> BoxFuture<'static, Result<bool, RpcError>> {
        Box::pin(async { Err(RpcError::new("Error", "archived deletion is unavailable")) })
    }
    /// zcode-task `deleteArchivedTasks`。
    fn delete_archived_batch(
        &self,
        _target: WorkspaceTarget,
        _task_ids: Vec<String>,
    ) -> BoxFuture<'static, Result<Value, RpcError>> {
        Box::pin(async { Err(RpcError::new("Error", "archived deletion is unavailable")) })
    }
}
