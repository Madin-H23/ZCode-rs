//! tasks 表读写（packages/services/src/session/taskIndexRepo.ts）。
//!
//! 全部是同步函数，只在 [`super::TasksIndex`] 的专用线程上调用。每个写操作都在
//! `BEGIN IMMEDIATE` 里完成“读当前行 → 合并 → 写回”：TS 只对删除、unread 与 grouped 顶层
//! 顺序这样做，Rust 对所有读改写都这样做，多个 Host 交错写同一行时不会丢更新。
use anyhow::Context as _;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, OptionalExtension as _, Row, TransactionBehavior, named_params};
use serde_json::Value;
use zcode_kit::js::{json as js_json, string as js_string};

use super::meta::{
    JsObject, TaskRow, is_remote_identity, meta_workspace_key, resolve_row_workspace_identity,
    row_to_meta, truthy,
};

/// 搜索正文只为匹配服务，截断上限与 TS 相同（UTF-16 单元）。
const TASK_SEARCH_TEXT_MAX_CHARS: usize = 200_000;
const GROUPED_TASK_ORDER_STEP: i64 = 1000;
/// packages/shared/src/zcode-task-types.ts 的系统分组。
pub(crate) const CRON_DEFAULT_GROUP_ID: &str = "zcode-default-group-cron";
pub(crate) const OFF_PEAK_DEFAULT_GROUP_ID: &str = "zcode-default-group-off-peak";

const SELECT_TASK_ROW: &str = "SELECT
          workspace_key,
          workspace_path,
          workspace_identity,
          task_id,
          title,
          task_status,
          provider,
          mode,
          model,
          migration_source,
          forked_from_task_id,
          cron_automation_id,
          off_peak_task_id,
          created_at,
          updated_at,
          unread_at,
          last_unread_at,
          pinned,
          archived,
          deleted,
          title_overridden,
          searchable_text,
          meta_json
        FROM tasks
        WHERE workspace_key = ? AND task_id = ?";

const LIST_TASK_METAS: &str = "SELECT
          workspace_key,
          workspace_path,
          workspace_identity,
          task_id,
          title,
          task_status,
          provider,
          mode,
          model,
          migration_source,
          forked_from_task_id,
          cron_automation_id,
          off_peak_task_id,
          created_at,
          updated_at,
          unread_at,
          pinned,
          archived,
          deleted,
          title_overridden,
          searchable_text,
          meta_json
        FROM tasks
        WHERE (@workspace_key IS NULL OR workspace_key = @workspace_key)
          AND (@include_deleted = 1 OR deleted = 0)
          -- 按请求指定的 runtime provider 过滤；迁移来源另存于 migration_source。
          AND (@provider IS NULL OR provider = @provider)
          AND (@pinned IS NULL OR pinned = @pinned)
          AND (@archived IS NULL OR archived = @archived)
        ORDER BY updated_at DESC, created_at DESC, task_id DESC";

const LIST_DELETED_TASK_IDS: &str = "SELECT task_id
        FROM tasks
        WHERE workspace_key = @workspace_key
          AND deleted = 1
          AND (@provider IS NULL OR provider = @provider)
        ORDER BY task_id";

const WRITE_RECORD: &str = "INSERT INTO tasks (
          workspace_key,
          workspace_path,
          workspace_identity,
          task_id,
          title,
          task_status,
          provider,
          mode,
          model,
          migration_source,
          forked_from_task_id,
          cron_automation_id,
          off_peak_task_id,
          created_at,
          updated_at,
          unread_at,
          last_unread_at,
          pinned,
          archived,
          deleted,
          title_overridden,
          searchable_text,
          meta_json
        ) VALUES (
          @workspace_key,
          @workspace_path,
          @workspace_identity,
          @task_id,
          @title,
          @task_status,
          @provider,
          @mode,
          @model,
          @migration_source,
          @forked_from_task_id,
          @cron_automation_id,
          @off_peak_task_id,
          @created_at,
          @updated_at,
          @unread_at,
          @last_unread_at,
          @pinned,
          @archived,
          @deleted,
          @title_overridden,
          @searchable_text,
          @meta_json
        )
        ON CONFLICT(workspace_key, task_id) DO UPDATE SET
          workspace_path = excluded.workspace_path,
          workspace_identity = excluded.workspace_identity,
          title = excluded.title,
          task_status = excluded.task_status,
          provider = excluded.provider,
          mode = excluded.mode,
          model = excluded.model,
          migration_source = excluded.migration_source,
          forked_from_task_id = excluded.forked_from_task_id,
          cron_automation_id = excluded.cron_automation_id,
          off_peak_task_id = excluded.off_peak_task_id,
          created_at = excluded.created_at,
          updated_at = excluded.updated_at,
          unread_at = CASE
            WHEN @write_unread_at = 1 THEN excluded.unread_at
            ELSE tasks.unread_at
          END,
          last_unread_at = MAX(
            tasks.last_unread_at,
            COALESCE(tasks.unread_at, 0),
            CASE WHEN @write_unread_at = 1 THEN excluded.last_unread_at ELSE 0 END
          ),
          pinned = excluded.pinned,
          archived = excluded.archived,
          deleted = excluded.deleted,
          title_overridden = excluded.title_overridden,
          searchable_text = excluded.searchable_text,
          meta_json = excluded.meta_json";

// ---- 行读取 ----

/// node:sqlite 把 INTEGER/REAL 都读成 JS number；这里按同样的宽松规则取整数列。
fn int_column(row: &Row<'_>, name: &str) -> rusqlite::Result<Option<i64>> {
    Ok(match row.get_ref(name)? {
        ValueRef::Integer(i) => Some(i),
        ValueRef::Real(f) => Some(f as i64),
        _ => None,
    })
}

/// 列值原样转成 JSON（`created_at` / `updated_at` 进入兜底 meta）。
pub(crate) fn sql_to_json(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(f) => serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(_) => Value::Null,
    }
}

pub(crate) fn read_task_row(row: &Row<'_>) -> rusqlite::Result<TaskRow> {
    let flag = |name: &str| int_column(row, name).map(|v| v == Some(1));
    // listTaskMetas 的 SELECT 不含 last_unread_at（rowToMeta 用不到）。
    let has_last_unread = row.as_ref().column_index("last_unread_at").is_ok();
    Ok(TaskRow {
        workspace_key: row.get("workspace_key")?,
        workspace_path: row.get("workspace_path")?,
        workspace_identity: row.get("workspace_identity")?,
        task_id: row.get("task_id")?,
        title: row.get("title")?,
        task_status: row.get("task_status")?,
        provider: row.get("provider")?,
        mode: row.get("mode")?,
        model: row.get("model")?,
        migration_source: row.get("migration_source")?,
        forked_from_task_id: row.get("forked_from_task_id")?,
        cron_automation_id: row.get("cron_automation_id")?,
        off_peak_task_id: row.get("off_peak_task_id")?,
        created_at: sql_to_json(row.get_ref("created_at")?),
        updated_at: sql_to_json(row.get_ref("updated_at")?),
        unread_at: int_column(row, "unread_at")?,
        last_unread_at: if has_last_unread {
            int_column(row, "last_unread_at")?.unwrap_or(0)
        } else {
            0
        },
        pinned: flag("pinned")?,
        archived: flag("archived")?,
        deleted: flag("deleted")?,
        title_overridden: flag("title_overridden")?,
        searchable_text: row.get("searchable_text")?,
        meta_json: row.get("meta_json")?,
    })
}

pub(crate) fn get_task_row(
    conn: &Connection,
    workspace_key: &str,
    task_id: &str,
) -> rusqlite::Result<Option<TaskRow>> {
    conn.prepare_cached(SELECT_TASK_ROW)?
        .query_row((workspace_key, task_id), read_task_row)
        .optional()
}

// ---- writeRecord ----

/// 一次 tasks 行写入（TS `TaskIndexWriteRecord`）。
pub(crate) struct WriteRecord<'a> {
    pub meta: &'a JsObject,
    pub pinned: bool,
    pub archived: bool,
    pub deleted: bool,
    pub title_overridden: bool,
    /// 只有 unread 专属写路径可以改 unread_at；其他写入保留现有 CAS marker。
    pub write_unread_at: bool,
    /// `None` 保留已有 searchable_text（不带正文上下文的写入不能清空索引）。
    pub searchable_text: Option<&'a str>,
}

/// JS 值绑定到 SQLite（node:sqlite 的规则：undefined/null → NULL，不接受布尔与对象）。
fn bind(value: Option<&Value>) -> anyhow::Result<SqlValue> {
    Ok(match value {
        None | Some(Value::Null) => SqlValue::Null,
        Some(Value::String(s)) => SqlValue::Text(s.clone()),
        Some(Value::Number(n)) => match n.as_i64() {
            Some(i) => SqlValue::Integer(i),
            None => SqlValue::Real(n.as_f64().unwrap_or(f64::NAN)),
        },
        Some(other) => anyhow::bail!("cannot bind {other} to a tasks-index column"),
    })
}

pub(crate) fn write_record(conn: &Connection, record: WriteRecord<'_>) -> anyhow::Result<JsObject> {
    let meta = record.meta;
    let key = meta_workspace_key(meta);
    let task_id = meta.str("taskId").unwrap_or_default().to_owned();
    let searchable_text = match record.searchable_text {
        Some(text) => js_string::utf16_prefix(text, TASK_SEARCH_TEXT_MAX_CHARS).to_owned(),
        None => get_task_row(conn, &key, &task_id)?
            .map(|row| row.searchable_text)
            .unwrap_or_default(),
    };
    let unread_at = bind(meta.get("unreadAt"))?;
    let last_unread_at = match &unread_at {
        SqlValue::Null => SqlValue::Integer(0),
        value => value.clone(),
    };
    conn.prepare_cached(WRITE_RECORD)?.execute(named_params! {
        "@workspace_key": key,
        "@workspace_path": bind(meta.get("workspacePath"))?,
        "@workspace_identity": bind(meta.get("workspaceIdentity"))?,
        "@task_id": task_id,
        "@title": bind(meta.get("title"))?,
        "@task_status": bind(meta.get("status"))?,
        "@provider": bind(meta.get("provider"))?,
        "@mode": bind(meta.get("mode"))?,
        "@model": bind(meta.get("model"))?,
        "@migration_source": bind(meta.get("migrationSource"))?,
        "@forked_from_task_id": bind(meta.get("forkedFromTaskId"))?,
        // cron / off-peak 身份随 meta_json 写入（单一来源），另投影到索引列。
        "@cron_automation_id": bind(meta.get("cronAutomationId"))?,
        "@off_peak_task_id": bind(meta.get("offPeakTaskId"))?,
        "@created_at": bind(meta.get("createdAt"))?,
        "@updated_at": bind(meta.get("updatedAt"))?,
        "@unread_at": unread_at,
        "@last_unread_at": last_unread_at,
        "@write_unread_at": i64::from(record.write_unread_at),
        "@pinned": i64::from(record.pinned),
        "@archived": i64::from(record.archived),
        "@deleted": i64::from(record.deleted),
        "@title_overridden": i64::from(record.title_overridden),
        "@searchable_text": searchable_text,
        "@meta_json": meta.stringify(),
    })?;
    let persisted = get_task_row(conn, &key, &task_id)?
        .with_context(|| format!("tasks-index row missing after write: {task_id}"))?;
    Ok(row_to_meta(&persisted))
}

// ---- grouped 顺序与系统分组 ----

fn next_grouped_top_sort_order(conn: &Connection) -> rusqlite::Result<i64> {
    let min: Option<i64> = conn.query_row(
        "SELECT MIN(sort_order) AS min_sort_order
        FROM task_group_view_node_orders",
        [],
        |row| row.get(0),
    )?;
    Ok(min.unwrap_or(GROUPED_TASK_ORDER_STEP * 2) - GROUPED_TASK_ORDER_STEP)
}

/// `taskOrderNodeKey`：`JSON.stringify([workspaceKey, taskId])`，不能用 NUL 分隔。
pub(crate) fn task_order_node_key(workspace_key: &str, task_id: &str) -> String {
    js_json::stringify(&Value::Array(vec![
        Value::String(workspace_key.to_owned()),
        Value::String(task_id.to_owned()),
    ]))
}

/// 删除 task 时一并清掉分组成员与顶层顺序（含旧版本只写 workspace key 的顺序行）。
pub(crate) fn delete_task_grouping_references(
    conn: &Connection,
    workspace_key: &str,
    task_id: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM task_group_members
        WHERE workspace_key = ? AND task_id = ?",
        (workspace_key, task_id),
    )?;
    conn.execute(
        "DELETE FROM task_group_view_node_orders
        WHERE node_type = 'task' AND (node_key = ? OR node_key = ?)",
        (task_order_node_key(workspace_key, task_id), workspace_key),
    )?;
    Ok(())
}

struct SystemGroup {
    id: &'static str,
    title: &'static str,
    color: &'static str,
}

const CRON_GROUP: SystemGroup = SystemGroup {
    id: CRON_DEFAULT_GROUP_ID,
    title: "cron",
    color: "blue",
};
const OFF_PEAK_GROUP: SystemGroup = SystemGroup {
    id: OFF_PEAK_DEFAULT_GROUP_ID,
    title: "off-peak",
    color: "purple",
};

/// 归入系统分组；全部 INSERT OR IGNORE，绝不覆盖用户手动整理的结果。
fn ensure_system_group_membership(
    conn: &Connection,
    workspace_path: &str,
    workspace_identity: Option<&str>,
    task_id: &str,
    group: &SystemGroup,
    now: i64,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO task_groups (group_id, title, color, created_at, updated_at)
        VALUES (?, ?, ?, ?, ?)",
        (group.id, group.title, group.color, now, now),
    )?;
    let sort_order = next_grouped_top_sort_order(conn)?;
    conn.execute(
        "INSERT OR IGNORE INTO task_group_view_node_orders (node_type, node_key, sort_order, created_at, updated_at)
        VALUES ('group', ?, ?, ?, ?)",
        (group.id, sort_order, now, now),
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO task_group_members (
          group_id,
          workspace_key,
          workspace_path,
          workspace_identity,
          task_id,
          sort_order,
          added_at,
          created_at,
          updated_at
        ) VALUES (?, ?, ?, ?, ?, NULL, ?, ?, ?)",
        (
            group.id,
            super::meta::resolve_workspace_key(workspace_path, workspace_identity),
            workspace_path,
            workspace_identity,
            task_id,
            now,
            now,
            now,
        ),
    )?;
    Ok(())
}

/// 闲时任务暂不支持远程 workspace：远程会话即使带标记也不归组。
pub(crate) fn ensure_off_peak_group_membership(
    conn: &Connection,
    workspace_path: &str,
    workspace_identity: Option<&str>,
    task_id: &str,
    now: i64,
) -> anyhow::Result<()> {
    if workspace_identity
        .is_some_and(|identity| !identity.is_empty() && is_remote_identity(identity))
    {
        return Ok(());
    }
    ensure_system_group_membership(
        conn,
        workspace_path,
        workspace_identity,
        task_id,
        &OFF_PEAK_GROUP,
        now,
    )
}

/// 首次公开时把 task 放到 grouped 顶层最前；已有成员关系或顶层顺序时不动（不能让慢回源越过新任务）。
pub(crate) fn initialize_grouped_task_at_top(
    conn: &Connection,
    workspace_key: &str,
    task_id: &str,
    now: i64,
) -> anyhow::Result<bool> {
    let Some(row) = get_task_row(conn, workspace_key, task_id)? else {
        return Ok(false);
    };
    if row.deleted || row.archived || row.pinned {
        return Ok(false);
    }
    let member = conn
        .query_row(
            "SELECT 1 AS found
        FROM task_group_members
        WHERE workspace_key = ? AND task_id = ?
        LIMIT 1",
            (workspace_key, task_id),
            |_| Ok(()),
        )
        .optional()?;
    let node_key = task_order_node_key(workspace_key, task_id);
    let top_order = conn
        .query_row(
            "SELECT 1 AS found
        FROM task_group_view_node_orders
        WHERE node_type = 'task' AND node_key = ?
        LIMIT 1",
            [&node_key],
            |_| Ok(()),
        )
        .optional()?;
    if member.is_some() || top_order.is_some() {
        return Ok(false);
    }
    let sort_order = next_grouped_top_sort_order(conn)?;
    conn.execute(
        "INSERT INTO task_group_view_node_orders (
          node_type,
          node_key,
          sort_order,
          created_at,
          updated_at
        ) VALUES (?, ?, ?, ?, ?)
        ON CONFLICT(node_type, node_key) DO UPDATE SET
          sort_order = excluded.sort_order,
          updated_at = excluded.updated_at",
        ("task", &node_key, sort_order, now, now),
    )?;
    Ok(true)
}

// ---- 业务操作 ----

fn immediate(conn: &mut Connection) -> rusqlite::Result<rusqlite::Transaction<'_>> {
    conn.transaction_with_behavior(TransactionBehavior::Immediate)
}

fn is_terminal_status(status: Option<&Value>) -> bool {
    matches!(status.and_then(Value::as_str), Some("completed" | "error"))
}

/// turn.completed 先写入较新的 completed/error，随后到达的旧 running snapshot 不能把它降级。
fn should_preserve_newer_terminal_status(existing: Option<&JsObject>, incoming: &JsObject) -> bool {
    let Some(existing) = existing else {
        return false;
    };
    if !is_terminal_status(existing.get("status")) {
        return false;
    }
    let incoming_status = incoming.get("status");
    if truthy(incoming_status) && incoming_status.and_then(Value::as_str) != Some("running") {
        return false;
    }
    number(existing.get("updatedAt")) > number(incoming.get("updatedAt"))
}

fn number(value: Option<&Value>) -> f64 {
    value.and_then(Value::as_f64).unwrap_or(f64::NAN)
}

/// `seedTaskMetaIfMissing`：只在行不存在时写入基线；已存在（含已删除）的行原样返回。
/// 整批一个 IMMEDIATE 事务；返回每项读回的 meta 与是否新插入。
pub(crate) fn seed_task_metas_if_missing(
    conn: &mut Connection,
    metas: &[JsObject],
) -> anyhow::Result<Vec<(JsObject, bool)>> {
    let tx = immediate(conn)?;
    let mut results = Vec::with_capacity(metas.len());
    for meta in metas {
        let key = meta_workspace_key(meta);
        let task_id = meta.str("taskId").unwrap_or_default();
        if let Some(existing) = get_task_row(&tx, &key, task_id)? {
            results.push((row_to_meta(&existing), false));
            continue;
        }
        let persisted = write_record(
            &tx,
            WriteRecord {
                meta,
                pinned: false,
                archived: false,
                deleted: false,
                title_overridden: truthy(meta.defined("titleOverridden")),
                write_unread_at: false,
                searchable_text: None,
            },
        )?;
        results.push((persisted, true));
    }
    tx.commit()?;
    Ok(results)
}

/// `syncTaskMetaAtGroupedTop`：合并快照并在同一事务里初始化 grouped 顶层顺序。
/// grouped 顶层顺序的处理方式（`syncTaskMeta` 与 `syncTaskMetaAtGroupedTop` 的唯一差别）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GroupedAdmission {
    /// `syncTaskMeta`：只合并 task 行，不碰 grouped 顶层顺序。
    Skip,
    /// `syncTaskMetaAtGroupedTop`：同一事务里首次初始化顶层顺序。
    InitializeAtTop,
}

/// `syncTaskMetaWithGroupedAdmission`：合并 agent 快照。返回读回的 meta 与是否初始化了
/// grouped 顶层顺序（`Skip` 时恒为 false）。
pub(crate) fn sync_task_meta(
    conn: &mut Connection,
    input: &JsObject,
    searchable_text: Option<&str>,
    admission: GroupedAdmission,
    now: i64,
) -> anyhow::Result<(JsObject, bool)> {
    let tx = immediate(conn)?;
    let key = meta_workspace_key(input);
    let task_id = input.str("taskId").unwrap_or_default().to_owned();
    let existing = get_task_row(&tx, &key, &task_id)?;
    let existing_meta = existing.as_ref().map(row_to_meta);
    let existing_field = |name: &str| existing_meta.as_ref().and_then(|m| m.get(name)).cloned();
    let title_overridden = existing.as_ref().is_some_and(|row| row.title_overridden);
    // snapshot 的 updatedAt 不含 applyAgentPatch 写入的事件时间；取 max 保证单调不回退。
    let updated_at = number(input.get("updatedAt")).max(
        existing_meta
            .as_ref()
            .and_then(|m| m.get("updatedAt"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
    );
    let preserve = should_preserve_newer_terminal_status(existing_meta.as_ref(), input);
    let mut meta = input.clone();
    // 用户手动重命名属于 app 侧状态，agent 快照不能把它冲掉。
    let title = if title_overridden && existing_meta.is_some() {
        existing_field("title")
    } else {
        input.get("title").cloned()
    };
    meta.set("title", title);
    meta.set("titleOverridden", Some(Value::Bool(title_overridden)));
    let (status, last_error) = if preserve {
        (existing_field("status"), existing_field("lastError"))
    } else {
        (
            input.get("status").cloned(),
            input.get("lastError").cloned(),
        )
    };
    meta.set("status", status);
    meta.set("lastError", last_error);
    let target = if input.has_own("target") {
        input.get("target").cloned()
    } else {
        existing_field("target")
    };
    meta.set("target", target);
    // 快照不知道迁移来源 / cron / off-peak 身份：缺省时保留已有值。
    for name in ["migrationSource", "cronAutomationId", "offPeakTaskId"] {
        let value = input
            .defined(name)
            .cloned()
            .or_else(|| existing_field(name));
        meta.set(name, value);
    }
    meta.set("updatedAt", Some(js_number(updated_at)));
    let unread_at = input
        .defined("unreadAt")
        .cloned()
        .or_else(|| existing_field("unreadAt"));
    meta.set("unreadAt", unread_at);

    let persisted = write_record(
        &tx,
        WriteRecord {
            meta: &meta,
            pinned: existing.as_ref().is_some_and(|row| row.pinned),
            archived: existing.as_ref().is_some_and(|row| row.archived),
            deleted: existing.as_ref().is_some_and(|row| row.deleted),
            title_overridden,
            write_unread_at: false,
            searchable_text,
        },
    )?;
    let workspace_path = meta.str("workspacePath").unwrap_or_default();
    let workspace_identity = meta.str("workspaceIdentity");
    // 会话内 CronCreate 给已有 task 补标记，不能只判断“新行”。
    let gained = |name: &str| {
        truthy(meta.get(name)) && !truthy(existing_meta.as_ref().and_then(|m| m.get(name)))
    };
    if gained("cronAutomationId") {
        ensure_system_group_membership(
            &tx,
            workspace_path,
            workspace_identity,
            &task_id,
            &CRON_GROUP,
            now,
        )?;
    }
    if gained("offPeakTaskId") {
        ensure_off_peak_group_membership(&tx, workspace_path, workspace_identity, &task_id, now)?;
    }
    // task 行与顶层顺序同事务提交：分两次写时 sessions-index 会先公开缺序 task。
    let initialized = match admission {
        GroupedAdmission::Skip => false,
        GroupedAdmission::InitializeAtTop => {
            initialize_grouped_task_at_top(&tx, &key, &task_id, now)?
        }
    };
    tx.commit()?;
    Ok((persisted, initialized))
}

/// JS number 转 JSON：整数值写成整数。
fn js_number(value: f64) -> Value {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15 {
        Value::from(value as i64)
    } else {
        serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
    }
}

/// `applyAgentPatch`：行缺失或已删除时返回 `None`；手动标题不会被 agent 标题覆盖。
pub(crate) fn apply_agent_patch(
    conn: &mut Connection,
    workspace_key: &str,
    task_id: &str,
    patch: &JsObject,
) -> anyhow::Result<Option<JsObject>> {
    let tx = immediate(conn)?;
    let Some(row) = get_task_row(&tx, workspace_key, task_id)? else {
        return Ok(None);
    };
    if row.deleted {
        return Ok(None);
    }
    let current = row_to_meta(&row);
    let mut next = current.clone();
    let title = if !row.title_overridden && truthy(patch.get("title")) {
        patch.get("title")
    } else {
        current.get("title")
    };
    next.set("title", title.cloned());
    next.set("titleOverridden", Some(Value::Bool(row.title_overridden)));
    let updated_at = patch
        .defined("updatedAt")
        .or_else(|| current.get("updatedAt"));
    next.set("updatedAt", updated_at.cloned());
    // 键存在即覆盖（TS `"lastError" in patch`）：入参的 null 已转成 undefined，即清除该字段；
    // target 的 null 是合法值（目标已清除），原样保留。status 同样按键存在处理，见 TasksIndex 文档。
    for name in ["status", "lastError", "target"] {
        let value = if patch.has_own(name) {
            patch.get(name)
        } else {
            current.get(name)
        };
        next.set(name, value.cloned());
    }
    let persisted = write_record(
        &tx,
        WriteRecord {
            meta: &next,
            pinned: row.pinned,
            archived: row.archived,
            deleted: row.deleted,
            title_overridden: row.title_overridden,
            write_unread_at: false,
            searchable_text: None,
        },
    )?;
    tx.commit()?;
    Ok(Some(persisted))
}

/// `updateTaskState` 用到的补丁字段。
#[derive(Debug, Default, Clone)]
pub(crate) struct StatePatch {
    pub pinned: Option<bool>,
    pub archived: Option<bool>,
    pub deleted: Option<bool>,
    pub title: Option<String>,
    pub title_overridden: Option<bool>,
    pub updated_at: Option<i64>,
    /// `Some(Some(t))` 申请新的未读 marker；`Some(None)` 是“键存在、值 undefined”，即清除。
    pub unread_at: Option<Option<i64>>,
}

/// `updateTaskState`：行缺失或已删除时返回 `None`（TS 抛错，由 adapter 兜底 seed 后重试）。
pub(crate) fn update_task_state(
    conn: &mut Connection,
    workspace_key: &str,
    task_id: &str,
    patch: &StatePatch,
) -> anyhow::Result<Option<JsObject>> {
    let tx = immediate(conn)?;
    let Some(row) = get_task_row(&tx, workspace_key, task_id)? else {
        return Ok(None);
    };
    if row.deleted {
        return Ok(None);
    }
    let current = row_to_meta(&row);
    // 毫秒时间戳会让同一 task 的两次未读撞号，清除后只看当前值又会复用旧 marker；
    // 必须在写锁内基于不随清除重置的持久 watermark 分配严格递增的 marker。
    let last_unread_at = row
        .last_unread_at
        .max(row.unread_at.unwrap_or(0))
        .max(current.get("unreadAt").and_then(Value::as_i64).unwrap_or(0));
    let unread_at = match patch.unread_at {
        Some(Some(requested)) => Some(Value::from(requested.max(last_unread_at + 1))),
        Some(None) => None,
        None => current.get("unreadAt").cloned(),
    };
    let mut next = current.clone();
    next.set(
        "title",
        patch
            .title
            .clone()
            .map(Value::String)
            .or_else(|| current.get("title").cloned()),
    );
    next.set(
        "titleOverridden",
        patch
            .title_overridden
            .map(Value::Bool)
            .or_else(|| current.get("titleOverridden").cloned()),
    );
    next.set("model", current.get("model").cloned());
    next.set(
        "updatedAt",
        patch
            .updated_at
            .map(Value::from)
            .or_else(|| current.get("updatedAt").cloned()),
    );
    next.set("unreadAt", unread_at);
    for name in ["status", "lastError", "target"] {
        next.set(name, current.get(name).cloned());
    }
    let persisted = write_record(
        &tx,
        WriteRecord {
            meta: &next,
            pinned: patch.pinned.unwrap_or(row.pinned),
            archived: patch.archived.unwrap_or(row.archived),
            deleted: patch.deleted.unwrap_or(row.deleted),
            title_overridden: patch.title_overridden.unwrap_or(row.title_overridden),
            write_unread_at: patch.unread_at.is_some(),
            searchable_text: None,
        },
    )?;
    if patch.deleted == Some(true) {
        // 删除标记与分组引用必须原子提交，否则 task 可见性与分组归属会长期矛盾。
        delete_task_grouping_references(&tx, &row.workspace_key, &row.task_id)?;
    }
    tx.commit()?;
    Ok(Some(persisted))
}

/// `clearTaskUnreadIfMatches`：比较与清除持有同一写事务，旧的已读点击不能清掉新 marker。
/// 返回 `(meta, cleared)`；行缺失或已删除时返回 `None`。
pub(crate) fn clear_task_unread_if_matches(
    conn: &mut Connection,
    workspace_key: &str,
    task_id: &str,
    expected_unread_at: i64,
) -> anyhow::Result<Option<(JsObject, bool)>> {
    let tx = immediate(conn)?;
    let Some(row) = get_task_row(&tx, workspace_key, task_id)? else {
        return Ok(None);
    };
    if row.deleted {
        return Ok(None);
    }
    let current = row_to_meta(&row);
    if current.get("unreadAt").and_then(Value::as_i64) != Some(expected_unread_at) {
        tx.commit()?;
        return Ok(Some((current, false)));
    }
    let mut next = current;
    next.set("unreadAt", None);
    let persisted = write_record(
        &tx,
        WriteRecord {
            meta: &next,
            pinned: row.pinned,
            archived: row.archived,
            deleted: false,
            title_overridden: row.title_overridden,
            write_unread_at: true,
            searchable_text: None,
        },
    )?;
    tx.commit()?;
    Ok(Some((persisted, true)))
}

/// `listTaskMetas` 的过滤条件；`workspace_key` 为空表示所有 workspace。
#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    pub workspace_key: Option<String>,
    pub include_deleted: bool,
    pub provider: Option<String>,
    pub pinned: Option<bool>,
    pub archived: Option<bool>,
}

pub(crate) fn list_task_metas(
    conn: &Connection,
    filter: &ListFilter,
) -> anyhow::Result<Vec<JsObject>> {
    let workspace_key = filter
        .workspace_key
        .as_deref()
        .filter(|key| !key.is_empty());
    let mut statement = conn.prepare_cached(LIST_TASK_METAS)?;
    let rows = statement.query_map(
        named_params! {
            "@workspace_key": workspace_key,
            "@include_deleted": i64::from(filter.include_deleted),
            "@provider": filter.provider.as_deref(),
            "@pinned": filter.pinned.map(i64::from),
            "@archived": filter.archived.map(i64::from),
        },
        read_task_row,
    )?;
    let mut metas = Vec::new();
    for row in rows {
        metas.push(row_to_meta(&row?));
    }
    Ok(metas)
}

pub(crate) fn list_deleted_task_ids(
    conn: &Connection,
    workspace_key: &str,
    provider: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let mut statement = conn.prepare_cached(LIST_DELETED_TASK_IDS)?;
    let ids = statement
        .query_map(
            named_params! { "@workspace_key": workspace_key, "@provider": provider },
            |row| row.get(0),
        )?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(ids)
}

pub(crate) fn get_task_meta(
    conn: &Connection,
    workspace_key: &str,
    task_id: &str,
) -> anyhow::Result<Option<JsObject>> {
    Ok(get_task_row(conn, workspace_key, task_id)?
        .filter(|row| !row.deleted)
        .map(|row| row_to_meta(&row)))
}

// ---- 启动修复（TaskIndexRepo.initialize 与 OffPeakTaskRepo.initialize） ----

/// 存量回填：早期 off-peak 会话行没有 off_peak_task_id，按 session 绑定只补投影列。
fn backfill_off_peak_task_markers(conn: &Connection) -> rusqlite::Result<()> {
    let has_table = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'off_peak_tasks'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_table {
        return Ok(());
    }
    conn.execute(
        "UPDATE tasks SET off_peak_task_id = (
          SELECT o.off_peak_task_id FROM off_peak_tasks o
          WHERE o.session_id = tasks.task_id AND o.workspace_key = tasks.workspace_key
        )
        WHERE off_peak_task_id IS NULL
          AND EXISTS (
            SELECT 1 FROM off_peak_tasks o
            WHERE o.session_id = tasks.task_id AND o.workspace_key = tasks.workspace_key
          )",
        [],
    )?;
    Ok(())
}

/// 成员关系回填：只补过投影列的存量行不会再触发“首次获得标记”，启动时补一次系统分组归属。
fn backfill_off_peak_group_memberships(conn: &Connection, now: i64) -> anyhow::Result<()> {
    let mut statement = conn.prepare(
        "SELECT workspace_key, workspace_path, workspace_identity, task_id FROM tasks
         WHERE off_peak_task_id IS NOT NULL AND deleted = 0",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (workspace_key, workspace_path, workspace_identity, task_id) in rows {
        // remote 判定看主键 workspace_key：历史行的 identity 列可能缺失，按路径重算会串到本地。
        if is_remote_identity(&workspace_key) {
            continue;
        }
        let identity = resolve_row_workspace_identity(
            &workspace_key,
            &workspace_path,
            workspace_identity.as_deref(),
        );
        ensure_off_peak_group_membership(
            conn,
            &workspace_path,
            identity.as_deref(),
            &task_id,
            now,
        )?;
    }
    Ok(())
}

/// 旧版本删除 task 只写 deleted，分组成员与顶层顺序仍在；启动时幂等收敛。
fn cleanup_deleted_task_grouping_references(conn: &Connection) -> rusqlite::Result<()> {
    let mut statement = conn.prepare(
        "SELECT workspace_key, task_id
        FROM tasks
        WHERE deleted = 1",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (workspace_key, task_id) in rows {
        delete_task_grouping_references(conn, &workspace_key, &task_id)?;
    }
    Ok(())
}

/// 启动时的一次性修复，顺序同 TS：TaskIndexRepo 的三项，再到 OffPeakTaskRepo 的状态回收。
///
/// 每项各自一个 IMMEDIATE 事务；全部幂等，多个 Host 先后执行结果相同。
pub(crate) fn run_startup_repairs(conn: &mut Connection, now: i64) -> anyhow::Result<()> {
    {
        let tx = immediate(conn)?;
        backfill_off_peak_task_markers(&tx)?;
        tx.commit()?;
    }
    {
        let tx = immediate(conn)?;
        backfill_off_peak_group_memberships(&tx, now)?;
        tx.commit()?;
    }
    {
        let tx = immediate(conn)?;
        cleanup_deleted_task_grouping_references(&tx)?;
        tx.commit()?;
    }
    // 早期预留的 awaiting_approval 从未在生产链路写入；遗留行回到 running，交给启动回收处理。
    conn.execute(
        "UPDATE off_peak_tasks
        SET status = 'running', updated_at = @now
        WHERE status = 'awaiting_approval'",
        named_params! { "@now": now },
    )?;
    Ok(())
}
