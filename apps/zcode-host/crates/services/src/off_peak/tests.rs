//! 闲时任务测试：仓库与 TS 对拍（`fixtures/repo.json` 由 scripts/rust-host/off-peak-fixtures.ts 生成），
//! mock 网关 + 客户端 + 编排服务的端到端行为（只连 127.0.0.1）。
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;

use rusqlite::Connection;
use serde_json::{Map, Value, json};

use super::client::{ClientError, ServerClient};
use super::credentials::CredentialSource;
use super::service::OffPeakService;
use crate::providers::ProviderRuntime;
use crate::tasks_index::TasksIndex;
use crate::tasks_index::automation_repo::workspace_key;
use crate::tasks_index::off_peak_repo::{
    OffPeakRepo, SchedulingPatch, TerminalUpdate, TicketFields,
};

const REPO: &str = include_str!("fixtures/repo.json");

async fn open_index(path: &Path, clock: Arc<AtomicI64>) -> TasksIndex {
    TasksIndex::open_with(
        path.to_path_buf(),
        Arc::new(|_| {}),
        Arc::new(move || clock.load(Ordering::SeqCst)),
        Duration::from_secs(5),
    )
    .await
    .unwrap()
}

fn dump(path: &Path, sql: &str) -> Value {
    let conn = Connection::open(path).unwrap();
    let mut statement = conn.prepare(sql).unwrap();
    let names: Vec<String> = statement
        .column_names()
        .iter()
        .map(std::string::ToString::to_string)
        .collect();
    let rows = statement
        .query_map([], |row| {
            let mut object = Map::new();
            for (index, name) in names.iter().enumerate() {
                let value = match row.get_ref(index)? {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(value) => json!(value),
                    rusqlite::types::ValueRef::Real(value) => json!(value),
                    rusqlite::types::ValueRef::Text(text) => json!(String::from_utf8_lossy(text)),
                    rusqlite::types::ValueRef::Blob(_) => Value::Null,
                };
                object.insert(name.clone(), value);
            }
            Ok(Value::Object(object))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    Value::Array(rows)
}

fn optional_int(value: &Value) -> Option<Option<i64>> {
    match value {
        Value::Null => Some(None),
        other => other.as_i64().map(Some),
    }
}

async fn apply(repo: &OffPeakRepo, op: &Value) -> Result<Value, String> {
    let error = |error: anyhow::Error| error.to_string();
    let id = op["id"].as_str().unwrap_or_default();
    Ok(match op["op"].as_str().unwrap() {
        "create" => {
            let options = &op["options"];
            repo.create(
                op["params"].clone(),
                TicketFields {
                    off_peak_task_id: options["offPeakTaskId"].as_str().map(str::to_owned),
                    server_ticket_id: options["serverTicketId"].as_str().map(str::to_owned),
                    queue_position: options["queuePosition"].as_i64(),
                    registered_at: options["registeredAt"].as_i64(),
                    schedulable: options["schedulable"] == true,
                },
                options["now"].as_i64(),
            )
            .await
            .map_err(error)?
        }
        "list" => {
            let scope = &op["scope"];
            let key = scope["workspacePath"]
                .as_str()
                .map(|path| workspace_key(path, scope["workspaceIdentity"].as_str()));
            json!(repo.list(key).await.map_err(error)?)
        }
        "get" => repo.get(id).await.map_err(error)?.unwrap_or(Value::Null),
        "hasActiveBoundTask" => json!(
            repo.has_active_bound_task(
                op["key"].as_str().unwrap(),
                op["session"].as_str().unwrap()
            )
            .await
            .map_err(error)?
        ),
        "setPaused" => repo
            .set_paused(id, op["paused"] == true, op["now"].as_i64())
            .await
            .map_err(error)?
            .unwrap_or(Value::Null),
        "updateEditableFields" => repo
            .update_editable_fields(id, op["params"].clone(), None)
            .await
            .map_err(error)?
            .unwrap_or(Value::Null),
        "updateSchedulingSnapshot" => {
            let patch = &op["patch"];
            repo.update_scheduling_snapshot(
                id,
                SchedulingPatch {
                    schedulable: patch["schedulable"].as_bool(),
                    queue_position: patch.get("queuePosition").and_then(optional_int),
                    next_poll_at: patch.get("nextPollAt").and_then(optional_int),
                    server_ticket_id: patch["serverTicketId"].as_str().map(str::to_owned),
                    registered_at: patch["registeredAt"].as_i64(),
                    now: patch["now"].as_i64(),
                },
            )
            .await
            .map_err(error)?;
            Value::Null
        }
        "listNonTerminal" => json!(repo.list_non_terminal().await.map_err(error)?),
        "listUnsettledTerminal" => json!(repo.list_unsettled_terminal().await.map_err(error)?),
        "markHistoryDeleted" => repo
            .mark_history_deleted(id, op["now"].as_i64())
            .await
            .map_err(error)?
            .unwrap_or(Value::Null),
        "requeueForContinuation" => repo
            .requeue_for_continuation(id, op["now"].as_i64())
            .await
            .map_err(error)?
            .unwrap_or(Value::Null),
        "markTerminal" => {
            let update = &op["update"];
            repo.mark_terminal(
                id,
                TerminalUpdate {
                    status: update["status"].as_str().unwrap().into(),
                    ended_at: update["endedAt"].as_i64().unwrap(),
                    failure_reason: update["failureReason"].as_str().map(str::to_owned),
                    files_changed: update["filesChanged"].as_i64(),
                    dispatch_error: update["dispatchError"].as_str().map(str::to_owned),
                },
            )
            .await
            .map_err(error)?
            .unwrap_or(Value::Null)
        }
        "markSettled" => {
            repo.mark_settled(id, op["at"].as_i64().unwrap())
                .await
                .map_err(error)?;
            Value::Null
        }
        "delete" => {
            repo.delete(id).await.map_err(error)?;
            Value::Null
        }
        other => panic!("unknown op {other}"),
    })
}

#[tokio::test]
async fn repo_matches_typescript() {
    let fixture: Value = serde_json::from_str(REPO).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tasks-index.sqlite");
    let clock = Arc::new(AtomicI64::new(fixture["baseNow"].as_i64().unwrap()));
    let repo = OffPeakRepo::new(open_index(&path, clock.clone()).await);
    for (step, (op, expected)) in fixture["ops"]
        .as_array()
        .unwrap()
        .iter()
        .zip(fixture["results"].as_array().unwrap())
        .enumerate()
    {
        if let Some(now) = op["now"].as_i64() {
            clock.store(now, Ordering::SeqCst);
        }
        if op["op"] == "sql" {
            let result = Connection::open(&path)
                .unwrap()
                .execute_batch(op["sql"].as_str().unwrap());
            assert_eq!(
                result.is_err(),
                expected.get("error").is_some(),
                "step {step}"
            );
            continue;
        }
        let actual = match apply(&repo, op).await {
            Ok(value) => json!({"value": value}),
            Err(error) => json!({"error": error}),
        };
        assert_eq!(
            actual.to_string(),
            expected.to_string(),
            "step {step}: {op}"
        );
    }
    let rows = dump(
        &path,
        "SELECT * FROM off_peak_tasks ORDER BY off_peak_task_id",
    );
    assert_eq!(rows, fixture["rows"]);
}

fn mock_vars(extra: &[(&str, &str)]) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::from([("ZCODE_OFFPEAK_MOCK".to_owned(), "1".to_owned())]);
    for (key, value) in extra {
        vars.insert((*key).to_owned(), (*value).to_owned());
    }
    vars
}

#[tokio::test]
async fn gateway_and_client_follow_the_ticket_state_machine() {
    let vars = mock_vars(&[
        ("ZCODE_OFFPEAK_MOCK_QUOTA_COUNT", "1"),
        ("ZCODE_OFFPEAK_MOCK_READY_DELAY_MS", "0"),
    ]);
    let client = ServerClient::new(
        vars.clone(),
        CredentialSource::new(vars, None),
        None,
        Some(0),
    )
    .unwrap();
    assert!(
        client
            .origin()
            .await
            .unwrap()
            .starts_with("http://127.0.0.1:")
    );
    match client.take_ticket("task-1").await {
        Err(ClientError::Server(error)) => {
            assert_eq!(error.biz_code, Some(3103));
            assert_eq!(error.http_status, 429);
            assert!(error.next_take_at.is_some());
            assert_eq!(
                error.message,
                "off-peak /ticket failed: HTTP 429 code=3103 free tier limit reached"
            );
        }
        other => panic!("expected quota error, got {other:?}"),
    }
    let ticket = client.take_ticket("task-1").await.unwrap();
    assert_eq!(ticket.state, "ready");
    assert_eq!(ticket.position, None);
    assert_eq!(ticket.next_poll_after_ms, Some(5_000));
    let again = client.take_ticket("task-1").await.unwrap();
    let status = client
        .batch_status(&[
            ticket.ticket_id.clone(),
            again.ticket_id.clone(),
            "nope".into(),
        ])
        .await
        .unwrap();
    let states: Vec<&str> = status
        .tickets
        .iter()
        .map(|entry| entry.state.as_str())
        .collect();
    assert_eq!(states, ["expired", "ready", "not_found"]);
    client.settle(&again.ticket_id).await.unwrap();
    let status = client
        .batch_status(std::slice::from_ref(&again.ticket_id))
        .await
        .unwrap();
    assert_eq!(status.tickets[0].state, "settled");
    assert_eq!(
        client.take_number_availability().await.unwrap(),
        json!({"canTakeNumber": true})
    );
}

fn providers(dir: &Path) -> ProviderRuntime {
    // 模拟已授权的闲时 Provider：Rust Host 还没有账号（R4），用个人配置把它覆盖成 api-key 接入。
    let personal = dir.join("provider_config.json");
    std::fs::write(
        &personal,
        json!({"schemaVersion": 1, "config": {"providerConfigRules": {"providerRules": [{
            "providerId": "account:bigmodel-offpeak-idle-plan",
            "config": {"access": {"type": "api-key", "apiKey": "sk-offpeak"}},
        }]}}})
        .to_string(),
    )
    .unwrap();
    ProviderRuntime::new(
        Some(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../../../config/provider/zcode-builtin.json"),
        ),
        personal,
    )
}

#[tokio::test]
async fn service_creates_pauses_continues_and_cancels_with_the_mock_gateway() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(AtomicI64::new(1_791_300_000_000));
    let index = open_index(&dir.path().join("tasks-index.sqlite"), clock.clone()).await;
    let vars = mock_vars(&[("ZCODE_OFFPEAK_MOCK_READY_DELAY_MS", "60000")]);
    let wakes = Arc::new(AtomicUsize::new(0));
    let wake_counter = wakes.clone();
    let service = OffPeakService::new(
        OffPeakRepo::new(index),
        ServerClient::new(
            vars.clone(),
            CredentialSource::new(vars.clone(), None),
            None,
            Some(0),
        )
        .unwrap(),
        providers(dir.path()),
        Arc::new(move || clock.load(Ordering::SeqCst)),
        Arc::new(move || {
            wake_counter.fetch_add(1, Ordering::SeqCst);
        }),
        None,
    );
    assert_eq!(
        service.coding_plan_support().await.unwrap(),
        json!({"supported": true, "kind": "bigmodel-personal", "providerFamily": "bigmodel",
               "providerId": "account:bigmodel-individual-coding-plan"})
    );
    let params = json!({
        "title": "nightly",
        "prompt": "refactor",
        "permissionMode": "yolo",
        "modelSelection": {"providerId": "ignored", "modelId": "GLM-5.3",
                           "options": {"reasoningLevel": "high"}},
        "workspacePath": "/tmp/ws",
    });
    let invalid = service
        .create_task(&json!({"title": "x", "prompt": "y", "permissionMode": "nope"}))
        .await
        .unwrap();
    assert_eq!(invalid["errorCategory"], "client_validation");
    assert_eq!(invalid["providerName"], "127.0.0.1");
    let created = service.create_task(&params).await.unwrap();
    assert_eq!(created["ok"], true, "{created}");
    assert_eq!(created["ticketInitialState"], "queued");
    assert_eq!(created["queuePosition"], 1);
    let task = &created["task"];
    assert_eq!(task["status"], "queued");
    assert_eq!(
        task["modelSelection"]["providerId"],
        "account:bigmodel-offpeak-idle-plan"
    );
    let id = task["offPeakTaskId"].as_str().unwrap().to_owned();
    assert_eq!(wakes.load(Ordering::SeqCst), 0);

    let paused = service.pause_task(&id).await.unwrap().unwrap();
    assert_eq!(paused["status"], "paused");
    let resumed = service.continue_task(&id).await.unwrap().unwrap();
    assert_eq!(resumed["status"], "queued");
    assert_eq!(resumed["serverTicketId"], task["serverTicketId"]);
    let edited = service
        .update_task(&id, &json!({"title": "renamed"}))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(edited["title"], "renamed");
    let listed = service.list().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["schedulable"], false);

    let cancelled = service.cancel_task(&id).await.unwrap().unwrap();
    assert_eq!(cancelled["status"], "cancelled");
    // 核销在后台完成：settled_at 回填后不再出现在 outbox。
    for _ in 0..50 {
        if service
            .repo()
            .list_unsettled_terminal()
            .await
            .unwrap()
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        service
            .repo()
            .list_unsettled_terminal()
            .await
            .unwrap()
            .is_empty()
    );
    service.delete_task(&id).await.unwrap();
    assert!(service.get(&id).await.unwrap().is_none());
}
