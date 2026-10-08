//! 启动准备与迁移账本（packages/services/src/session/tasksDatabase/startup.ts、migrations.ts）。
//!
//! ```text
//! TasksIndex::open ──► 专用线程 prepare()
//!   checking ─ mkdir -p ─ 打开启动连接（busy_timeout 25）
//!   ├─ PRAGMA journal_mode=WAL      ┐ SQLITE_BUSY：每次等待先报 waiting_for_lock，
//!   ├─ inspect（只读预检账本）       │ 100 ms 重试直到 lock_wait（默认 60 min）
//!   ├─ BEGIN IMMEDIATE              ┘
//!   ├─ 账本表 → 逐项比对 checksum → migrating(完成数/总数) → 写账本 → committing → COMMIT
//!   └─ maintaining ─ 业务连接（busy_timeout 60 min 做启动修复，之后 5000）─ ready
//! 任一步失败：报 failed，错误携带分类码、SQLite 码、迁移 id 与迁移事实。
//! ```
//!
//! 同一文件由 Node Host、其他窗口的 Host 共享：迁移只在 IMMEDIATE 写锁内执行，账本 checksum
//! 与 TS 逐字节一致，不一致时以 “Task database migration checksum mismatch” 失败。
use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension as _};
use serde::Serialize;

use super::schema::{
    ACTIVE_OFF_PEAK_PREDICATE, ADOPT_TASK_SCHEMA, AUTOMATION_SCHEMA, BOUND_INDEX, COLUMNS, INDEXES,
    LEDGER_SCHEMA, MIGRATIONS, OFF_PEAK_SCHEMA, OFFICIAL_GLM_SELECTION,
    OFFICIAL_GLM_SELECTION_MIGRATION_SQL, PROVIDER_SELECTION, TASK_INDEX_SCHEMA,
};
use super::{legacy_selection, repo};

/// 启动拿写锁的最长等待（TS `LOCK_WAIT_MS`）。
pub(crate) const LOCK_WAIT: Duration = Duration::from_secs(60 * 60);
const STARTUP_BUSY_TIMEOUT_MS: u64 = 25;
/// 业务连接的 busy_timeout（`TaskIndexRepo` 默认 5000）。
const BUSINESS_BUSY_TIMEOUT_MS: u64 = 5000;
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// 启动阶段（`DatabaseStartupState.databasePhase`，另加 `failed`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StoragePhase {
    Checking,
    WaitingForLock,
    Migrating,
    Committing,
    /// 迁移已提交，正在跑启动修复（TS 的 `maintaining`）。
    Maintaining,
    Ready,
    Failed,
}

/// `DatabaseMigrationFacts.kind`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationKind {
    None,
    Initialize,
    Upgrade,
}

/// `DatabaseMigrationFacts`：需求种类与本次真实执行/提交的迁移数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationFacts {
    pub kind: MigrationKind,
    pub executed_count: u32,
    pub committed_count: u32,
    /// 锁内账本基线：外层 `None` 表示尚未取得（字段缺失），`Some(None)` 表示账本为空（null）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_applied_migration_id: Option<Option<String>>,
}

/// 一次进度回调。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageProgress {
    pub phase: StoragePhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub migration: Option<MigrationFacts>,
    /// 已执行的迁移数（`migrating` / `committing` 时给出）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<u32>,
    /// 本次锁内待执行的迁移总数（同上）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
}

/// `DatabaseStartupErrorCode` 中由数据库准备产生的部分。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupErrorCode {
    StorageFull,
    PermissionDenied,
    IoError,
    OutOfMemory,
    Corrupt,
    OpenFailed,
    LockTimeout,
    ChecksumMismatch,
    SqlFailed,
}

impl StartupErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StorageFull => "storage_full",
            Self::PermissionDenied => "permission_denied",
            Self::IoError => "io_error",
            Self::OutOfMemory => "out_of_memory",
            Self::Corrupt => "corrupt",
            Self::OpenFailed => "open_failed",
            Self::LockTimeout => "lock_timeout",
            Self::ChecksumMismatch => "checksum_mismatch",
            Self::SqlFailed => "sql_failed",
        }
    }
}

/// 启动失败：消息与 TS 相同，另带跨进程可传的结构化事实（不含 SQL、路径内容或凭据）。
#[derive(Debug)]
pub struct TasksIndexError {
    code: StartupErrorCode,
    sqlite_code: Option<i32>,
    migration_id: Option<String>,
    migration: Option<MigrationFacts>,
    source: anyhow::Error,
}

impl TasksIndexError {
    fn new(source: anyhow::Error) -> Self {
        let (code, sqlite_code) = classify(&source);
        Self {
            code,
            sqlite_code,
            migration_id: None,
            migration: None,
            source,
        }
    }

    fn with_code(mut self, code: StartupErrorCode) -> Self {
        self.code = code;
        self
    }

    fn checksum_mismatch(migration_id: &str) -> Self {
        let mut error = Self::new(anyhow::anyhow!(
            "Task database migration checksum mismatch: {migration_id}"
        ))
        .with_code(StartupErrorCode::ChecksumMismatch);
        error.migration_id = Some(migration_id.to_owned());
        error
    }

    /// `classifyDatabaseStartupError` 的结果。
    pub fn code(&self) -> StartupErrorCode {
        self.code
    }

    /// SQLite 扩展错误码（`errcode`）。
    pub fn sqlite_code(&self) -> Option<i32> {
        self.sqlite_code
    }

    /// 失败时正在执行或校验的冻结迁移 id。
    pub fn migration_id(&self) -> Option<&str> {
        self.migration_id.as_deref()
    }

    /// 失败时的迁移事实（`startupMigration`）。
    pub fn migration(&self) -> Option<&MigrationFacts> {
        self.migration.as_ref()
    }

    pub(crate) fn internal(message: &str) -> Self {
        Self::new(anyhow::anyhow!("{message}"))
    }
}

impl fmt::Display for TasksIndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.source)
    }
}

impl std::error::Error for TasksIndexError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.source()
    }
}

impl From<rusqlite::Error> for TasksIndexError {
    fn from(error: rusqlite::Error) -> Self {
        Self::new(error.into())
    }
}

impl From<anyhow::Error> for TasksIndexError {
    fn from(error: anyhow::Error) -> Self {
        Self::new(error)
    }
}

impl From<std::io::Error> for TasksIndexError {
    fn from(error: std::io::Error) -> Self {
        Self::new(error.into())
    }
}

fn sqlite_extended_code(error: &rusqlite::Error) -> Option<i32> {
    match error {
        rusqlite::Error::SqliteFailure(failure, _) => Some(failure.extended_code),
        _ => None,
    }
}

/// `classifyDatabaseStartupError`：沿 cause 链取最具体的原因，不从错误文案推断。
fn classify(error: &anyhow::Error) -> (StartupErrorCode, Option<i32>) {
    let mut fallback = StartupErrorCode::SqlFailed;
    let mut sqlite_code = None;
    for cause in error.chain() {
        if let Some(code) = cause
            .downcast_ref::<rusqlite::Error>()
            .and_then(sqlite_extended_code)
        {
            sqlite_code.get_or_insert(code);
            match code & 0xff {
                13 => return (StartupErrorCode::StorageFull, sqlite_code),
                3 | 8 => return (StartupErrorCode::PermissionDenied, sqlite_code),
                10 => return (StartupErrorCode::IoError, sqlite_code),
                7 => return (StartupErrorCode::OutOfMemory, sqlite_code),
                11 | 26 => return (StartupErrorCode::Corrupt, sqlite_code),
                14 => fallback = StartupErrorCode::OpenFailed,
                5 => fallback = StartupErrorCode::LockTimeout,
                _ => {}
            }
        }
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind;
            match io.kind() {
                ErrorKind::StorageFull | ErrorKind::QuotaExceeded => {
                    return (StartupErrorCode::StorageFull, sqlite_code);
                }
                ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem => {
                    return (StartupErrorCode::PermissionDenied, sqlite_code);
                }
                ErrorKind::OutOfMemory => return (StartupErrorCode::OutOfMemory, sqlite_code),
                _ if is_eio(io) => return (StartupErrorCode::IoError, sqlite_code),
                _ => {}
            }
        }
    }
    (fallback, sqlite_code)
}

#[cfg(unix)]
fn is_eio(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::EIO)
}

/// Windows 的原始错误码不是 errno，不按 EIO 判断。
#[cfg(not(unix))]
fn is_eio(_: &std::io::Error) -> bool {
    false
}

// ---- 迁移 ----

/// `inspectTasksMigrationKind`：只读预检，不授权执行；迁移 runner 拿锁后仍逐项复查。
pub(crate) fn inspect_migration_kind(conn: &Connection) -> Result<MigrationKind, TasksIndexError> {
    let has_ledger = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='tasks_schema_migration'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    let mut pending = false;
    for migration in MIGRATIONS.iter() {
        let applied = if has_ledger {
            applied_checksum(conn, &migration.id)?
        } else {
            None
        };
        match applied {
            None => pending = true,
            Some(checksum) if checksum != migration.checksum => {
                return Err(TasksIndexError::checksum_mismatch(&migration.id));
            }
            Some(_) => {}
        }
    }
    if !pending {
        return Ok(MigrationKind::None);
    }
    let has_tables = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name NOT IN ('tasks_schema_migration', 'sqlite_sequence') LIMIT 1",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    Ok(if has_tables {
        MigrationKind::Upgrade
    } else {
        MigrationKind::Initialize
    })
}

fn applied_checksum(conn: &Connection, id: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT checksum FROM tasks_schema_migration WHERE id=?",
        [id],
        |row| row.get(0),
    )
    .optional()
}

/// `databaseMigrationIdSchema`：`^[a-zA-Z_0-9-]{1,128}$`。
fn is_valid_migration_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `runTasksDatabaseMigrations`（`transactionOpen: true`）：调用方已持有 `BEGIN IMMEDIATE`。
/// 成功时已 COMMIT；失败时尽力 ROLLBACK，原始错误不被回滚错误覆盖。
pub(crate) fn run_migrations(
    conn: &Connection,
    facts: &mut MigrationFacts,
    report: &dyn Fn(StorageProgress),
    now: &dyn Fn() -> i64,
) -> Result<(), TasksIndexError> {
    let mut current: Option<&str> = None;
    let result = (|| -> Result<(), TasksIndexError> {
        conn.execute_batch(LEDGER_SCHEMA)?;
        // 锁内、版本 SQL 之前采集；空账本是 null，非法编号不作为事实上报。
        let baseline: Option<String> = conn
            .query_row(
                "SELECT id FROM tasks_schema_migration ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        facts.last_applied_migration_id = match baseline {
            None => Some(None),
            Some(id) if is_valid_migration_id(&id) => Some(Some(id)),
            Some(_) => None,
        };
        let mut total = 0;
        for migration in MIGRATIONS.iter() {
            if applied_checksum(conn, &migration.id)?.is_none() {
                total += 1;
            }
        }
        for migration in MIGRATIONS.iter() {
            current = Some(&migration.id);
            if let Some(checksum) = applied_checksum(conn, &migration.id)? {
                if checksum != migration.checksum {
                    return Err(TasksIndexError::checksum_mismatch(&migration.id));
                }
                continue;
            }
            if facts.kind == MigrationKind::None {
                facts.kind = MigrationKind::Upgrade;
            }
            report(StorageProgress {
                phase: StoragePhase::Migrating,
                migration: Some(facts.clone()),
                completed: Some(facts.executed_count),
                total: Some(total),
            });
            match migration.id.as_str() {
                ADOPT_TASK_SCHEMA => adopt_schema(conn)?,
                PROVIDER_SELECTION => legacy_selection::import_legacy_automation_selections(conn)?,
                OFFICIAL_GLM_SELECTION => {
                    conn.execute_batch(OFFICIAL_GLM_SELECTION_MIGRATION_SQL)?
                }
                other => {
                    return Err(TasksIndexError::internal(&format!(
                        "tasks-index migration {other} has no Rust implementation"
                    )));
                }
            }
            facts.executed_count += 1;
            conn.execute(
                "INSERT INTO tasks_schema_migration VALUES(?,?,?)",
                (&migration.id, &migration.checksum, now()),
            )?;
        }
        report(StorageProgress {
            phase: StoragePhase::Committing,
            migration: Some(facts.clone()),
            completed: Some(facts.executed_count),
            total: Some(total),
        });
        conn.execute_batch("COMMIT")?;
        facts.committed_count = facts.executed_count;
        Ok(())
    })();
    if let Err(mut error) = result {
        if !conn.is_autocommit() {
            // 回滚也可能因 IO 失败；不能覆盖真正导致迁移失败的异常，连接随后由调用方关闭。
            let _ = conn.execute_batch("ROLLBACK");
        }
        if error.migration_id.is_none() {
            error.migration_id = current.map(str::to_owned);
        }
        return Err(error);
    }
    Ok(())
}

/// 0001：接管已有分散建表，补齐冻结的后加列与索引。
fn adopt_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(&format!(
        "{TASK_INDEX_SCHEMA}{AUTOMATION_SCHEMA}{OFF_PEAK_SCHEMA}"
    ))?;
    for (table, column, definition) in COLUMNS {
        let exists = {
            let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
            statement
                .query_map([], |row| row.get::<_, String>("name"))?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .iter()
                .any(|name| name == column)
        };
        if exists {
            continue;
        }
        conn.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition}"
        ))?;
        if table == "automations" && column == "scheduled_run_count" {
            conn.execute_batch("UPDATE automations SET scheduled_run_count=run_count")?;
        }
    }
    conn.execute_batch(INDEXES)?;
    // 沿用已裁决的旧重复绑定保留策略：有重复时不建唯一索引，但不吞掉真实 SQL 错误。
    let duplicate = conn
        .query_row(
            &format!(
                "SELECT 1 FROM off_peak_tasks WHERE {ACTIVE_OFF_PEAK_PREDICATE}
    GROUP BY workspace_key, session_id HAVING count(*)>1 LIMIT 1"
            ),
            [],
            |_| Ok(()),
        )
        .optional()?;
    if duplicate.is_none() {
        conn.execute_batch(BOUND_INDEX)?;
    }
    Ok(())
}

// ---- 启动 ----

fn set_busy_timeout(conn: &Connection, millis: u64) -> rusqlite::Result<()> {
    conn.execute_batch(&format!("PRAGMA busy_timeout = {millis}"))
}

/// 遇 SQLITE_BUSY 时重试到截止时间；每个调用点各自报一次 `waiting_for_lock`。
fn acquire<T>(
    deadline: Instant,
    report_waiting: &dyn Fn(),
    mut operation: impl FnMut() -> Result<T, TasksIndexError>,
) -> Result<T, TasksIndexError> {
    let mut waiting = false;
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if error.sqlite_code.is_some_and(|code| code & 0xff == 5) => {
                if Instant::now() >= deadline {
                    let mut timeout = TasksIndexError::new(
                        error.source.context("Task storage lock wait expired"),
                    )
                    .with_code(StartupErrorCode::LockTimeout);
                    timeout.migration_id = error.migration_id;
                    return Err(timeout);
                }
                if !waiting {
                    waiting = true;
                    report_waiting();
                }
                std::thread::sleep(LOCK_RETRY_INTERVAL);
            }
            Err(error) => return Err(error),
        }
    }
}

/// 在专用线程上同步执行（不占用 tokio 运行时）。成功返回已就绪的业务连接。
pub(crate) fn prepare(
    path: &Path,
    report: &dyn Fn(StorageProgress),
    now: &dyn Fn() -> i64,
    lock_wait: Duration,
) -> Result<Connection, TasksIndexError> {
    let progress = |phase: StoragePhase, migration: Option<&MigrationFacts>| {
        report(StorageProgress {
            phase,
            migration: migration.cloned(),
            completed: None,
            total: None,
        });
    };
    progress(StoragePhase::Checking, None);
    let mut facts: Option<MigrationFacts> = None;
    let result = (|| -> Result<Connection, TasksIndexError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        {
            let conn = Connection::open(path)?;
            set_busy_timeout(&conn, STARTUP_BUSY_TIMEOUT_MS)?;
            conn.execute_batch("PRAGMA foreign_keys = ON")?;
            let deadline = Instant::now() + lock_wait;
            acquire(
                deadline,
                &|| progress(StoragePhase::WaitingForLock, facts.as_ref()),
                || Ok(conn.execute_batch("PRAGMA journal_mode = WAL")?),
            )?;
            conn.execute_batch("PRAGMA synchronous = NORMAL")?;
            let kind = acquire(
                deadline,
                &|| progress(StoragePhase::WaitingForLock, facts.as_ref()),
                || inspect_migration_kind(&conn),
            )?;
            facts = Some(MigrationFacts {
                kind,
                executed_count: 0,
                committed_count: 0,
                last_applied_migration_id: None,
            });
            progress(StoragePhase::Checking, facts.as_ref());
            acquire(
                deadline,
                &|| progress(StoragePhase::WaitingForLock, facts.as_ref()),
                || Ok(conn.execute_batch("BEGIN IMMEDIATE")?),
            )?;
            if let Some(facts) = facts.as_mut() {
                run_migrations(&conn, facts, report, now)?;
            }
            // COMMIT 已成功，先发布事实；后续 close 失败不能把已提交误报为未提交。
            progress(StoragePhase::Maintaining, facts.as_ref());
            conn.close()
                .map_err(|(_, error)| TasksIndexError::from(error))?;
        }
        // 启动修复用长 busy_timeout，与 TS 启动 Worker 里的 repo（LOCK_WAIT_MS）一致；
        // 修复完成后同一连接降为业务连接的 5000 ms。
        let mut conn = Connection::open(path)?;
        let repair_timeout = lock_wait.as_millis().min(i32::MAX as u128) as u64;
        set_busy_timeout(&conn, repair_timeout)?;
        conn.execute_batch("PRAGMA foreign_keys = ON")?;
        conn.execute_batch("PRAGMA journal_mode = WAL")?;
        conn.execute_batch("PRAGMA synchronous = NORMAL")?;
        repo::run_startup_repairs(&mut conn, now())?;
        set_busy_timeout(&conn, BUSINESS_BUSY_TIMEOUT_MS)?;
        Ok(conn)
    })();
    match result {
        Ok(conn) => {
            progress(StoragePhase::Ready, facts.as_ref());
            Ok(conn)
        }
        Err(mut error) => {
            // 失败事实随原错误交给调用方，不倒退发布 checking/migrating，也不覆盖首因。
            error.migration = facts.clone();
            progress(StoragePhase::Failed, facts.as_ref());
            Err(error)
        }
    }
}
