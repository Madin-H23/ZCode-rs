//! `applyGroupedTaskViewOrder` 与它返回的 legacy grouped 视图（`queryGroupedTaskView`）。
//!
//! UI 不采信返回值，但查询里的“补齐成员顺序、补齐顶层顺序”会写库，所以完整移植。
//! workspace 分组 bootstrap 只在 bootstrap 表为空时运行；apply 在同一次调用里先写了标记，
//! 因此这条路径上它永远不会执行，Rust 不移植它的本体（rust-host-tasks.md 第 5 节）。
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use anyhow::bail;
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, TransactionBehavior};
use serde_json::{Map, Value, json};

use super::groups::{MemberRow, members, order_rows, visible_groups};
use super::meta::resolve_workspace_key;
use super::repo::{CRON_DEFAULT_GROUP_ID, get_task_row, read_task_row, task_order_node_key};
use super::task_list::{normalize_workspace_keys, placeholders, row_to_list_item, scopes};
use crate::support::collate::locale_compare;

const ORDER_STEP: i64 = 1000;
const BOOTSTRAP_ONCE_KEY: &str = "__zcode_internal_grouped_workspace_bootstrap_once__";

/// `ZCodeGroupedTaskRef` → (workspaceKey, workspacePath, workspaceIdentity, taskId)。
struct TaskRef {
    key: String,
    path: String,
    identity: Option<String>,
    task_id: String,
}

fn task_ref(value: &Value) -> TaskRef {
    let path = value["workspacePath"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let identity = value["workspaceIdentity"].as_str().map(str::to_owned);
    TaskRef {
        key: resolve_workspace_key(&path, identity.as_deref()),
        path,
        identity,
        task_id: value["taskId"].as_str().unwrap_or_default().to_owned(),
    }
}

fn nul_key(workspace_key: &str, task_id: &str) -> String {
    format!("{workspace_key}\u{0}{task_id}")
}

/// 返回 None 表示 provider 不符、静默跳过；scope 外或不可见报 TS 的错误。
fn validate(
    conn: &Connection,
    keys: &HashSet<String>,
    task: &TaskRef,
    provider: Option<&str>,
) -> anyhow::Result<Option<()>> {
    if !keys.contains(&task.key) {
        bail!("Grouped task order 包含当前 scope 外的 task");
    }
    let row = get_task_row(conn, &task.key, &task.task_id)?;
    let Some(row) = row.filter(|row| !row.deleted && !row.archived && !row.pinned) else {
        bail!("Grouped task order 包含不可见 task");
    };
    if provider.is_some_and(|provider| row.provider.as_deref() != Some(provider)) {
        return Ok(None);
    }
    Ok(Some(()))
}

enum TopNode {
    Group(String),
    Task(TaskRef),
}

pub(crate) fn apply_grouped_task_view_order(
    conn: &mut Connection,
    now: i64,
    input: &Value,
    provider: Option<&str>,
) -> anyhow::Result<Value> {
    let keys: HashSet<String> = normalize_workspace_keys(&scopes(&input["workspaceScopes"]))
        .into_iter()
        .collect();
    let group_ids: HashSet<String> = {
        let mut statement = conn.prepare("SELECT group_id FROM task_groups")?;
        statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    let mut top_task_keys: Vec<(String, String)> = Vec::new();
    let mut top_nodes: Vec<TopNode> = Vec::new();
    for node in input["topLevelNodes"].as_array().into_iter().flatten() {
        if node["type"] == "group" {
            let group_id = node["groupId"].as_str().unwrap_or_default().to_owned();
            if !group_ids.contains(&group_id) {
                bail!("Grouped task order 包含不存在的 group");
            }
            top_nodes.push(TopNode::Group(group_id));
            continue;
        }
        let task = task_ref(&node["task"]);
        if validate(conn, &keys, &task, provider)?.is_none() {
            continue;
        }
        if !top_task_keys
            .iter()
            .any(|(k, t)| k == &task.key && t == &task.task_id)
        {
            top_task_keys.push((task.key.clone(), task.task_id.clone()));
        }
        top_nodes.push(TopNode::Task(task));
    }
    let mut grouped: HashSet<String> = HashSet::new();
    let mut visible_groups_input: Vec<(String, Vec<TaskRef>)> = Vec::new();
    for group in input["groups"].as_array().into_iter().flatten() {
        let group_id = group["groupId"].as_str().unwrap_or_default().to_owned();
        if !group_ids.contains(&group_id) {
            bail!("Grouped task order 包含不存在的 group");
        }
        let mut refs = Vec::new();
        for value in group["taskRefs"].as_array().into_iter().flatten() {
            let task = task_ref(value);
            if validate(conn, &keys, &task, provider)?.is_none() {
                continue;
            }
            if !grouped.insert(nul_key(&task.key, &task.task_id)) {
                bail!("Grouped task order 不能让同一个 task 进入多个 group");
            }
            refs.push(task);
        }
        visible_groups_input.push((group_id, refs));
    }
    // scope 内全部 task（含已删除）：本次要清掉它们的顶层顺序。
    let scoped: Vec<(String, String)> = if keys.is_empty() {
        Vec::new()
    } else {
        let sorted = normalize_workspace_keys(&scopes(&input["workspaceScopes"]));
        let mut statement = conn.prepare(&format!(
            "SELECT workspace_key, task_id
              FROM tasks
              WHERE workspace_key IN ({})",
            placeholders(sorted.len())
        ))?;
        statement
            .query_map(rusqlite::params_from_iter(sorted.iter()), |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?
    };

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO task_group_workspace_bootstraps (
          workspace_key,
          group_id,
          created_at,
          updated_at
        ) VALUES (?, NULL, ?, ?)
        ON CONFLICT(workspace_key) DO UPDATE SET
          updated_at = excluded.updated_at",
        (BOOTSTRAP_ONCE_KEY, now, now),
    )?;
    for (workspace_key, task_id) in &top_task_keys {
        if workspace_key.is_empty() || task_id.is_empty() {
            bail!("Grouped task order 顶层 task key 非法");
        }
        tx.execute(
            "DELETE FROM task_group_members
        WHERE workspace_key = ? AND task_id = ?",
            (workspace_key, task_id),
        )?;
    }
    for (group_id, refs) in &visible_groups_input {
        for (index, task) in refs.iter().enumerate() {
            tx.execute(
                "INSERT INTO task_group_members (
          group_id,
          workspace_key,
          workspace_path,
          workspace_identity,
          task_id,
          sort_order,
          added_at,
          created_at,
          updated_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(workspace_key, task_id) DO UPDATE SET
          group_id = excluded.group_id,
          workspace_path = excluded.workspace_path,
          workspace_identity = excluded.workspace_identity,
          sort_order = excluded.sort_order,
          updated_at = excluded.updated_at",
                rusqlite::params![
                    group_id,
                    task.key,
                    task.path,
                    task.identity,
                    task.task_id,
                    (index as i64 + 1) * ORDER_STEP,
                    now,
                    now,
                    now
                ],
            )?;
        }
    }
    tx.execute(
        "DELETE FROM task_group_view_node_orders WHERE node_type = 'group'",
        [],
    )?;
    for (workspace_key, task_id) in &scoped {
        if workspace_key.is_empty() || task_id.is_empty() {
            continue;
        }
        tx.execute(
            "DELETE FROM task_group_view_node_orders
        WHERE node_type = 'task' AND (node_key = ? OR node_key = ?)",
            (task_order_node_key(workspace_key, task_id), workspace_key),
        )?;
    }
    for (index, node) in top_nodes.iter().enumerate() {
        let (node_type, node_key) = match node {
            TopNode::Group(group_id) => ("group", group_id.clone()),
            TopNode::Task(task) => ("task", task_order_node_key(&task.key, &task.task_id)),
        };
        tx.execute(
            "INSERT INTO task_group_view_node_orders (
          node_type,
          node_key,
          sort_order,
          created_at,
          updated_at
        ) VALUES (?, ?, ?, ?, ?)",
            (
                node_type,
                node_key,
                (index as i64 + 1) * ORDER_STEP,
                now,
                now,
            ),
        )?;
    }
    tx.commit()?;
    query_grouped_task_view(conn, now, &input["workspaceScopes"], provider)
}

/// 视图里的一个顶层节点（`ZCodeGroupedTaskViewNode`），保留 JSON 键序：type, group/task, tasks, sortOrder。
struct ViewNode {
    map_key: String,
    node_type: &'static str,
    node_key: String,
    created_at: f64,
    body: Map<String, Value>,
    sort_order: Option<i64>,
}

impl ViewNode {
    fn into_value(self) -> Value {
        let mut body = self.body;
        if let Some(sort_order) = self.sort_order {
            body.insert("sortOrder".into(), json!(sort_order));
        }
        Value::Object(body)
    }
}

fn number(value: &Value) -> f64 {
    value.as_f64().unwrap_or(0.0)
}

/// `taskNodeKey(meta)`：按 meta 的 path/identity 重新解析 key。
fn meta_node_key(task: &Map<String, Value>) -> (String, String) {
    let key = resolve_workspace_key(
        task.get("workspacePath")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        task.get("workspaceIdentity").and_then(Value::as_str),
    );
    let task_id = task
        .get("taskId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    (key, task_id)
}

const SELECT_ACTIVE: &str = "SELECT
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
                NULL AS off_peak_task_id,
                created_at,
                updated_at,
                unread_at,
                pinned,
                archived,
                deleted,
                title_overridden,
                searchable_text,
                meta_json
              FROM tasks";

/// `queryGroupedTaskView({workspaceScopes, provider})`（includeAllWorkspaces 恒为 false）。
pub(crate) fn query_grouped_task_view(
    conn: &mut Connection,
    now: i64,
    workspace_scopes: &Value,
    provider: Option<&str>,
) -> anyhow::Result<Value> {
    let keys = normalize_workspace_keys(&scopes(workspace_scopes));
    // TS 的 active 查询不选 off_peak_task_id（rowToMeta 对 NULL 与缺列一视同仁）。
    let active = if keys.is_empty() {
        Vec::new()
    } else {
        let mut clauses = vec![
            "deleted = 0".to_owned(),
            "archived = 0".to_owned(),
            "pinned = 0".to_owned(),
            format!("workspace_key IN ({})", placeholders(keys.len())),
        ];
        let mut args: Vec<SqlValue> = keys.iter().cloned().map(SqlValue::Text).collect();
        if let Some(provider) = provider {
            clauses.push("provider = ?".into());
            args.push(SqlValue::Text(provider.to_owned()));
        }
        let mut statement = conn.prepare(&format!(
            "{SELECT_ACTIVE}\n              WHERE {}",
            clauses.join(" AND ")
        ))?;
        statement
            .query_map(rusqlite::params_from_iter(args.iter()), read_task_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let visible: HashSet<String> = keys.iter().cloned().collect();
    let groups = visible_groups(conn, &visible)?;
    let member_rows = members(conn)?;
    let orders: HashMap<String, i64> = order_rows(conn)?
        .into_iter()
        .map(|(node_type, node_key, sort_order)| (format!("{node_type}:{node_key}"), sort_order))
        .collect();
    let mut member_by_key: HashMap<String, MemberRow> = HashMap::new();
    let mut members_by_group: Vec<(String, Vec<String>)> = Vec::new();
    for member in member_rows {
        let key = nul_key(&member.workspace_key, &member.task_id);
        match members_by_group
            .iter_mut()
            .find(|(id, _)| id == &member.group_id)
        {
            Some((_, list)) => list.push(key.clone()),
            None => members_by_group.push((member.group_id.clone(), vec![key.clone()])),
        }
        member_by_key.insert(key, member);
    }
    let mut active_tasks: Vec<(String, Map<String, Value>)> = Vec::new();
    for row in &active {
        let key = nul_key(&row.workspace_key, &row.task_id);
        // 主键唯一，同一个 key 不会出现两次。
        active_tasks.push((key, row_to_list_item(row, None)));
    }
    let active_by_key: HashMap<&str, &Map<String, Value>> = active_tasks
        .iter()
        .map(|(key, item)| (key.as_str(), item))
        .collect();

    let mut grouped_visible: HashSet<String> = HashSet::new();
    let mut nodes: Vec<ViewNode> = Vec::new();
    for group in groups {
        let group_id = group["id"].as_str().unwrap_or_default().to_owned();
        let mut tasks: Vec<Map<String, Value>> = members_by_group
            .iter()
            .find(|(id, _)| id == &group_id)
            .map(|(_, keys)| {
                keys.iter()
                    .filter_map(|key| active_by_key.get(key.as_str()).map(|task| (*task).clone()))
                    .collect()
            })
            .unwrap_or_default();
        let node_key_of = |task: &Map<String, Value>| {
            let (key, task_id) = meta_node_key(task);
            nul_key(&key, &task_id)
        };
        if group_id == CRON_DEFAULT_GROUP_ID {
            tasks.sort_by(|left, right| {
                number(&right["createdAt"])
                    .partial_cmp(&number(&left["createdAt"]))
                    .unwrap_or(Ordering::Equal)
                    .then_with(|| locale_compare(&node_key_of(right), &node_key_of(left)))
            });
        } else {
            normalize_member_orders(conn, now, &group_id, &tasks, &mut member_by_key)?;
            let order_of = |task: &Map<String, Value>| {
                member_by_key
                    .get(&node_key_of(task))
                    .and_then(|member| member.sort_order)
                    .unwrap_or(0)
            };
            tasks.sort_by(|left, right| {
                order_of(left)
                    .cmp(&order_of(right))
                    .then_with(|| locale_compare(&node_key_of(left), &node_key_of(right)))
            });
        }
        for task in &tasks {
            grouped_visible.insert(node_key_of(task));
        }
        let mut body = Map::new();
        body.insert("type".into(), json!("group"));
        let created_at = number(&group["createdAt"]);
        body.insert("group".into(), group);
        body.insert(
            "tasks".into(),
            Value::Array(tasks.into_iter().map(Value::Object).collect()),
        );
        nodes.push(ViewNode {
            map_key: format!("group:{group_id}"),
            node_type: "group",
            sort_order: orders.get(&format!("group:{group_id}")).copied(),
            node_key: group_id,
            created_at,
            body,
        });
    }
    for (_, task) in &active_tasks {
        let (key, task_id) = meta_node_key(task);
        let node_key = nul_key(&key, &task_id);
        if member_by_key.contains_key(&node_key) || grouped_visible.contains(&node_key) {
            continue;
        }
        let order_key = task_order_node_key(&key, &task_id);
        let mut body = Map::new();
        body.insert("type".into(), json!("task"));
        body.insert("task".into(), Value::Object(task.clone()));
        nodes.push(ViewNode {
            map_key: format!("task:{order_key}"),
            node_type: "task",
            sort_order: orders.get(&format!("task:{order_key}")).copied(),
            node_key: order_key,
            created_at: number(&task["createdAt"]),
            body,
        });
    }
    normalize_top_orders(conn, now, &mut nodes)?;
    nodes.sort_by(|left, right| {
        left.sort_order
            .unwrap_or(0)
            .cmp(&right.sort_order.unwrap_or(0))
            .then_with(|| locale_compare(&left.map_key, &right.map_key))
    });
    Ok(json!({"nodes": nodes.into_iter().map(ViewNode::into_value).collect::<Vec<_>>()}))
}

/// `normalizeGroupMemberOrders`：sort_order 为 NULL 的成员按加入时间倒序补到组内最后。
fn normalize_member_orders(
    conn: &mut Connection,
    now: i64,
    group_id: &str,
    tasks: &[Map<String, Value>],
    member_by_key: &mut HashMap<String, MemberRow>,
) -> anyhow::Result<()> {
    let mut missing: Vec<(String, f64)> = tasks
        .iter()
        .filter_map(|task| {
            let (key, task_id) = meta_node_key(task);
            let node_key = nul_key(&key, &task_id);
            let member = member_by_key.get(&node_key)?;
            member.sort_order.is_none().then(|| {
                let added = member
                    .added_at
                    .as_f64()
                    .unwrap_or_else(|| number(&task["createdAt"]));
                (node_key, added)
            })
        })
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    missing.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| locale_compare(&left.0, &right.0))
    });
    let max: Option<i64> = conn.query_row(
        "SELECT MAX(sort_order) AS max_sort_order
        FROM task_group_members
        WHERE group_id = ?",
        [group_id],
        |row| row.get(0),
    )?;
    let mut next = max.unwrap_or(0);
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for (node_key, _) in missing {
        let Some(member) = member_by_key.get_mut(&node_key) else {
            continue;
        };
        next += ORDER_STEP;
        tx.execute(
            "UPDATE task_group_members
      SET sort_order = ?, updated_at = ?
      WHERE workspace_key = ? AND task_id = ?",
            (next, now, &member.workspace_key, &member.task_id),
        )?;
        member.sort_order = Some(next);
    }
    tx.commit()?;
    Ok(())
}

/// `normalizeGroupedTopNodeOrders`：缺顺序的顶层节点按创建时间倒序补到最后。
fn normalize_top_orders(
    conn: &mut Connection,
    now: i64,
    nodes: &mut [ViewNode],
) -> anyhow::Result<()> {
    let mut missing: Vec<usize> = (0..nodes.len())
        .filter(|&i| nodes[i].sort_order.is_none())
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    missing.sort_by(|&left, &right| {
        let (left, right) = (&nodes[left], &nodes[right]);
        right
            .created_at
            .partial_cmp(&left.created_at)
            .unwrap_or(Ordering::Equal)
            .then_with(|| locale_compare(&left.map_key, &right.map_key))
    });
    let max: Option<i64> = conn.query_row(
        "SELECT MAX(sort_order) AS max_sort_order
        FROM task_group_view_node_orders",
        [],
        |row| row.get(0),
    )?;
    let mut next = max.unwrap_or(0);
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for index in missing {
        next += ORDER_STEP;
        let node = &mut nodes[index];
        tx.execute(
            "INSERT INTO task_group_view_node_orders (
        node_type,
        node_key,
        sort_order,
        created_at,
        updated_at
      ) VALUES (?, ?, ?, ?, ?)",
            (node.node_type, &node.node_key, next, now, now),
        )?;
        node.sort_order = Some(next);
    }
    tx.commit()?;
    Ok(())
}
