//! 侧栏任务列表：`zcode-task` 的列表成员、index syncer 与 `window-controller` 投影
//! （docs/specs/rust-host-r2.md 第 6 节）。
//!
//! 数据所有者：tasks-index.sqlite 的行（membership、未读、标题覆盖）归 Host；会话的实时活性归 CLI
//! （经 sessions-index）；window-controller 只是两者在内存里的投影，不持久化任何东西。
pub mod controller;
pub mod meta;
pub mod repo;
pub mod service;
mod sqlite;
pub mod syncer;

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
}
