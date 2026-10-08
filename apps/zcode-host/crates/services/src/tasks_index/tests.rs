//! tasks-index 测试：与 TS 对拍（`fixtures/parity.json` 由 scripts/rust-host/tasks-index-fixtures.ts
//! 用真实 TS 实现生成）、迁移账本兼容、启动锁等待、逐条语义与多句柄并发。
//! 只使用 tempfile 临时目录，不读写开发者的 `~/.zcode`。
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;
use rusqlite::types::Value as SqlValue;
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};
use zcode_cli_domain::js_json;

use super::meta::resolve_workspace_key;
use super::repo::{self, GroupedAdmission, ListFilter, StatePatch, sql_to_json};
use super::schema::{
    AUTOMATION_SCHEMA, BOUND_INDEX, COLUMNS, INDEXES, MIGRATIONS, MIGRATIONS_FIXTURE,
    OFF_PEAK_SCHEMA, OFFICIAL_GLM_SELECTION_MIGRATION_SQL, PROVIDER_SELECTION_VERSIONS,
    SCHEDULED_COUNT_BACKFILL, TASK_INDEX_SCHEMA,
};
use super::startup::{self, LOCK_WAIT, MigrationFacts};
use super::{
    MigrationKind, StartupErrorCode, StoragePhase, StorageProgress, TasksIndex, input_meta,
    input_patch, set_unread,
};

const PARITY: &str = include_str!("fixtures/parity.json");

fn parity() -> Value {
    serde_json::from_str(PARITY).expect("parity fixture")
}

/// 键序敏感的比较：两边都按 `JSON.stringify` 输出。
fn text(value: &Value) -> String {
    js_json::stringify(value)
}

fn db_path(dir: &Path) -> PathBuf {
    dir.join(".zcode").join("v2").join("tasks-index.sqlite")
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

fn compare_dumps(conn: &Connection, scenario: &Value, failures: &mut Vec<String>) {
    let name = scenario["name"].as_str().unwrap_or_default();
    for entry in scenario["dumps"].as_array().into_iter().flatten() {
        let actual = dump(conn, entry["sql"].as_str().unwrap_or_default());
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

/// 与 TS `runTasksDatabaseMigrations(db)`（自开事务）等价的建库。
fn migrate_raw(conn: &Connection, now: i64) {
    let mut facts = MigrationFacts {
        kind: startup::inspect_migration_kind(conn).expect("inspect"),
        executed_count: 0,
        committed_count: 0,
        last_applied_migration_id: None,
    };
    conn.execute_batch("BEGIN IMMEDIATE").expect("begin");
    startup::run_migrations(conn, &mut facts, &|_| {}, &|| now).expect("migrate");
}

fn progress_json(progress: &StorageProgress) -> Value {
    let mut value = serde_json::to_value(progress).expect("progress json");
    if let Some(map) = value.as_object_mut() {
        map.remove("completed");
        map.remove("total");
    }
    value
}

// ---- 账本与 schema ----

#[test]
fn migration_fixture_matches_rust_ddl() {
    let fixture: Value = serde_json::from_str(MIGRATIONS_FIXTURE).expect("fixture");
    let columns: Vec<Value> = COLUMNS
        .iter()
        .map(|(table, column, definition)| json!([table, column, definition]))
        .collect();
    let rust_inputs = [
        json!([
            TASK_INDEX_SCHEMA,
            AUTOMATION_SCHEMA,
            OFF_PEAK_SCHEMA,
            columns,
            INDEXES,
            BOUND_INDEX,
            SCHEDULED_COUNT_BACKFILL
        ]),
        json!(PROVIDER_SELECTION_VERSIONS),
        json!([OFFICIAL_GLM_SELECTION_MIGRATION_SQL]),
    ];
    let migrations = fixture["migrations"].as_array().expect("migrations");
    assert_eq!(
        migrations.len(),
        rust_inputs.len(),
        "every TS migration needs a Rust port"
    );
    for (entry, rust_input) in migrations.iter().zip(&rust_inputs) {
        let id = entry["id"].as_str().unwrap_or_default();
        let checksum_input = text(&entry["checksumInput"]);
        assert_eq!(
            text(rust_input),
            checksum_input,
            "{id}: Rust DDL differs from TS"
        );
        let digest = Sha256::digest(checksum_input.as_bytes());
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, entry["checksum"].as_str().unwrap_or_default(), "{id}");
    }
    let ids: Vec<&str> = MIGRATIONS.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "0001_adopt_task_schema",
            "0002_provider_selection",
            "0003_official_glm_selection"
        ]
    );
}

#[test]
fn startup_matches_typescript() {
    let fixture = parity();
    let base_now = fixture["baseNow"].as_i64().unwrap_or_default();
    let prepare_now = fixture["prepareNow"].as_i64().unwrap_or_default();
    let mut failures = Vec::new();
    for scenario in fixture["startup"].as_array().into_iter().flatten() {
        let name = scenario["name"].as_str().unwrap_or_default();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir
            .path()
            .join(name)
            .join("nested")
            .join(".zcode")
            .join("v2")
            .join("tasks-index.sqlite");
        let setup = scenario["setup"].as_array().cloned().unwrap_or_default();
        let migrated = scenario["base"] == "migrated";
        if migrated || !setup.is_empty() {
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            let conn = Connection::open(&path).expect("open");
            if migrated {
                migrate_raw(&conn, base_now);
            }
            for statement in &setup {
                conn.execute_batch(statement.as_str().unwrap_or_default())
                    .expect("setup");
            }
            for id in scenario["resetLedger"].as_array().into_iter().flatten() {
                conn.execute(
                    "DELETE FROM tasks_schema_migration WHERE id = ?",
                    [id.as_str().unwrap_or_default()],
                )
                .expect("reset ledger");
            }
        }
        let events = Mutex::new(Vec::new());
        let conn = startup::prepare(
            &path,
            &|event| events.lock().expect("events").push(event),
            &|| prepare_now,
            LOCK_WAIT,
        )
        .unwrap_or_else(|error| panic!("{name}: {error}"));
        let actual: Vec<Value> = events
            .into_inner()
            .expect("events")
            .iter()
            .map(progress_json)
            .collect();
        if Value::Array(actual.clone()) != scenario["progress"] {
            failures.push(format!(
                "{name} progress:\n  expected {}\n  actual   {}",
                scenario["progress"],
                Value::Array(actual)
            ));
        }
        compare_dumps(&conn, scenario, &mut failures);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ---- 业务操作对拍 ----

fn target_key(target: &Value) -> (String, String) {
    (
        resolve_workspace_key(
            target["workspacePath"].as_str().unwrap_or_default(),
            target["workspaceIdentity"].as_str(),
        ),
        target["taskId"].as_str().unwrap_or_default().to_owned(),
    )
}

fn insert_row(conn: &Connection, row: &Map<String, Value>) -> anyhow::Result<()> {
    let columns: Vec<&str> = row.keys().map(String::as_str).collect();
    let placeholders = vec!["?"; columns.len()].join(", ");
    let values = row.values().map(|value| match value {
        Value::String(s) => SqlValue::Text(s.clone()),
        Value::Number(n) => n.as_i64().map_or(SqlValue::Null, SqlValue::Integer),
        _ => SqlValue::Null,
    });
    conn.execute(
        &format!(
            "INSERT INTO tasks ({}) VALUES ({placeholders})",
            columns.join(", ")
        ),
        rusqlite::params_from_iter(values),
    )?;
    Ok(())
}

fn meta_or_null(meta: Option<super::meta::JsObject>) -> Value {
    meta.map_or(Value::Null, |meta| meta.to_value())
}

fn apply_op(conn: &mut Connection, op: &Value, clock: &mut i64) -> anyhow::Result<Value> {
    if let Some(now) = op["now"].as_i64() {
        *clock = now;
    }
    let now = *clock;
    let (key, task_id) = target_key(&op["target"]);
    let state = |conn: &mut Connection, patch: StatePatch| -> anyhow::Result<Value> {
        Ok(meta_or_null(repo::update_task_state(
            conn, &key, &task_id, &patch,
        )?))
    };
    Ok(match op["op"].as_str().unwrap_or_default() {
        "insertRow" => {
            insert_row(conn, op["row"].as_object().expect("row"))?;
            Value::Null
        }
        "seed" => {
            let meta = input_meta(op["meta"].clone())?;
            let mut results = repo::seed_task_metas_if_missing(conn, &[meta])?;
            results.remove(0).0.to_value()
        }
        name @ ("syncTop" | "sync") => {
            let meta = input_meta(op["meta"].clone())?;
            let admission = if name == "syncTop" {
                GroupedAdmission::InitializeAtTop
            } else {
                GroupedAdmission::Skip
            };
            let (meta, initialized) =
                repo::sync_task_meta(conn, &meta, op["searchableText"].as_str(), admission, now)?;
            if name == "syncTop" {
                json!({ "meta": meta.to_value(), "initializedGroupedOrder": initialized })
            } else {
                meta.to_value()
            }
        }
        "agentPatch" => {
            let patch = input_patch(op["patch"].clone())?;
            meta_or_null(repo::apply_agent_patch(conn, &key, &task_id, &patch)?)
        }
        "unread" => {
            let outcome = set_unread(
                conn,
                &key,
                &task_id,
                op["unread"].as_bool().unwrap_or_default(),
                op["expectedUnreadAt"].as_i64(),
                now,
            )?;
            match outcome.meta {
                Some(meta) => json!({ "meta": meta, "changed": outcome.changed }),
                None => Value::Null,
            }
        }
        "pinned" => state(
            conn,
            StatePatch {
                pinned: op["value"].as_bool(),
                ..StatePatch::default()
            },
        )?,
        "archived" => state(
            conn,
            StatePatch {
                archived: op["value"].as_bool(),
                ..StatePatch::default()
            },
        )?,
        "delete" => state(
            conn,
            StatePatch {
                deleted: Some(true),
                ..StatePatch::default()
            },
        )?,
        "rename" => state(
            conn,
            StatePatch {
                title: op["title"].as_str().map(str::to_owned),
                title_overridden: Some(true),
                updated_at: Some(now),
                ..StatePatch::default()
            },
        )?,
        "get" => meta_or_null(repo::get_task_meta(conn, &key, &task_id)?),
        "list" => {
            let filter = &op["filter"];
            let workspace_key = filter["workspacePath"]
                .as_str()
                .map(|path| resolve_workspace_key(path, filter["workspaceIdentity"].as_str()));
            let metas = repo::list_task_metas(
                conn,
                &ListFilter {
                    workspace_key,
                    include_deleted: filter["includeDeleted"].as_bool().unwrap_or(false),
                    provider: filter["provider"].as_str().map(str::to_owned),
                    pinned: filter["pinned"].as_bool(),
                    archived: filter["archived"].as_bool(),
                },
            )?;
            Value::Array(metas.iter().map(|meta| meta.to_value()).collect())
        }
        "listDeleted" => json!(repo::list_deleted_task_ids(
            conn,
            &key,
            op["provider"].as_str()
        )?),
        other => anyhow::bail!("unknown op {other}"),
    })
}

#[test]
fn operations_match_typescript() {
    let fixture = parity();
    let base_now = fixture["baseNow"].as_i64().unwrap_or_default();
    let mut failures = Vec::new();
    for scenario in fixture["operations"].as_array().into_iter().flatten() {
        let name = scenario["name"].as_str().unwrap_or_default();
        let dir = tempfile::tempdir().expect("tempdir");
        let mut conn = startup::prepare(
            &dir.path().join(name).join("tasks-index.sqlite"),
            &|_| {},
            &|| base_now,
            LOCK_WAIT,
        )
        .expect("prepare");
        let mut clock = base_now;
        let ops = scenario["ops"].as_array().cloned().unwrap_or_default();
        let expected = scenario["results"].as_array().cloned().unwrap_or_default();
        assert!(
            !ops.is_empty() && ops.len() == expected.len(),
            "{name}: fixture shape"
        );
        assert!(
            scenario["dumps"].as_array().is_some_and(|d| !d.is_empty()),
            "{name}: dumps"
        );
        for (index, (op, expected)) in ops.iter().zip(&expected).enumerate() {
            let actual = apply_op(&mut conn, op, &mut clock)
                .unwrap_or_else(|error| panic!("{name} #{index} {}: {error:#}", op["op"]));
            if text(&actual) != text(expected) {
                failures.push(format!(
                    "{name} #{index} {}:\n  expected {}\n  actual   {}",
                    op["op"],
                    text(expected),
                    text(&actual)
                ));
            }
        }
        compare_dumps(&conn, scenario, &mut failures);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ---- 启动：TS 库、篡改、锁等待 ----

type Events = Arc<Mutex<Vec<StorageProgress>>>;

fn recorder() -> (Events, impl Fn(StorageProgress) + Send + Sync + 'static) {
    let events: Events = Arc::default();
    let sink = events.clone();
    (events, move |event| {
        sink.lock().expect("events").push(event)
    })
}

fn phases(events: &Events) -> Vec<StoragePhase> {
    events
        .lock()
        .expect("events")
        .iter()
        .map(|event| event.phase)
        .collect()
}

/// 用 TS 准备出的 `sqlite_master` 与账本逐条重建一个“TS 库”。
fn build_typescript_prepared(path: &Path) -> (Value, Value) {
    let fixture = parity();
    let fresh = fixture["startup"]
        .as_array()
        .and_then(|all| all.iter().find(|s| s["name"] == "fresh"))
        .cloned()
        .expect("fresh scenario");
    let table = |name: &str| {
        fresh["dumps"]
            .as_array()
            .and_then(|dumps| dumps.iter().find(|d| d["table"] == name))
            .map(|d| d["rows"].clone())
            .expect("dump")
    };
    let (schema, ledger) = (table("sqlite_master"), table("tasks_schema_migration"));
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let conn = Connection::open(path).expect("open");
    for entry in schema.as_array().into_iter().flatten() {
        if let Some(sql) = entry["sql"].as_str() {
            conn.execute_batch(sql).expect("schema");
        }
    }
    for row in ledger.as_array().into_iter().flatten() {
        conn.execute(
            "INSERT INTO tasks_schema_migration VALUES(?,?,?)",
            (
                row["id"].as_str(),
                row["checksum"].as_str(),
                row["time_applied"].as_i64(),
            ),
        )
        .expect("ledger");
    }
    conn.execute_batch("PRAGMA journal_mode = WAL")
        .expect("wal");
    (schema, ledger)
}

#[tokio::test]
async fn opens_typescript_prepared_database_without_migrating() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = db_path(dir.path());
    let (schema, ledger) = build_typescript_prepared(&path);
    let (events, sink) = recorder();
    let index = TasksIndex::open(path.clone(), sink).await.expect("open");
    let last = events
        .lock()
        .expect("events")
        .last()
        .cloned()
        .expect("events");
    assert_eq!(last.phase, StoragePhase::Ready);
    let facts = last.migration.expect("facts");
    assert_eq!(facts.kind, MigrationKind::None);
    assert_eq!((facts.executed_count, facts.committed_count), (0, 0));
    assert!(!phases(&events).contains(&StoragePhase::Migrating));
    let conn = Connection::open(&path).expect("open");
    assert_eq!(
        text(&dump(
            &conn,
            "SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY rowid"
        )),
        text(&schema)
    );
    // time_applied 仍是 TS 写入的值：没有重新执行迁移。
    assert_eq!(
        text(&dump(
            &conn,
            "SELECT * FROM tasks_schema_migration ORDER BY id"
        )),
        text(&ledger)
    );
    drop(index);
}

#[tokio::test]
async fn tampered_checksum_fails_like_typescript() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = db_path(dir.path());
    drop(TasksIndex::open(path.clone(), |_| {}).await.expect("open"));
    let conn = Connection::open(&path).expect("open");
    conn.execute(
        "UPDATE tasks_schema_migration SET checksum = 'tampered' WHERE id = '0002_provider_selection'",
        [],
    )
    .expect("tamper");
    let (events, sink) = recorder();
    let error = TasksIndex::open(path.clone(), sink)
        .await
        .err()
        .expect("checksum mismatch");
    assert_eq!(
        error.to_string(),
        "Task database migration checksum mismatch: 0002_provider_selection"
    );
    assert_eq!(error.code(), StartupErrorCode::ChecksumMismatch);
    assert_eq!(error.code().as_str(), "checksum_mismatch");
    assert_eq!(error.migration_id(), Some("0002_provider_selection"));
    assert_eq!(phases(&events).last(), Some(&StoragePhase::Failed));
    // 账本原样保留，不会被“修复”。
    let checksum: String = conn
        .query_row(
            "SELECT checksum FROM tasks_schema_migration WHERE id = '0002_provider_selection'",
            [],
            |row| row.get(0),
        )
        .expect("ledger");
    assert_eq!(checksum, "tampered");
}

#[tokio::test]
async fn waits_for_the_write_lock_then_becomes_ready() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = db_path(dir.path());
    drop(TasksIndex::open(path.clone(), |_| {}).await.expect("open"));
    let holder = Connection::open(&path).expect("open");
    holder.execute_batch("BEGIN IMMEDIATE").expect("lock");
    let (events, sink) = recorder();
    let opening = tokio::spawn(TasksIndex::open(path.clone(), sink));
    for _ in 0..200 {
        if phases(&events).contains(&StoragePhase::WaitingForLock) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(phases(&events).contains(&StoragePhase::WaitingForLock));
    assert!(!opening.is_finished());
    holder.execute_batch("COMMIT").expect("unlock");
    opening.await.expect("join").expect("open after lock");
    assert_eq!(phases(&events).last(), Some(&StoragePhase::Ready));
}

#[tokio::test]
async fn lock_wait_expiry_is_a_lock_timeout() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = db_path(dir.path());
    drop(TasksIndex::open(path.clone(), |_| {}).await.expect("open"));
    let holder = Connection::open(&path).expect("open");
    holder.execute_batch("BEGIN IMMEDIATE").expect("lock");
    let error = TasksIndex::open_with(
        path,
        Arc::new(|_| {}),
        Arc::new(|| 0),
        Duration::from_millis(300),
    )
    .await
    .err()
    .expect("timeout");
    assert_eq!(error.code(), StartupErrorCode::LockTimeout);
    assert!(error.to_string().contains("Task storage lock wait expired"));
    assert_eq!(error.migration().map(|m| m.kind), Some(MigrationKind::None));
}

// ---- 语义（公开 API） ----

const W: &str = "/tmp/zcode-test/workspace";

fn meta(task_id: &str, extra: Value) -> Value {
    let mut meta = json!({
        "taskId": task_id,
        "traceId": format!("trace-{task_id}"),
        "title": format!("Task {task_id}"),
        "workspacePath": W,
        "createdAt": 1000,
        "updatedAt": 1000,
        "mode": "build",
        "provider": "glm",
    });
    if let (Some(base), Value::Object(extra)) = (meta.as_object_mut(), extra) {
        base.extend(extra);
    }
    meta
}

async fn open_temp(dir: &Path) -> TasksIndex {
    TasksIndex::open(db_path(dir), |_| {}).await.expect("open")
}

fn raw(index: &TasksIndex) -> Connection {
    Connection::open(index.path()).expect("open raw")
}

#[tokio::test]
async fn seed_inserts_only_missing_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let index = open_temp(dir.path()).await;
    let seeded = index
        .seed_task_metas_if_missing(vec![meta("a", json!({})), meta("b", json!({}))])
        .await
        .expect("seed");
    assert_eq!(seeded, 2);
    index.set_pinned(W, "a", true).await.expect("pin");
    index.mark_deleted(W, "b").await.expect("delete");
    let seeded = index
        .seed_task_metas_if_missing(vec![
            meta("a", json!({"title": "other"})),
            meta("b", json!({})),
            meta("c", json!({})),
        ])
        .await
        .expect("seed");
    assert_eq!(seeded, 1);
    let a = index.get_task_meta(W, "a").await.expect("get").expect("a");
    assert_eq!(a["title"], "Task a");
    assert_eq!(index.get_task_meta(W, "b").await.expect("get"), None);
    assert!(
        index
            .seed_task_metas_if_missing(vec![json!({"taskId": "x"})])
            .await
            .is_err()
    );
}

#[tokio::test]
async fn sync_merges_like_typescript() {
    let dir = tempfile::tempdir().expect("tempdir");
    let index = open_temp(dir.path()).await;
    let first = index
        .sync_task_meta_at_grouped_top(
            meta(
                "t",
                json!({"cronAutomationId": "auto", "migrationSource": "claudeCode"}),
            ),
            Some("body text".into()),
        )
        .await
        .expect("sync");
    assert!(
        !first.initialized_grouped_order,
        "cron membership owns the row"
    );
    index
        .apply_agent_patch(W, "t", json!({"status": "completed", "updatedAt": 5000}))
        .await
        .expect("patch");
    index.rename(W, "t", "Manual".into()).await.expect("rename");
    index.set_pinned(W, "t", true).await.expect("pin");
    let merged = index
        .sync_task_meta(
            meta(
                "t",
                json!({"title": "Agent", "status": "running", "updatedAt": 10}),
            ),
            None,
        )
        .await
        .expect("sync");
    assert_eq!(merged["title"], "Manual");
    assert_eq!(merged["status"], "completed");
    assert_eq!(merged["cronAutomationId"], "auto");
    assert_eq!(merged["migrationSource"], "claudeCode");
    assert!(merged["updatedAt"].as_i64() > Some(5000));
    let conn = raw(&index);
    let (pinned, text): (i64, String) = conn
        .query_row(
            "SELECT pinned, searchable_text FROM tasks WHERE task_id = 't'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("row");
    assert_eq!((pinned, text.as_str()), (1, "body text"));
}

#[tokio::test]
async fn grouped_top_order_is_initialized_once_and_only_at_top() {
    let dir = tempfile::tempdir().expect("tempdir");
    let index = open_temp(dir.path()).await;
    let plain = index
        .sync_task_meta(meta("plain", json!({})), None)
        .await
        .expect("sync");
    assert_eq!(plain["taskId"], "plain");
    let a = index
        .sync_task_meta_at_grouped_top(meta("a", json!({})), None)
        .await
        .expect("a");
    let again = index
        .sync_task_meta_at_grouped_top(meta("a", json!({})), None)
        .await
        .expect("a again");
    let b = index
        .sync_task_meta_at_grouped_top(meta("b", json!({})), None)
        .await
        .expect("b");
    assert!(a.initialized_grouped_order && b.initialized_grouped_order);
    assert!(!again.initialized_grouped_order);
    let orders = dump(
        &raw(&index),
        "SELECT node_key, sort_order FROM task_group_view_node_orders ORDER BY sort_order",
    );
    assert_eq!(
        orders,
        json!([
            {"node_key": format!("[\"{W}\",\"b\"]"), "sort_order": 0},
            {"node_key": format!("[\"{W}\",\"a\"]"), "sort_order": 1000},
        ])
    );
}

#[tokio::test]
async fn agent_patch_null_clears_and_absent_keeps() {
    let dir = tempfile::tempdir().expect("tempdir");
    let index = open_temp(dir.path()).await;
    let goal = json!({
        "sessionID": "s", "targetID": "g", "objective": "ship", "status": "active",
        "tokenBudget": null, "tokensUsed": 0, "timeUsedSeconds": 0,
        "time": {"created": 1, "updated": 2}
    });
    index
        .sync_task_meta(
            meta(
                "t",
                json!({"status": "error", "lastError": {"message": "boom"}, "target": goal}),
            ),
            None,
        )
        .await
        .expect("sync");
    // 键缺失：保留。
    let kept = index
        .apply_agent_patch(W, "t", json!({"updatedAt": 2000}))
        .await
        .expect("patch")
        .expect("row");
    assert_eq!(kept["lastError"]["message"], "boom");
    assert_eq!(kept["status"], "error");
    assert_eq!(kept["target"]["targetID"], "g");
    // null：清除 lastError（TS 传 undefined），其余字段不动。
    let cleared = index
        .apply_agent_patch(W, "t", json!({"lastError": null}))
        .await
        .expect("patch")
        .expect("row");
    assert!(cleared.get("lastError").is_none());
    assert_eq!(cleared["status"], "error");
    assert_eq!(cleared["updatedAt"], 2000);
    let cleared = index
        .apply_agent_patch(W, "t", json!({"status": null, "target": null}))
        .await
        .expect("patch")
        .expect("row");
    assert!(cleared.get("status").is_none());
    assert_eq!(cleared["target"], Value::Null);
    // 必填字段不能清除。
    for key in ["title", "updatedAt"] {
        assert!(
            index
                .apply_agent_patch(W, "t", json!({ key: null }))
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn agent_patch_respects_manual_title_and_deletion() {
    let dir = tempfile::tempdir().expect("tempdir");
    let index = open_temp(dir.path()).await;
    index
        .seed_task_metas_if_missing(vec![meta("t", json!({}))])
        .await
        .expect("seed");
    let patched = index
        .apply_agent_patch(W, "t", json!({"title": "Agent"}))
        .await
        .expect("patch")
        .expect("row");
    assert_eq!(patched["title"], "Agent");
    index.rename(W, "t", "Mine".into()).await.expect("rename");
    let patched = index
        .apply_agent_patch(W, "t", json!({"title": "Agent 2"}))
        .await
        .expect("patch")
        .expect("row");
    assert_eq!(patched["title"], "Mine");
    assert_eq!(patched["titleOverridden"], true);
    index.mark_deleted(W, "t").await.expect("delete");
    assert_eq!(
        index
            .apply_agent_patch(W, "t", json!({"title": "x"}))
            .await
            .expect("patch"),
        None
    );
    assert_eq!(
        index
            .apply_agent_patch(W, "missing", json!({}))
            .await
            .expect("patch"),
        None
    );
}

#[tokio::test]
async fn unread_markers_are_monotonic_and_clear_by_cas() {
    let dir = tempfile::tempdir().expect("tempdir");
    let index = open_temp(dir.path()).await;
    index
        .seed_task_metas_if_missing(vec![meta("t", json!({}))])
        .await
        .expect("seed");
    let first = index.set_unread(W, "t", true, None).await.expect("unread");
    let second = index.set_unread(W, "t", true, None).await.expect("unread");
    let marker =
        |outcome: &super::UnreadOutcome| outcome.meta.as_ref().and_then(|m| m["unreadAt"].as_i64());
    assert!(first.changed && second.changed);
    assert!(marker(&second) > marker(&first));
    let stale = index
        .set_unread(W, "t", false, marker(&first))
        .await
        .expect("cas");
    assert!(!stale.changed);
    assert_eq!(marker(&stale), marker(&second));
    let cleared = index
        .set_unread(W, "t", false, marker(&second))
        .await
        .expect("cas");
    assert!(cleared.changed);
    assert_eq!(marker(&cleared), None);
    // 清除后再次标记：基于持久 watermark，不会复用旧 marker。
    let third = index.set_unread(W, "t", true, None).await.expect("unread");
    assert!(marker(&third) > marker(&second));
    let unconditional = index.set_unread(W, "t", false, None).await.expect("clear");
    assert!(unconditional.changed);
    assert_eq!(marker(&unconditional), None);
    let missing = index
        .set_unread(W, "missing", true, None)
        .await
        .expect("missing");
    assert_eq!(
        missing,
        super::UnreadOutcome {
            meta: None,
            changed: false
        }
    );
}

#[tokio::test]
async fn context_menu_mutations_and_list_filters() {
    let dir = tempfile::tempdir().expect("tempdir");
    let index = open_temp(dir.path()).await;
    index
        .seed_task_metas_if_missing(vec![
            meta("a", json!({"updatedAt": 3000})),
            meta("b", json!({"updatedAt": 2000})),
            meta("c", json!({"updatedAt": 2000, "createdAt": 1500})),
            meta("d", json!({"updatedAt": 2000, "provider": "other"})),
        ])
        .await
        .expect("seed");
    let pinned = index.set_pinned(W, "a", true).await.expect("pin");
    assert!(pinned.is_some());
    index.set_archived(W, "b", true).await.expect("archive");
    index.mark_deleted(W, "c").await.expect("delete");
    let renamed = index
        .rename(W, "d", "Renamed".into())
        .await
        .expect("rename")
        .expect("row");
    assert_eq!(renamed["titleOverridden"], true);
    let ids = |metas: Vec<Value>| -> Vec<String> {
        metas
            .iter()
            .map(|m| m["taskId"].as_str().unwrap_or_default().to_owned())
            .collect()
    };
    let filter = |pinned, archived, include_deleted, provider: Option<&str>| ListFilter {
        workspace_key: Some(W.to_owned()),
        include_deleted,
        provider: provider.map(str::to_owned),
        pinned,
        archived,
    };
    assert_eq!(
        ids(index
            .list_task_metas(filter(Some(true), None, false, None))
            .await
            .expect("list")),
        ["a"]
    );
    assert_eq!(
        ids(index
            .list_task_metas(filter(None, Some(true), false, None))
            .await
            .expect("list")),
        ["b"]
    );
    // updated_at DESC, created_at DESC, task_id DESC。
    assert_eq!(
        ids(index
            .list_task_metas(filter(None, None, true, None))
            .await
            .expect("list")),
        ["d", "a", "c", "b"]
    );
    assert_eq!(
        ids(index
            .list_task_metas(filter(None, None, false, Some("glm")))
            .await
            .expect("list")),
        ["a", "b"]
    );
    assert_eq!(
        index.list_deleted_task_ids(W, None).await.expect("deleted"),
        ["c"]
    );
    assert_eq!(index.set_pinned(W, "c", true).await.expect("pin"), None);
    let unarchived = index
        .set_archived(W, "b", false)
        .await
        .expect("unarchive")
        .expect("row");
    assert_eq!(unarchived["taskId"], "b");
    assert_eq!(
        index
            .list_task_metas(ListFilter::default())
            .await
            .expect("all")
            .len(),
        3
    );
}

#[tokio::test]
async fn invalid_meta_json_falls_back_to_columns() {
    let dir = tempfile::tempdir().expect("tempdir");
    let index = open_temp(dir.path()).await;
    index
        .seed_task_metas_if_missing(vec![meta("t", json!({"model": "m"}))])
        .await
        .expect("seed");
    raw(&index)
        .execute(
            "UPDATE tasks SET meta_json = '{broken' WHERE task_id = 't'",
            [],
        )
        .expect("corrupt");
    let fallback = index
        .get_task_meta(W, "t")
        .await
        .expect("get")
        .expect("row");
    assert_eq!(fallback["traceId"], "zcode-t");
    assert_eq!(fallback["model"], "m");
    assert_eq!(fallback["provider"], "glm");
}

// ---- 多句柄并发（同一文件，模拟两个 Host） ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_opens_migrate_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = db_path(dir.path());
    let (left_events, left_sink) = recorder();
    let (right_events, right_sink) = recorder();
    let (left, right) = tokio::join!(
        TasksIndex::open(path.clone(), left_sink),
        TasksIndex::open(path.clone(), right_sink)
    );
    left.expect("left");
    right.expect("right");
    let executed = |events: &Events| {
        events
            .lock()
            .expect("events")
            .last()
            .and_then(|e| e.migration.as_ref())
            .map_or(0, |m| m.executed_count)
    };
    assert_eq!(executed(&left_events) + executed(&right_events), 3);
    let conn = Connection::open(&path).expect("open");
    let count: i64 = conn
        .query_row("SELECT count(*) FROM tasks_schema_migration", [], |row| {
            row.get(0)
        })
        .expect("ledger");
    assert_eq!(count, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_handles_allocate_distinct_unread_markers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let left = open_temp(dir.path()).await;
    let right = open_temp(dir.path()).await;
    left.seed_task_metas_if_missing(vec![meta("t", json!({}))])
        .await
        .expect("seed");
    let mut tasks = Vec::new();
    for round in 0..40 {
        let index = if round % 2 == 0 {
            left.clone()
        } else {
            right.clone()
        };
        tasks.push(tokio::spawn(async move {
            index.set_unread(W, "t", true, None).await
        }));
    }
    let mut markers = Vec::new();
    for task in tasks {
        let outcome = task.await.expect("join").expect("unread");
        markers.push(
            outcome
                .meta
                .and_then(|m| m["unreadAt"].as_i64())
                .expect("marker"),
        );
    }
    let mut unique = markers.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        markers.len(),
        "markers must never repeat: {markers:?}"
    );
    let last = right
        .get_task_meta(W, "t")
        .await
        .expect("get")
        .expect("row");
    assert_eq!(last["unreadAt"].as_i64(), unique.last().copied());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_handles_initialize_distinct_grouped_orders() {
    let dir = tempfile::tempdir().expect("tempdir");
    let left = open_temp(dir.path()).await;
    let right = open_temp(dir.path()).await;
    let mut tasks = Vec::new();
    for n in 0..30 {
        let index = if n % 2 == 0 {
            left.clone()
        } else {
            right.clone()
        };
        tasks.push(tokio::spawn(async move {
            index
                .sync_task_meta_at_grouped_top(meta(&format!("t{n}"), json!({})), None)
                .await
        }));
    }
    for task in tasks {
        let outcome = task.await.expect("join").expect("sync");
        assert!(outcome.initialized_grouped_order);
    }
    let conn = raw(&left);
    let (rows, distinct): (i64, i64) = conn
        .query_row(
            "SELECT count(*), count(DISTINCT sort_order) FROM task_group_view_node_orders",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("orders");
    assert_eq!((rows, distinct), (30, 30));
    let tasks: i64 = conn
        .query_row("SELECT count(*) FROM tasks", [], |row| row.get(0))
        .expect("tasks");
    assert_eq!(tasks, 30);
}
