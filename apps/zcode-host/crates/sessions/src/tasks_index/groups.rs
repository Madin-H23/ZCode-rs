//! 任务分组与 grouped 视图（taskIndexRepo.ts 的 createTaskGroup … applyGroupedTaskViewOrder）。
//!
//! SQL、事务边界、遍历顺序与错误文案逐条照搬 TS。读结构的查询没有 ORDER BY，行序就是 SQLite
//! 的扫描序；Rust 用相同的 SQL，所以与 TS 读同一个库时顺序一致。
use std::collections::{HashMap, HashSet};

use anyhow::bail;
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior};
use serde_json::{Map, Value, json};
use zcode_kit::js::string as js_string;

use super::meta::resolve_row_workspace_identity;
use super::repo::sql_to_json;
use super::task_list::{normalize_workspace_keys, scopes};

const ORDER_STEP: i64 = 1000;
const DEFAULT_COLOR: &str = "gray";
const COLORS: &[&str] = &["gray", "red", "orange", "yellow", "green", "blue", "purple"];

fn is_group_color(color: &str) -> bool {
    COLORS.contains(&color)
}

/// `rowToTaskGroup`：未知颜色读成 gray。
fn group_json(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let color: String = row.get("color")?;
    Ok(json!({
        "id": row.get::<_, String>("group_id")?,
        "title": row.get::<_, String>("title")?,
        "color": if is_group_color(&color) { color } else { DEFAULT_COLOR.to_owned() },
        "createdAt": sql_to_json(row.get_ref("created_at")?),
        "updatedAt": sql_to_json(row.get_ref("updated_at")?),
    }))
}

const SELECT_GROUP: &str = "SELECT
          group_id,
          title,
          color,
          created_at,
          updated_at
        FROM task_groups";

fn read_group(conn: &Connection, group_id: &str) -> rusqlite::Result<Option<Value>> {
    conn.query_row(
        &format!("{SELECT_GROUP}\n        WHERE group_id = ?"),
        [group_id],
        group_json,
    )
    .optional()
}

/// `getNextGroupedTopSortOrder`：当前最小值再减一步（空表时为 1000）。
fn next_top_sort_order(conn: &Connection) -> rusqlite::Result<i64> {
    let min: Option<i64> = conn.query_row(
        "SELECT MIN(sort_order) AS min_sort_order
        FROM task_group_view_node_orders",
        [],
        |row| row.get(0),
    )?;
    Ok(min.unwrap_or(ORDER_STEP * 2) - ORDER_STEP)
}

fn upsert_top_order(
    conn: &Connection,
    node_type: &str,
    node_key: &str,
    sort_order: i64,
    now: i64,
) -> rusqlite::Result<()> {
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
        (node_type, node_key, sort_order, now, now),
    )?;
    Ok(())
}

/// `createTaskGroup`：id 由调用方生成（`task-group-<uuid>`），放到 grouped 顶层最前；不开事务（同 TS）。
pub(crate) fn create_task_group(
    conn: &Connection,
    now: i64,
    group_id: &str,
    title: Option<&str>,
    color: Option<&str>,
) -> anyhow::Result<Value> {
    let title = title
        .map(js_string::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or("New Group");
    // 颜色不校验（TS 直接写入入参）。
    let color = color.unwrap_or(DEFAULT_COLOR);
    conn.execute(
        "INSERT INTO task_groups (
          group_id,
          title,
          color,
          created_at,
          updated_at
        ) VALUES (?, ?, ?, ?, ?)",
        (group_id, title, color, now, now),
    )?;
    upsert_top_order(conn, "group", group_id, next_top_sort_order(conn)?, now)?;
    Ok(json!({
        "id": group_id,
        "title": title,
        "color": color,
        "createdAt": now,
        "updatedAt": now,
    }))
}

pub(crate) fn rename_task_group(
    conn: &Connection,
    now: i64,
    group_id: &str,
    title: &str,
) -> anyhow::Result<Value> {
    let title = Some(js_string::trim(title))
        .filter(|title| !title.is_empty())
        .unwrap_or("New Group");
    let changed = conn.execute(
        "UPDATE task_groups
        SET title = ?, updated_at = ?
        WHERE group_id = ?",
        (title, now, group_id),
    )?;
    if changed == 0 {
        bail!("Task group 不存在，无法重命名");
    }
    match read_group(conn, group_id)? {
        Some(group) => Ok(group),
        None => bail!("Task group 重命名后读取失败"),
    }
}

pub(crate) fn update_task_group_color(
    conn: &Connection,
    now: i64,
    group_id: &str,
    color: Option<&str>,
) -> anyhow::Result<Value> {
    let Some(color) = color.filter(|color| is_group_color(color)) else {
        bail!("Task group 颜色无效");
    };
    let changed = conn.execute(
        "UPDATE task_groups
        SET color = ?, updated_at = ?
        WHERE group_id = ?",
        (color, now, group_id),
    )?;
    if changed == 0 {
        bail!("Task group 不存在，无法更新颜色");
    }
    match read_group(conn, group_id)? {
        Some(group) => Ok(group),
        None => bail!("Task group 更新颜色后读取失败"),
    }
}

/// `deleteTaskGroup`：成员随外键级联删除；bootstrap 行保留。
pub(crate) fn delete_task_group(conn: &mut Connection, group_id: &str) -> anyhow::Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let deleted = tx.execute("DELETE FROM task_groups WHERE group_id = ?", [group_id])?;
    if deleted == 0 {
        bail!("Task group 不存在，无法删除");
    }
    tx.execute(
        "DELETE FROM task_group_view_node_orders
          WHERE node_type = 'group' AND node_key = ?",
        [group_id],
    )?;
    tx.commit()?;
    Ok(())
}

/// bootstrap 绑定的 workspace 分组只在其 workspace 可见；其余分组总是可见。
pub(super) fn visible_groups(
    conn: &Connection,
    visible_keys: &HashSet<String>,
) -> anyhow::Result<Vec<Value>> {
    let mut bootstrap = conn.prepare(
        "SELECT workspace_key, group_id
        FROM task_group_workspace_bootstraps
        WHERE group_id IS NOT NULL",
    )?;
    let mut bound: HashMap<String, String> = HashMap::new();
    for row in bootstrap.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })? {
        let (workspace_key, group_id) = row?;
        if let Some(group_id) = group_id.filter(|id| !id.is_empty()) {
            bound.insert(group_id, workspace_key);
        }
    }
    let mut statement = conn.prepare(SELECT_GROUP)?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>("group_id")?, group_json(row)?))
    })?;
    let mut groups = Vec::new();
    for row in rows {
        let (group_id, group) = row?;
        if bound
            .get(&group_id)
            .is_none_or(|key| visible_keys.contains(key))
        {
            groups.push(group);
        }
    }
    Ok(groups)
}

/// task_group_members 的一行。
#[derive(Debug, Clone)]
pub(super) struct MemberRow {
    pub group_id: String,
    pub workspace_key: String,
    pub workspace_path: String,
    pub workspace_identity: Option<String>,
    pub task_id: String,
    pub sort_order: Option<i64>,
    pub added_at: Value,
}

pub(super) fn members(conn: &Connection) -> anyhow::Result<Vec<MemberRow>> {
    let mut statement = conn.prepare(
        "SELECT
          group_id,
          workspace_key,
          workspace_path,
          workspace_identity,
          task_id,
          sort_order,
          added_at,
          created_at,
          updated_at
        FROM task_group_members",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(MemberRow {
            group_id: row.get("group_id")?,
            workspace_key: row.get("workspace_key")?,
            workspace_path: row.get("workspace_path")?,
            workspace_identity: row.get("workspace_identity")?,
            task_id: row.get("task_id")?,
            sort_order: row.get("sort_order")?,
            added_at: sql_to_json(row.get_ref("added_at")?),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// task_group_view_node_orders 的一行：(node_type, node_key, sort_order)。
pub(super) fn order_rows(conn: &Connection) -> anyhow::Result<Vec<(String, String, i64)>> {
    let mut statement = conn.prepare(
        "SELECT
          node_type,
          node_key,
          sort_order,
          created_at,
          updated_at
        FROM task_group_view_node_orders",
    )?;
    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// `queryGroupedTaskViewStructure`：只读三张表，不 join tasks、不写回。
pub(crate) fn query_grouped_task_view_structure(
    conn: &Connection,
    workspace_scopes: &Value,
) -> anyhow::Result<Value> {
    let visible: HashSet<String> = normalize_workspace_keys(&scopes(workspace_scopes))
        .into_iter()
        .collect();
    let groups = visible_groups(conn, &visible)?;
    let members: Vec<Value> = members(conn)?
        .into_iter()
        .map(|row| {
            let mut member = Map::new();
            member.insert("groupId".into(), json!(row.group_id));
            member.insert("workspaceKey".into(), json!(row.workspace_key));
            member.insert("workspacePath".into(), json!(row.workspace_path));
            if let Some(identity) = resolve_row_workspace_identity(
                &row.workspace_key,
                &row.workspace_path,
                row.workspace_identity.as_deref(),
            ) {
                member.insert("workspaceIdentity".into(), json!(identity));
            }
            member.insert("taskId".into(), json!(row.task_id));
            member.insert("sortOrder".into(), json!(row.sort_order));
            member.insert("addedAt".into(), row.added_at);
            Value::Object(member)
        })
        .collect();
    let mut top_level_orders = Vec::new();
    for (node_type, node_key, sort_order) in order_rows(conn)? {
        if node_type == "group" {
            top_level_orders
                .push(json!({"type": "group", "groupId": node_key, "sortOrder": sort_order}));
            continue;
        }
        // 历史脏 node_key（旧版只写 workspace key）静默跳过。
        if let Ok(Value::Array(parts)) = serde_json::from_str::<Value>(&node_key)
            && let (Some(Value::String(workspace_key)), Some(Value::String(task_id))) =
                (parts.first(), parts.get(1))
        {
            top_level_orders.push(json!({
                "type": "task",
                "workspaceKey": workspace_key,
                "taskId": task_id,
                "sortOrder": sort_order,
            }));
        }
    }
    Ok(json!({"groups": groups, "members": members, "topLevelOrders": top_level_orders}))
}
