//! 调度用到的 repo 操作与结算的 TS 对拍：`fixtures/repo.json` 由 scripts/rust-host/scheduler-fixtures.ts
//! 用真实 TS 实现（AutomationRepo、OffPeakTaskRepo、manualClaimRelease.ts、offPeakDispatchSettlement.ts）
//! 在固定时钟下生成；这里在同样的时钟下逐步复现，比较返回值（含键序）与最终表内容。
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use rusqlite::Connection;
use serde_json::{Map, Value, json};

use super::DispatchFailure;
use super::settle::{self, OffPeakRetry};
use crate::tasks_index::TasksIndex;
use crate::tasks_index::automation_repo::{
    AutomationRepo, DispatchFailed, FailureKind, RunIdentity, SkippedRun,
};
use crate::tasks_index::off_peak_repo::{OffPeakRepo, RunningUpdate};

const REPO: &str = include_str!("fixtures/repo.json");

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

fn text(op: &Value, key: &str) -> String {
    op[key].as_str().unwrap().to_owned()
}

fn kind(op: &Value) -> FailureKind {
    match op["kind"].as_str().or_else(|| op["failureKind"].as_str()) {
        Some("permanent") => FailureKind::Permanent,
        _ => FailureKind::Transient,
    }
}

struct Replay {
    automations: AutomationRepo,
    off_peak: OffPeakRepo,
    retry: OffPeakRetry,
}

impl Replay {
    async fn apply(&mut self, op: &Value) -> anyhow::Result<Value> {
        let repo = &self.automations;
        Ok(match op["op"].as_str().unwrap() {
            "claimDue" => json!(repo.claim_due(op["now"].as_i64().unwrap()).await?),
            "claimManualRuns" => Value::Array(
                repo.claim_manual_runs(op["now"].as_i64().unwrap())
                    .await?
                    .into_iter()
                    .map(|claimed| json!({"automation": claimed.automation, "run": claimed.run}))
                    .collect(),
            ),
            "upsertRunClaimed" => {
                let run = &op["run"];
                repo.upsert_run_claimed(
                    &RunIdentity {
                        run_id: text(run, "runId"),
                        automation_id: text(run, "automationId"),
                        workspace_key: text(run, "workspaceKey"),
                        scheduled_at: run["scheduledAt"].as_i64(),
                        trigger: text(run, "trigger"),
                    },
                    op.get("modelSelection"),
                )
                .await?;
                Value::Null
            }
            "getRun" => repo.get_run(&text(op, "runId")).await?.unwrap_or_default(),
            "markRunDispatch" => {
                repo.mark_run_dispatch(
                    &text(op, "runId"),
                    &text(op, "status"),
                    op["sessionId"].as_str().map(str::to_owned),
                    op["error"].as_str().map(str::to_owned),
                )
                .await?;
                Value::Null
            }
            "markDispatched" => {
                repo.mark_dispatched(
                    &text(op, "id"),
                    op["dispatchedAt"].as_i64().unwrap(),
                    op["nextRunAt"].as_i64(),
                )
                .await?;
                Value::Null
            }
            "markDispatchFailed" => {
                repo.mark_dispatch_failed(
                    &text(op, "id"),
                    DispatchFailed {
                        failed_at: op["failedAt"].as_i64().unwrap(),
                        error: text(op, "error"),
                        kind: kind(op),
                        next_run_at: op["nextRunAt"].as_i64(),
                    },
                )
                .await?;
                Value::Null
            }
            "releaseClaim" => {
                repo.release_claim(&text(op, "id")).await?;
                Value::Null
            }
            "skipAndReschedule" => {
                let params = &op["params"];
                repo.skip_and_reschedule(SkippedRun {
                    automation_id: text(params, "automationId"),
                    run_id: text(params, "runId"),
                    workspace_key: text(params, "workspaceKey"),
                    scheduled_at: params["scheduledAt"].as_i64(),
                    reason: text(params, "reason"),
                    next_run_at: params["nextRunAt"].as_i64(),
                    finalize: params["finalize"].as_bool().unwrap_or(false),
                })
                .await?;
                Value::Null
            }
            "markManualRunDispatched" => json!(
                repo.mark_manual_run_dispatched(
                    &text(op, "runId"),
                    op["sessionId"].as_str().map(str::to_owned),
                    op["dispatchedAt"].as_i64().unwrap(),
                )
                .await?
            ),
            "settleManualClaim" => {
                let (id, run_id) = (text(op, "id"), text(op, "runId"));
                let mut errors = Vec::new();
                // ok 只表示 prompt 已受理，锁由 Host 在会话终态时释放：不动。
                if op["ok"] == false
                    && !settle::release_manual_claim(repo, &id, &run_id, None).await?
                {
                    errors.push(settle::manual_claim_skipped(&id, &run_id));
                }
                json!({ "errors": errors })
            }
            "countActive" => json!(self.off_peak.count_active().await?),
            "recoverInterrupted" => json!(
                self.off_peak
                    .recover_interrupted(op["now"].as_i64().unwrap())
                    .await?
            ),
            "offPeakClaimDue" => json!(self.off_peak.claim_due(op["now"].as_i64().unwrap()).await?),
            "markRunning" => {
                let update = &op["update"];
                let field = |key: &str| update[key].as_str().map(str::to_owned);
                self.off_peak
                    .mark_running(
                        &text(op, "id"),
                        RunningUpdate {
                            started_at: update["startedAt"].as_i64().unwrap(),
                            conversation_id: field("conversationId"),
                            session_id: field("sessionId"),
                            server_ticket_id: field("serverTicketId"),
                        },
                    )
                    .await?
                    .unwrap_or_default()
            }
            "releaseOffPeakClaim" => {
                self.off_peak
                    .release_claim(
                        &text(op, "id"),
                        op["error"].as_str().map(str::to_owned),
                        op["now"].as_i64(),
                    )
                    .await?;
                Value::Null
            }
            "settleOffPeak" => {
                let result = if op["ok"] == true {
                    Ok(text(op, "sessionId"))
                } else {
                    Err(DispatchFailure {
                        error: text(op, "error"),
                        kind: kind(op),
                    })
                };
                settle::settle_off_peak(
                    &self.off_peak,
                    &mut self.retry,
                    &text(op, "id"),
                    result,
                    op["now"].as_i64().unwrap(),
                )
                .await?;
                let (retry_at, attempts) = self.retry.snapshot();
                json!({"retryAt": retry_at, "attempts": attempts})
            }
            other => panic!("unknown op {other}"),
        })
    }
}

#[tokio::test]
async fn scheduler_repo_operations_match_typescript() {
    let fixture: Value = serde_json::from_str(REPO).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tasks-index.sqlite");
    let clock = Arc::new(AtomicI64::new(fixture["baseNow"].as_i64().unwrap()));
    let index_clock = clock.clone();
    let index = TasksIndex::open_with(
        path.clone(),
        Arc::new(|_| {}),
        Arc::new(move || index_clock.load(Ordering::SeqCst)),
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    let mut replay = Replay {
        automations: AutomationRepo::new(index.clone()),
        off_peak: OffPeakRepo::new(index),
        retry: OffPeakRetry::default(),
    };
    let ops = fixture["ops"].as_array().unwrap();
    let results = fixture["results"].as_array().unwrap();
    assert_eq!(ops.len(), results.len());
    for (step, (op, expected)) in ops.iter().zip(results).enumerate() {
        if let Some(now) = op["now"].as_i64() {
            clock.store(now, Ordering::SeqCst);
        }
        if op["op"] == "sql" {
            Connection::open(&path)
                .unwrap()
                .execute_batch(op["sql"].as_str().unwrap())
                .unwrap();
            continue;
        }
        let actual = match replay.apply(op).await {
            Ok(value) => json!({"value": value}),
            Err(error) => json!({"error": error.to_string()}),
        };
        assert_eq!(&actual, expected, "step {step}: {op}");
        assert_eq!(
            actual.to_string(),
            expected.to_string(),
            "step {step} key order: {op}"
        );
    }
    let dumps = json!({
        "automations": dump(&path, "SELECT * FROM automations ORDER BY automation_id"),
        "automation_runs": dump(&path, "SELECT * FROM automation_runs ORDER BY run_id"),
        "off_peak_tasks": dump(&path, "SELECT * FROM off_peak_tasks ORDER BY off_peak_task_id"),
    });
    for table in ["automations", "automation_runs", "off_peak_tasks"] {
        assert_eq!(dumps[table], fixture["dumps"][table], "table {table}");
    }
}
