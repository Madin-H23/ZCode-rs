//! tasks-index.sqlite 的冻结 DDL 与迁移账本（packages/services/src/session/tasksDatabase/
//! schema-v1.ts、migrations.ts、official-glm-selection-v3.ts）。
//!
//! 这些文本参与 0001/0003 的 checksum，也决定 `sqlite_master` 里保存的建表语句，必须与 TS 逐字节
//! 相同；`tests` 用 `scripts/rust-host/tasks-index-fixtures.ts` 生成的 fixture 校验。账本 checksum
//! 不在 Rust 里另算，直接取 fixture（TS 用 `sha256(JSON.stringify(checksumInput))` 计算）。
use std::sync::LazyLock;

use serde::Deserialize;

/// 0001 接管的 task 索引表。
pub(crate) const TASK_INDEX_SCHEMA: &str = r#"
      CREATE TABLE IF NOT EXISTS tasks (
        workspace_key TEXT NOT NULL,
        workspace_path TEXT NOT NULL,
        workspace_identity TEXT,
        task_id TEXT NOT NULL,
        title TEXT NOT NULL DEFAULT '',
        task_status TEXT,
        provider TEXT,
        mode TEXT NOT NULL DEFAULT 'build',
        model TEXT,
        migration_source TEXT,
        forked_from_task_id TEXT,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        unread_at INTEGER,
        last_unread_at INTEGER NOT NULL DEFAULT 0,
        pinned INTEGER NOT NULL DEFAULT 0,
        archived INTEGER NOT NULL DEFAULT 0,
        deleted INTEGER NOT NULL DEFAULT 0,
        title_overridden INTEGER NOT NULL DEFAULT 0,
        meta_json TEXT NOT NULL DEFAULT '{}',
        PRIMARY KEY (workspace_key, task_id)
      );

      CREATE INDEX IF NOT EXISTS idx_tasks_workspace_archived_updated
      ON tasks (workspace_key, archived, updated_at DESC)
      WHERE deleted = 0;

      CREATE INDEX IF NOT EXISTS idx_tasks_workspace_pinned_updated
      ON tasks (workspace_key, pinned, updated_at DESC)
      WHERE deleted = 0;

      CREATE TABLE IF NOT EXISTS task_groups (
        group_id TEXT PRIMARY KEY,
        title TEXT NOT NULL,
        color TEXT NOT NULL DEFAULT 'gray',
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
      );

      CREATE TABLE IF NOT EXISTS task_group_members (
        group_id TEXT NOT NULL,
        workspace_key TEXT NOT NULL,
        workspace_path TEXT NOT NULL,
        workspace_identity TEXT,
        task_id TEXT NOT NULL,
        sort_order INTEGER,
        added_at INTEGER NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        PRIMARY KEY (workspace_key, task_id),
        FOREIGN KEY (group_id) REFERENCES task_groups(group_id) ON DELETE CASCADE
      );

      CREATE TABLE IF NOT EXISTS task_group_view_node_orders (
        node_type TEXT NOT NULL,
        node_key TEXT NOT NULL,
        sort_order INTEGER NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        PRIMARY KEY (node_type, node_key)
      );

      CREATE TABLE IF NOT EXISTS task_group_workspace_bootstraps (
        workspace_key TEXT PRIMARY KEY,
        group_id TEXT,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
      );

      CREATE INDEX IF NOT EXISTS idx_task_group_members_group_order
      ON task_group_members (group_id, sort_order, added_at);

      CREATE INDEX IF NOT EXISTS idx_task_group_view_node_orders_order
      ON task_group_view_node_orders (sort_order, created_at);
    "#;

/// 0001 接管的 automation 表（Rust Host 尚未实现 automation，只负责建表与迁移）。
pub(crate) const AUTOMATION_SCHEMA: &str = r#"
      CREATE TABLE IF NOT EXISTS automations (
        automation_id TEXT PRIMARY KEY,
        title TEXT NOT NULL DEFAULT '',
        cron_expr TEXT NOT NULL,
        prompt TEXT NOT NULL,
        model TEXT,
        provider TEXT,
        mode TEXT,
        thought_level TEXT,
        model_selection TEXT,
        workspace_key TEXT NOT NULL,
        workspace_path TEXT NOT NULL,
        workspace_identity TEXT,
        target_task_id TEXT,
        bot_delivery_target TEXT,
        location_kind TEXT NOT NULL DEFAULT 'local',
        recurring INTEGER NOT NULL DEFAULT 1,
        max_runs INTEGER,
        end_at INTEGER,
        schedule_rule TEXT,
        schedule_edited_by_user INTEGER NOT NULL DEFAULT 0,
        run_count INTEGER NOT NULL DEFAULT 0,
        scheduled_run_count INTEGER NOT NULL DEFAULT 0,
        enabled INTEGER NOT NULL DEFAULT 1,
        lifecycle_status TEXT NOT NULL DEFAULT 'active',
        next_run_at INTEGER,
        last_run_at INTEGER,
        running INTEGER NOT NULL DEFAULT 0,
        claimed_at INTEGER,
        dispatch_status TEXT NOT NULL DEFAULT 'idle',
        dispatch_attempts INTEGER NOT NULL DEFAULT 0,
        retry_at INTEGER,
        last_error TEXT,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
      );

      CREATE INDEX IF NOT EXISTS idx_automations_due
      ON automations (enabled, next_run_at);

      CREATE INDEX IF NOT EXISTS idx_automations_retry
      ON automations (enabled, retry_at);

      CREATE INDEX IF NOT EXISTS idx_automations_workspace
      ON automations (workspace_key);

      CREATE TABLE IF NOT EXISTS automation_runs (
        run_id TEXT PRIMARY KEY,
        automation_id TEXT NOT NULL,
        workspace_key TEXT NOT NULL,
        scheduled_at INTEGER,
        trigger TEXT NOT NULL DEFAULT 'schedule',
        model_selection TEXT,
        dispatch_status TEXT NOT NULL DEFAULT 'claimed',
        outcome TEXT,
        session_id TEXT,
        error TEXT,
        attempts INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
      );

      CREATE INDEX IF NOT EXISTS idx_automation_runs_by_automation
      ON automation_runs (automation_id, created_at DESC);
    "#;

/// 0001 接管的闲时任务表（同上，只建表与迁移）。
pub(crate) const OFF_PEAK_SCHEMA: &str = r#"
      CREATE TABLE IF NOT EXISTS off_peak_tasks (
        off_peak_task_id   TEXT PRIMARY KEY,
        server_ticket_id   TEXT,
        title              TEXT NOT NULL DEFAULT '',
        conversation_id    TEXT,
        session_id         TEXT,
        prompt             TEXT NOT NULL,
        permission_mode    TEXT NOT NULL,
        model              TEXT,
        thought_level      TEXT,
        model_selection    TEXT,
        workspace_key      TEXT NOT NULL,
        workspace_path     TEXT NOT NULL,
        workspace_identity TEXT,
        status             TEXT NOT NULL,
        queued_at          INTEGER NOT NULL,
        started_at         INTEGER,
        ended_at           INTEGER,
        failure_reason     TEXT,
        files_changed      INTEGER,
        settled_at         INTEGER,
        history_deleted_at INTEGER,
        registered_at      INTEGER,
        schedulable        INTEGER NOT NULL DEFAULT 0,
        queue_position     INTEGER,
        next_poll_at       INTEGER,
        claim_running      INTEGER NOT NULL DEFAULT 0,
        claimed_at         INTEGER,
        attempt_count      INTEGER NOT NULL DEFAULT 0,
        last_error         TEXT,
        created_at         INTEGER NOT NULL,
        updated_at         INTEGER NOT NULL
      );

      CREATE INDEX IF NOT EXISTS idx_off_peak_pick
      ON off_peak_tasks (status, queued_at);

      CREATE INDEX IF NOT EXISTS idx_off_peak_ws
      ON off_peak_tasks (workspace_key, status);
    "#;

/// 冻结的后加列（旧库按 `PRAGMA table_info` 补齐）；顺序也是 checksum 输入的一部分。
pub(crate) const COLUMNS: [(&str, &str, &str); 18] = [
    ("tasks", "title_overridden", "INTEGER NOT NULL DEFAULT 0"),
    ("tasks", "last_unread_at", "INTEGER NOT NULL DEFAULT 0"),
    ("tasks", "searchable_text", "TEXT NOT NULL DEFAULT ''"),
    ("tasks", "cron_automation_id", "TEXT"),
    ("tasks", "off_peak_task_id", "TEXT"),
    ("automations", "target_task_id", "TEXT"),
    ("automations", "bot_delivery_target", "TEXT"),
    ("automations", "mode", "TEXT"),
    ("automations", "end_at", "INTEGER"),
    ("automations", "schedule_rule", "TEXT"),
    (
        "automations",
        "schedule_edited_by_user",
        "INTEGER NOT NULL DEFAULT 0",
    ),
    ("automations", "thought_level", "TEXT"),
    ("automations", "model_selection", "TEXT"),
    (
        "automations",
        "scheduled_run_count",
        "INTEGER NOT NULL DEFAULT 0",
    ),
    ("automation_runs", "model_selection", "TEXT"),
    ("off_peak_tasks", "thought_level", "TEXT"),
    ("off_peak_tasks", "model_selection", "TEXT"),
    ("off_peak_tasks", "history_deleted_at", "INTEGER"),
];

pub(crate) const INDEXES: &str = "
  CREATE INDEX IF NOT EXISTS idx_tasks_cron_automation ON tasks(cron_automation_id, updated_at DESC)
    WHERE cron_automation_id IS NOT NULL AND deleted=0;
  CREATE INDEX IF NOT EXISTS idx_tasks_off_peak_task ON tasks(off_peak_task_id, updated_at DESC)
    WHERE off_peak_task_id IS NOT NULL AND deleted=0;
  CREATE INDEX IF NOT EXISTS idx_automations_target_task ON automations(target_task_id) WHERE target_task_id IS NOT NULL;
";

/// 活跃闲时绑定：`session_id IS NOT NULL AND status NOT IN (终态)`。
pub(crate) const ACTIVE_OFF_PEAK_PREDICATE: &str =
    "session_id IS NOT NULL AND status NOT IN ('completed','failed','cancelled')";

pub(crate) const BOUND_INDEX: &str = "CREATE UNIQUE INDEX IF NOT EXISTS idx_off_peak_bound_active ON off_peak_tasks(workspace_key,session_id) WHERE session_id IS NOT NULL AND status NOT IN ('completed','failed','cancelled')";

/// 0001 checksum 输入的最后一项：冻结的 scheduled_run_count 回填语义版本。
/// 运行时 checksum 取自 fixture，这些常量只用于校验 Rust 文本与 TS 一致。
#[cfg(test)]
pub(crate) const SCHEDULED_COUNT_BACKFILL: &str = "scheduled-count-backfill-v1";

/// 0002 不哈希函数文本（打包会改变它），只哈希冻结的语义版本。
#[cfg(test)]
pub(crate) const PROVIDER_SELECTION_VERSIONS: [&str; 2] = [
    "legacy-automation-selection-v1",
    "no-provider-for-legacy-off-peak-v1",
];

/// 0003：只改 account 计划的当前选择，旧字段与坏 JSON 原样保留。
pub(crate) const OFFICIAL_GLM_SELECTION_MIGRATION_SQL: &str = r#"
UPDATE automations
SET model_selection = json_set(model_selection, '$.modelId',
  CASE lower(json_extract(model_selection, '$.modelId'))
    WHEN 'glm-5.3' THEN 'GLM-5.3'
    WHEN 'glm-5.3-flash' THEN 'GLM-5.3-Flash'
    WHEN 'glm-5v-turbo' THEN 'GLM-5V-Turbo'
    WHEN 'glm-5.2' THEN 'GLM-5.2'
    WHEN 'glm-5.1' THEN 'GLM-5.1'
    WHEN 'glm-5.1-highspeed' THEN 'GLM-5.1-Highspeed'
    WHEN 'glm-5' THEN 'GLM-5'
    WHEN 'glm-5-turbo' THEN 'GLM-5-Turbo'
    WHEN 'glm-4.7' THEN 'GLM-4.7'
    WHEN 'glm-4.7-flashx' THEN 'GLM-4.7-FlashX'
    WHEN 'glm-4.7-flash' THEN 'GLM-4.7-Flash'
    WHEN 'glm-4.6' THEN 'GLM-4.6'
    WHEN 'glm-4.5-air' THEN 'GLM-4.5-Air'
    WHEN 'glm-4.5' THEN 'GLM-4.5'
    WHEN 'glm-4.6v' THEN 'GLM-4.6V'
    WHEN 'glm-4.6v-flash' THEN 'GLM-4.6V-Flash'
    WHEN 'glm-4.6v-flashx' THEN 'GLM-4.6V-FlashX'
    WHEN 'glm-4.1v-thinking-flashx' THEN 'GLM-4.1V-Thinking-FlashX'
    WHEN 'glm-4.1v-thinking-flash' THEN 'GLM-4.1V-Thinking-Flash'
    WHEN 'glm-4-flashx-250414' THEN 'GLM-4-FlashX-250414'
    WHEN 'glm-4-flash-250414' THEN 'GLM-4-Flash-250414'
    WHEN 'glm-4v-flash' THEN 'GLM-4V-Flash'
    ELSE json_extract(model_selection, '$.modelId')
  END)
WHERE CASE WHEN json_valid(model_selection) THEN
  json_extract(model_selection, '$.providerId') IN ('account:zai-start-plan', 'account:bigmodel-start-plan', 'account:zai-individual-coding-plan', 'account:bigmodel-individual-coding-plan', 'account:zai-team-coding-plan', 'account:bigmodel-team-coding-plan')
  AND lower(json_extract(model_selection, '$.modelId')) IN ('glm-5.3', 'glm-5.3-flash', 'glm-5v-turbo', 'glm-5.2', 'glm-5.1', 'glm-5.1-highspeed', 'glm-5', 'glm-5-turbo', 'glm-4.7', 'glm-4.7-flashx', 'glm-4.7-flash', 'glm-4.6', 'glm-4.5-air', 'glm-4.5', 'glm-4.6v', 'glm-4.6v-flash', 'glm-4.6v-flashx', 'glm-4.1v-thinking-flashx', 'glm-4.1v-thinking-flash', 'glm-4-flashx-250414', 'glm-4-flash-250414', 'glm-4v-flash')
  ELSE 0 END;
"#;

/// 迁移账本表；文本与 `runTasksDatabaseMigrations` 相同，`sqlite_master` 才会一致。
pub(crate) const LEDGER_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS tasks_schema_migration (
      id TEXT PRIMARY KEY, checksum TEXT NOT NULL, time_applied INTEGER NOT NULL
    )";

pub(crate) const ADOPT_TASK_SCHEMA: &str = "0001_adopt_task_schema";
pub(crate) const PROVIDER_SELECTION: &str = "0002_provider_selection";
pub(crate) const OFFICIAL_GLM_SELECTION: &str = "0003_official_glm_selection";

/// 账本里的一项：TS 定义的 id 与 checksum。
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Migration {
    pub id: String,
    pub checksum: String,
}

#[derive(Deserialize)]
struct MigrationFixture {
    migrations: Vec<Migration>,
}

/// 由 `scripts/rust-host/tasks-index-fixtures.ts` 从 TS 迁移定义生成；`--check` 保证不过期。
pub(crate) const MIGRATIONS_FIXTURE: &str = include_str!("fixtures/migrations.json");

/// 按 TS 定义顺序排列的迁移账本。
pub(crate) static MIGRATIONS: LazyLock<Vec<Migration>> = LazyLock::new(|| {
    serde_json::from_str::<MigrationFixture>(MIGRATIONS_FIXTURE)
        .expect("tasks-index migrations fixture is embedded at build time")
        .migrations
});
