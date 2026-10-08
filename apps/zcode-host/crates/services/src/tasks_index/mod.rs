//! tasks-index.sqlite（docs/specs/rust-host-r2.md §7）：侧栏任务列表的产品壳状态。
//!
//! 文件固定在 `{dataBaseDir}/.zcode/v2/tasks-index.sqlite`，与 Node Host、其他窗口的 Host
//! 共享，因此 schema、迁移账本、行语义都与 TS 实现（packages/services/src/session/
//! taskIndexRepo.ts、tasksDatabase/）保持一致，Rust 是完全兼容的写者。
//!
//! 线程模型：每个 [`TasksIndex`] 句柄（及其 clone）独占一个专用线程和一条 SQLite 连接，
//! 请求按到达顺序在该线程上串行执行。这覆盖了 TS `enqueueWrite` 的同行串行（且更强：
//! 整个句柄全序）；跨句柄、跨进程的互斥由 SQLite 写锁负责——每个读改写都在
//! `BEGIN IMMEDIATE` 中完成，业务连接 busy_timeout 5000 ms。
//!
//! ```text
//! async 调用方 ──job──► mpsc ──► tasks-index 线程（rusqlite::Connection）
//!      ▲                              │ BEGIN IMMEDIATE … COMMIT
//!      └────────── oneshot 结果 ◄──────┘
//! 其他 Host / Node Host ──────────► 同一文件（WAL；写锁串行）
//! ```
mod legacy_selection;
mod meta;
mod repo;
mod schema;
mod startup;
#[cfg(test)]
mod tests;

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use rusqlite::Connection;
use serde_json::Value;
use tokio::sync::oneshot;
use zcode_host_kit::log;

use meta::{JsObject, as_integer, validate_input_meta};
use repo::{GroupedAdmission, StatePatch};

pub use meta::resolve_workspace_key;
pub use repo::ListFilter;
pub use startup::{
    MigrationFacts, MigrationKind, StartupErrorCode, StoragePhase, StorageProgress, TasksIndexError,
};

/// `syncTaskMetaAtGroupedTop` 的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct SyncOutcome {
    /// 合并后读回的 meta（`ZCodeTaskMeta` JSON）。
    pub meta: Value,
    /// grouped 顶层顺序是否由这次写入首次初始化。
    pub initialized_grouped_order: bool,
}

/// `set_unread` 的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct UnreadOutcome {
    /// 写入后的 meta；行不存在或已删除时为 `None`（TS 抛错，由调用方按需 seed 后重试）。
    pub meta: Option<Value>,
    /// 是否写入了 unread 状态：标记未读与无条件清除总是写入；CAS 清除只在 marker 匹配时写入。
    /// 为 true 时调用方应广播 `task_meta_changed`（同 TS adapter）。
    pub changed: bool,
}

type Job = Box<dyn FnOnce(&mut Connection) + Send>;
type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// tasks-index 的唯一读写入口；clone 共享同一线程与连接。
#[derive(Clone)]
pub struct TasksIndex {
    inner: Arc<Inner>,
}

struct Inner {
    jobs: mpsc::Sender<Job>,
    path: PathBuf,
    clock: Clock,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

impl TasksIndex {
    /// 准备存储（mkdir、pragma、等写锁、迁移、启动修复）并打开业务连接。
    ///
    /// `progress` 在 tasks-index 线程上调用，阶段依次为 checking →（waiting_for_lock）→
    /// （migrating …）→ committing → maintaining → ready；失败时最后一次是 failed。
    pub async fn open(
        path: PathBuf,
        progress: impl Fn(StorageProgress) + Send + Sync + 'static,
    ) -> Result<Self, TasksIndexError> {
        Self::open_with(
            path,
            Arc::new(progress),
            Arc::new(now_ms),
            startup::LOCK_WAIT,
        )
        .await
    }

    pub(crate) async fn open_with(
        path: PathBuf,
        progress: Arc<dyn Fn(StorageProgress) + Send + Sync>,
        clock: Clock,
        lock_wait: Duration,
    ) -> Result<Self, TasksIndexError> {
        let (jobs, receiver) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = oneshot::channel();
        let thread_path = path.clone();
        let thread_clock = clock.clone();
        std::thread::Builder::new()
            .name("tasks-index".into())
            .spawn(move || {
                let report = |event: StorageProgress| progress(event);
                let now = || thread_clock();
                match startup::prepare(&thread_path, &report, &now, lock_wait) {
                    Ok(conn) => {
                        // open 的 future 已被丢弃时直接退出，连接随之关闭。
                        if ready_tx.send(Ok(())).is_ok() {
                            run_jobs(conn, receiver);
                        }
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                    }
                }
            })?;
        match ready_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                log::error(format!(
                    "tasks-index startup failed code={} migration={:?}: {error}",
                    error.code().as_str(),
                    error.migration_id()
                ));
                return Err(error);
            }
            Err(_) => {
                return Err(TasksIndexError::internal(
                    "tasks-index startup thread exited",
                ));
            }
        }
        log::info("tasks-index ready");
        Ok(Self {
            inner: Arc::new(Inner { jobs, path, clock }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    async fn call<T, F>(&self, operation: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection, i64) -> anyhow::Result<T> + Send + 'static,
    {
        let (reply, result) = oneshot::channel();
        let clock = self.inner.clock.clone();
        let job: Job = Box::new(move |conn| {
            let _ = reply.send(operation(conn, clock()));
        });
        self.inner
            .jobs
            .send(job)
            .map_err(|_| anyhow!("tasks-index worker stopped"))?;
        result
            .await
            .map_err(|_| anyhow!("tasks-index request aborted"))?
    }

    /// `listTaskMetas`：按 updated_at、created_at、task_id 倒序。
    pub async fn list_task_metas(&self, filter: ListFilter) -> anyhow::Result<Vec<Value>> {
        self.call(move |conn, _| {
            Ok(repo::list_task_metas(conn, &filter)?
                .iter()
                .map(JsObject::to_value)
                .collect())
        })
        .await
    }

    /// `listDeletedTaskIds`：workspace 下的删除 tombstone，按 task_id 升序。
    pub async fn list_deleted_task_ids(
        &self,
        workspace_key: &str,
        provider: Option<&str>,
    ) -> anyhow::Result<Vec<String>> {
        let workspace_key = workspace_key.to_owned();
        let provider = provider.map(str::to_owned);
        self.call(move |conn, _| {
            repo::list_deleted_task_ids(conn, &workspace_key, provider.as_deref())
        })
        .await
    }

    /// `getTaskMeta`：行不存在或已删除时为 `None`。
    pub async fn get_task_meta(
        &self,
        workspace_key: &str,
        task_id: &str,
    ) -> anyhow::Result<Option<Value>> {
        let (workspace_key, task_id) = (workspace_key.to_owned(), task_id.to_owned());
        self.call(move |conn, _| {
            Ok(repo::get_task_meta(conn, &workspace_key, &task_id)?.map(|meta| meta.to_value()))
        })
        .await
    }

    /// `seedTaskMetaIfMissing` 的批量形式：只插入不存在的行，已有行（含已删除）不动。
    /// 整批先校验、再在一个 IMMEDIATE 事务里写入；返回新插入的行数。
    pub async fn seed_task_metas_if_missing(&self, metas: Vec<Value>) -> anyhow::Result<usize> {
        let metas = metas
            .into_iter()
            .map(input_meta)
            .collect::<anyhow::Result<Vec<_>>>()?;
        self.call(move |conn, _| {
            Ok(repo::seed_task_metas_if_missing(conn, &metas)?
                .iter()
                .filter(|(_, inserted)| *inserted)
                .count())
        })
        .await
    }

    /// `syncTaskMeta`：合并 agent 快照，不初始化 grouped 顶层顺序（终态重同步等）。
    /// 合并规则同 [`Self::sync_task_meta_at_grouped_top`]，返回合并后读回的 meta。
    pub async fn sync_task_meta(
        &self,
        meta: Value,
        searchable_text: Option<String>,
    ) -> anyhow::Result<Value> {
        let meta = input_meta(meta)?;
        self.call(move |conn, now| {
            let (meta, _) = repo::sync_task_meta(
                conn,
                &meta,
                searchable_text.as_deref(),
                GroupedAdmission::Skip,
                now,
            )?;
            Ok(meta.to_value())
        })
        .await
    }

    /// `syncTaskMetaAtGroupedTop`：合并 agent 快照并原子初始化 grouped 顶层顺序。
    ///
    /// 合并规则（`syncTaskMetaWithGroupedAdmission`）：updatedAt 取与已有值的最大值；手动标题
    /// 保留；已有较新的 completed/error 不被较旧的 running 快照降级；缺省的 target、
    /// migrationSource、cronAutomationId、offPeakTaskId、unreadAt 保留已有值；pinned、
    /// archived、deleted 保留；首次获得 cron / off-peak 标记时归入系统分组。
    /// `searchable_text` 为 `None` 时保留已有正文索引。
    ///
    /// meta 里不能表达 JS 的 undefined：顶层 `null`（`target` 除外）按缺省处理；
    /// `target` 缺省表示保留已有值，传 `null` 表示清除。
    pub async fn sync_task_meta_at_grouped_top(
        &self,
        meta: Value,
        searchable_text: Option<String>,
    ) -> anyhow::Result<SyncOutcome> {
        let meta = input_meta(meta)?;
        self.call(move |conn, now| {
            let (meta, initialized) = repo::sync_task_meta(
                conn,
                &meta,
                searchable_text.as_deref(),
                GroupedAdmission::InitializeAtTop,
                now,
            )?;
            Ok(SyncOutcome {
                meta: meta.to_value(),
                initialized_grouped_order: initialized,
            })
        })
        .await
    }

    /// `applyAgentPatch`：只读取 `title`、`status`、`lastError`、`target`、`updatedAt`。
    ///
    /// - 键缺失：保留当前值。
    /// - `lastError: null` / `status: null`：清除该字段（TS 传 `lastError: undefined`）。
    /// - `target: null`：目标已清除，写入 `null`（schema 允许，与 TS 收到 null 时相同）。
    /// - `title` / `updatedAt` 是必填字段，不能清除：传 `null` 返回错误。空标题按 TS 真值规则
    ///   视为未提供。
    ///
    /// 行缺失或已删除时为 `None`；手动重命名的标题不会被覆盖。
    pub async fn apply_agent_patch(
        &self,
        workspace_key: &str,
        task_id: &str,
        patch: Value,
    ) -> anyhow::Result<Option<Value>> {
        let patch = input_patch(patch)?;
        let (workspace_key, task_id) = (workspace_key.to_owned(), task_id.to_owned());
        self.call(move |conn, _| {
            Ok(
                repo::apply_agent_patch(conn, &workspace_key, &task_id, &patch)?
                    .map(|meta| meta.to_value()),
            )
        })
        .await
    }

    /// `setTaskUnread`：`unread=true` 在写锁内分配 `max(now, last_unread_at + 1)`；
    /// `unread=false` 带 `expected_unread_at` 是 CAS 清除，不带则无条件清除。
    pub async fn set_unread(
        &self,
        workspace_key: &str,
        task_id: &str,
        unread: bool,
        expected_unread_at: Option<i64>,
    ) -> anyhow::Result<UnreadOutcome> {
        let (workspace_key, task_id) = (workspace_key.to_owned(), task_id.to_owned());
        self.call(move |conn, now| {
            set_unread(
                conn,
                &workspace_key,
                &task_id,
                unread,
                expected_unread_at,
                now,
            )
        })
        .await
    }

    /// `setTaskPinned`。行缺失或已删除时为 `None`。
    pub async fn set_pinned(
        &self,
        workspace_key: &str,
        task_id: &str,
        pinned: bool,
    ) -> anyhow::Result<Option<Value>> {
        self.update_state(
            workspace_key,
            task_id,
            StatePatch {
                pinned: Some(pinned),
                ..StatePatch::default()
            },
        )
        .await
    }

    /// `archiveTask` / `unarchiveTask`。
    pub async fn set_archived(
        &self,
        workspace_key: &str,
        task_id: &str,
        archived: bool,
    ) -> anyhow::Result<Option<Value>> {
        self.update_state(
            workspace_key,
            task_id,
            StatePatch {
                archived: Some(archived),
                ..StatePatch::default()
            },
        )
        .await
    }

    /// `deleteTask`：写 deleted=1 并在同一事务清掉分组成员与顶层顺序。
    pub async fn mark_deleted(
        &self,
        workspace_key: &str,
        task_id: &str,
    ) -> anyhow::Result<Option<Value>> {
        self.update_state(
            workspace_key,
            task_id,
            StatePatch {
                deleted: Some(true),
                ..StatePatch::default()
            },
        )
        .await
    }

    /// `renameTask`：手动标题（title_overridden=1），updatedAt 取当前时间。
    pub async fn rename(
        &self,
        workspace_key: &str,
        task_id: &str,
        title: String,
    ) -> anyhow::Result<Option<Value>> {
        let (workspace_key, task_id) = (workspace_key.to_owned(), task_id.to_owned());
        self.call(move |conn, now| {
            let patch = StatePatch {
                title: Some(title),
                title_overridden: Some(true),
                updated_at: Some(now),
                ..StatePatch::default()
            };
            Ok(
                repo::update_task_state(conn, &workspace_key, &task_id, &patch)?
                    .map(|meta| meta.to_value()),
            )
        })
        .await
    }

    async fn update_state(
        &self,
        workspace_key: &str,
        task_id: &str,
        patch: StatePatch,
    ) -> anyhow::Result<Option<Value>> {
        let (workspace_key, task_id) = (workspace_key.to_owned(), task_id.to_owned());
        self.call(move |conn, _| {
            Ok(
                repo::update_task_state(conn, &workspace_key, &task_id, &patch)?
                    .map(|meta| meta.to_value()),
            )
        })
        .await
    }
}

/// adapter `setTaskUnread` 的分支：CAS 清除走 `clearTaskUnreadIfMatches`，其余走 `updateTaskState`。
fn set_unread(
    conn: &mut Connection,
    workspace_key: &str,
    task_id: &str,
    unread: bool,
    expected_unread_at: Option<i64>,
    now: i64,
) -> anyhow::Result<UnreadOutcome> {
    if let (false, Some(expected)) = (unread, expected_unread_at) {
        return Ok(
            match repo::clear_task_unread_if_matches(conn, workspace_key, task_id, expected)? {
                Some((meta, cleared)) => UnreadOutcome {
                    meta: Some(meta.to_value()),
                    changed: cleared,
                },
                None => UnreadOutcome {
                    meta: None,
                    changed: false,
                },
            },
        );
    }
    let patch = StatePatch {
        unread_at: Some(unread.then_some(now)),
        ..StatePatch::default()
    };
    let meta = repo::update_task_state(conn, workspace_key, task_id, &patch)?;
    Ok(UnreadOutcome {
        changed: meta.is_some(),
        meta: meta.map(|meta| meta.to_value()),
    })
}

/// 请求按到达顺序串行执行；单个请求 panic 时回滚未结束的事务，线程继续服务。
fn run_jobs(mut conn: Connection, jobs: mpsc::Receiver<Job>) {
    while let Ok(job) = jobs.recv() {
        if std::panic::catch_unwind(AssertUnwindSafe(|| job(&mut conn))).is_err() {
            if !conn.is_autocommit() {
                let _ = conn.execute_batch("ROLLBACK");
            }
            log::error("tasks-index request panicked; its caller receives an error");
        }
    }
    // 所有句柄都已释放：连接随作用域关闭。
}

fn input_object(value: Value, what: &str) -> anyhow::Result<JsObject> {
    match value {
        Value::Object(map) => Ok(JsObject::from_map(map).null_as_undefined()),
        other => Err(anyhow!("{what} must be a JSON object, got {other}")),
    }
}

fn input_meta(value: Value) -> anyhow::Result<JsObject> {
    let meta = input_object(value, "task meta")?;
    validate_input_meta(&meta)?;
    Ok(meta)
}

fn input_patch(value: Value) -> anyhow::Result<JsObject> {
    let Value::Object(raw) = &value else {
        return Err(anyhow!("agent patch must be a JSON object, got {value}"));
    };
    for key in ["title", "updatedAt"] {
        anyhow::ensure!(
            !matches!(raw.get(key), Some(Value::Null)),
            "agent patch cannot clear the required field {key}"
        );
    }
    let patch = input_object(value, "agent patch")?;
    let check = |key: &str, ok: fn(&Value) -> bool| {
        patch
            .defined(key)
            .is_none_or(ok)
            .then_some(())
            .with_context(|| format!("agent patch field {key} has the wrong type"))
    };
    check("title", Value::is_string)?;
    check("status", Value::is_string)?;
    check("updatedAt", |v| as_integer(v).is_some())?;
    check("lastError", Value::is_object)?;
    check("target", Value::is_object)?;
    Ok(patch)
}
