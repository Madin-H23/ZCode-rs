//! 分组、grouped 视图、搜索与归档的 TS 对拍（`fixtures/parity-groups.json`，由
//! scripts/rust-host/tasks-index-groups-fixtures.ts 用真实 TaskIndexRepo 生成）。
//! TS 的随机分组 id 在 fixture 里被替换成 `<group-n>`；Rust 直接用这个占位符作为 id。
use rusqlite::Connection;
use rusqlite::types::Value as SqlValue;
use serde_json::{Map, Value, json};
use zcode_kit::js::json as js_json;

use super::meta::resolve_workspace_key;
use super::repo::sql_to_json;
use super::startup::{self, LOCK_WAIT};
use super::{archive, grouped_view, groups, task_list};

const PARITY: &str = include_str!("fixtures/parity-groups.json");

fn text(value: &Value) -> String {
    js_json::stringify(value)
}

/// `group: n` → `groupId: "<group-n>"`（与 TS 的 resolveGroups 相同）。
fn resolve_groups(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(resolve_groups).collect()),
        Value::Object(map) => {
            let mut result = Map::new();
            for (key, entry) in map {
                match (key.as_str(), entry.as_u64()) {
                    ("group", Some(index)) => {
                        result.insert("groupId".into(), json!(format!("<group-{index}>")));
                    }
                    _ => {
                        result.insert(key.clone(), resolve_groups(entry));
                    }
                }
            }
            Value::Object(result)
        }
        other => other.clone(),
    }
}

fn insert_row(conn: &Connection, row: &Map<String, Value>) -> anyhow::Result<()> {
    let columns: Vec<&str> = row.keys().map(String::as_str).collect();
    let values = row.values().map(|value| match value {
        Value::String(s) => SqlValue::Text(s.clone()),
        Value::Number(n) => n.as_i64().map_or(SqlValue::Null, SqlValue::Integer),
        _ => SqlValue::Null,
    });
    conn.execute(
        &format!(
            "INSERT INTO tasks ({}) VALUES ({})",
            columns.join(", "),
            vec!["?"; columns.len()].join(", ")
        ),
        rusqlite::params_from_iter(values),
    )?;
    Ok(())
}

fn target_key(target: &Value) -> String {
    resolve_workspace_key(
        target["workspacePath"].as_str().unwrap_or_default(),
        target["workspaceIdentity"].as_str(),
    )
}

fn apply_op(
    conn: &mut Connection,
    op: &Value,
    now: i64,
    created: &mut usize,
) -> anyhow::Result<Value> {
    let resolved = resolve_groups(op);
    let group_id = resolved["groupId"].as_str().unwrap_or_default().to_owned();
    Ok(match op["op"].as_str().unwrap_or_default() {
        "insertRow" => {
            insert_row(conn, op["row"].as_object().expect("row"))?;
            Value::Null
        }
        "exec" => {
            conn.execute_batch(op["sql"].as_str().unwrap_or_default())?;
            Value::Null
        }
        "createGroup" => {
            let id = format!("<group-{created}>");
            *created += 1;
            groups::create_task_group(
                conn,
                now,
                &id,
                op["params"]["title"].as_str(),
                op["params"]["color"].as_str(),
            )?
        }
        "renameGroup" => {
            groups::rename_task_group(conn, now, &group_id, op["title"].as_str().unwrap_or(""))?
        }
        "updateColor" => {
            groups::update_task_group_color(conn, now, &group_id, op["color"].as_str())?
        }
        "deleteGroup" => {
            groups::delete_task_group(conn, &group_id)?;
            Value::Null
        }
        "structure" => groups::query_grouped_task_view_structure(conn, &op["scopes"])?,
        "apply" => {
            grouped_view::apply_grouped_task_view_order(conn, now, &resolved["input"], Some("glm"))?
        }
        "list" => task_list::query_task_list(conn, &op["query"], op["provider"].as_str())?,
        "archiveStale" => Value::Array(archive::archive_stale_tasks(
            conn,
            now,
            &target_key(&op["target"]),
            op["olderThanDays"].as_f64().unwrap_or_default(),
            op["provider"].as_str(),
        )?),
        "deleteArchived" => archive::delete_archived_task(
            conn,
            &target_key(&op["target"]),
            op["target"]["taskId"].as_str().unwrap_or_default(),
        )?
        .unwrap_or(Value::Null),
        other => panic!("unknown op {other}"),
    })
}

fn dump(conn: &Connection, sql: &str) -> Value {
    let mut statement = conn.prepare(sql).expect("dump sql");
    let names: Vec<String> = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let rows = statement
        .query_map([], |row| {
            let mut map = Map::new();
            for (index, name) in names.iter().enumerate() {
                map.insert(name.clone(), sql_to_json(row.get_ref(index)?));
            }
            Ok(Value::Object(map))
        })
        .expect("dump query")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("dump rows");
    Value::Array(rows)
}

#[test]
fn groups_lists_and_archives_match_typescript() {
    let fixture: Value = serde_json::from_str(PARITY).expect("fixture");
    let base_now = fixture["baseNow"].as_i64().expect("baseNow");
    let mut failures = Vec::new();
    for scenario in fixture["scenarios"].as_array().expect("scenarios") {
        let name = scenario["name"].as_str().unwrap_or_default();
        let dir = tempfile::tempdir().expect("tempdir");
        let mut conn = startup::prepare(
            &dir.path().join(name).join("tasks-index.sqlite"),
            &|_| {},
            &|| base_now,
            LOCK_WAIT,
        )
        .expect("prepare");
        let mut now = base_now;
        let mut created = 0;
        let ops = scenario["ops"].as_array().expect("ops");
        let expected = scenario["results"].as_array().expect("results");
        assert_eq!(ops.len(), expected.len(), "{name}: fixture shape");
        for (index, (op, expected)) in ops.iter().zip(expected).enumerate() {
            if let Some(at) = op["now"].as_i64() {
                now = at;
            }
            let actual = apply_op(&mut conn, op, now, &mut created)
                .unwrap_or_else(|error| json!({"error": format!("{error}")}));
            if text(&actual) != text(expected) {
                failures.push(format!(
                    "{name} #{index} {}:\n  expected {}\n  actual   {}",
                    op["op"],
                    text(expected),
                    text(&actual)
                ));
            }
        }
        for entry in scenario["dumps"].as_array().expect("dumps") {
            let actual = dump(&conn, entry["sql"].as_str().unwrap_or_default());
            if text(&actual) != text(&entry["rows"]) {
                failures.push(format!(
                    "{name} / {}:\n  expected {}\n  actual   {}",
                    entry["table"],
                    text(&entry["rows"]),
                    text(&actual)
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
