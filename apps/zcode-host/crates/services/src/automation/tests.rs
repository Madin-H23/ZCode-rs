//! automation 与 TS 对拍：`fixtures/*.json` 由 scripts/rust-host/automation-fixtures.ts 用真实 TS
//! 实现（croner、automationCron.ts、AutomationService、AutomationRepo）生成。
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use rusqlite::Connection;
use serde_json::{Map, Value, json};

use super::cron::{Cron, is_valid};
use super::schedule::{
    build_interval_schedule_rule, build_relative_delay_schedule, compute_initial_next_run_at,
    compute_schedule_rule_next_run_at, infer_minute_interval_schedule_rule,
    schedule_rule_definition,
};
use super::service::{AutomationService, Scope};
use super::time::{ChronoZone, Zone};
use crate::tasks_index::TasksIndex;
use crate::tasks_index::automation_repo::{AutomationRepo, RunIdentity};

const CRON: &str = include_str!("fixtures/cron.json");
const SCHEDULE: &str = include_str!("fixtures/schedule.json");
const SERVICE: &str = include_str!("fixtures/service.json");

fn zone(name: &str) -> ChronoZone<chrono_tz::Tz> {
    ChronoZone(name.parse().expect("known zone"))
}

fn outcome(result: Result<Option<i64>, String>) -> Value {
    match result {
        Ok(value) => json!({"value": value}),
        Err(error) => json!({"error": error}),
    }
}

#[test]
fn cron_matches_croner() {
    let fixture: Value = serde_json::from_str(CRON).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let zone = zone(case["zone"].as_str().unwrap());
        let expression = case["expression"].as_str().unwrap();
        assert_eq!(
            is_valid(expression, &zone),
            case["valid"].as_bool().unwrap(),
            "validity of {expression:?}"
        );
        if !case["valid"].as_bool().unwrap() {
            continue;
        }
        let cron = Cron::parse(expression, &zone).unwrap();
        for run in case["runs"].as_array().unwrap() {
            let from = run["from"].as_i64().unwrap();
            assert_eq!(
                outcome(cron.next_run(&zone, from)),
                run["next"],
                "next of {expression:?} in {} from {from}",
                case["zone"]
            );
            assert_eq!(
                outcome(cron.previous_run(&zone, from)),
                run["previous"],
                "previous of {expression:?} in {} from {from}",
                case["zone"]
            );
        }
    }
}

#[test]
fn schedule_rules_match_typescript() {
    let fixture: Value = serde_json::from_str(SCHEDULE).unwrap();
    let zone = zone("America/New_York");
    for case in fixture["rules"].as_array().unwrap() {
        let rule = case["rule"].as_object().unwrap();
        assert_eq!(
            schedule_rule_definition(rule),
            case["definition"],
            "{rule:?}"
        );
        for next in case["next"].as_array().unwrap() {
            let from = next["from"].as_i64().unwrap();
            assert_eq!(
                json!(compute_schedule_rule_next_run_at(&zone, rule, from)),
                next["next"],
                "{rule:?} from {from}"
            );
        }
    }
    for case in fixture["carriers"].as_array().unwrap() {
        let rule = build_interval_schedule_rule(
            &zone,
            case["unit"].as_str().unwrap(),
            case["interval"].as_i64().unwrap(),
            case["cronExpr"].as_str().unwrap(),
            1_791_300_000_000,
        )
        .unwrap();
        // 键序也要相同：规则原样序列化进 schedule_rule 列。
        assert_eq!(Value::Object(rule).to_string(), case["rule"].to_string());
    }
    for case in fixture["relative"].as_array().unwrap() {
        let (cron_expr, rule) = build_relative_delay_schedule(
            &zone,
            case["delay"].as_i64().unwrap(),
            1_791_300_000_000,
        );
        let actual = json!({"cronExpr": cron_expr, "scheduleRule": rule});
        assert_eq!(actual.to_string(), case["result"].to_string());
    }
    for case in fixture["inferred"].as_array().unwrap() {
        let rule = infer_minute_interval_schedule_rule(
            &zone,
            case["cronExpr"].as_str().unwrap(),
            1_791_300_000_000,
        );
        assert_eq!(
            rule.map(Value::Object).unwrap_or(Value::Null).to_string(),
            case["rule"].to_string()
        );
    }
    for case in fixture["initial"].as_array().unwrap() {
        let result = compute_initial_next_run_at(
            &zone,
            case["cronExpr"].as_str().unwrap(),
            case["recurring"].as_bool().unwrap(),
            None,
            case["from"].as_i64().unwrap(),
        );
        match result {
            Ok(value) => assert_eq!(json!(value), case["value"], "{case}"),
            Err(error) => assert_eq!(json!(error), case["error"], "{case}"),
        }
    }
}

/// 把生成的 id 换成 fixture 里的记号（长的先换）。
fn normalize(value: &Value, ids: &[(String, String)]) -> Value {
    match value {
        Value::String(text) => {
            let mut text = text.clone();
            let mut sorted: Vec<_> = ids.iter().collect();
            sorted.sort_by_key(|(id, _)| std::cmp::Reverse(id.len()));
            for (id, token) in sorted {
                text = text.replace(id.as_str(), token);
            }
            Value::String(text)
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| normalize(item, ids)).collect())
        }
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, item)| (key.clone(), normalize(item, ids)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn dump(path: &Path, sql: &str) -> Value {
    let conn = Connection::open(path).unwrap();
    let mut statement = conn.prepare(sql).unwrap();
    let names: Vec<String> = statement
        .column_names()
        .iter()
        .map(|name| name.to_string())
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

fn scope_of(value: &Value) -> Option<Scope> {
    value
        .is_object()
        .then(|| Scope::from_params(value))
        .flatten()
}

struct Replay {
    service: AutomationService,
    repo: AutomationRepo,
    clock: Arc<AtomicI64>,
    automations: Vec<String>,
    runs: Vec<String>,
    ids: Vec<(String, String)>,
}

impl Replay {
    fn run_id(&self, op: &Value) -> String {
        match op.get("run") {
            Some(index) => self.runs[index.as_u64().unwrap() as usize].clone(),
            None => op["runId"]
                .as_str()
                .unwrap()
                .replace("#0", &self.automations[0]),
        }
    }

    async fn apply(&mut self, op: &Value) -> Result<Value, String> {
        if let Some(now) = op["now"].as_i64() {
            self.clock.store(now, Ordering::SeqCst);
        }
        let id = op["id"]
            .as_u64()
            .map(|index| self.automations[index as usize].clone())
            .unwrap_or_default();
        let scope = scope_of(&op["scope"]);
        let default_scope = Scope {
            workspace_path: "/tmp/ws-a".into(),
            workspace_identity: None,
        };
        let error = |error: anyhow::Error| error.to_string();
        let value = match op["op"].as_str().unwrap() {
            "create" => {
                let created = self
                    .service
                    .create(op["params"].as_object().unwrap())
                    .await
                    .map_err(error)?;
                let automation_id = created["automationId"].as_str().unwrap().to_owned();
                self.ids.push((
                    automation_id.clone(),
                    format!("<automation:{}>", self.automations.len()),
                ));
                self.automations.push(automation_id);
                created
            }
            "list" => json!(self.service.list(scope.as_ref()).await.map_err(error)?),
            "update" => self
                .service
                .update(
                    &id,
                    op["patch"].as_object().unwrap(),
                    Some(scope.as_ref().unwrap_or(&default_scope)),
                )
                .await
                .map_err(error)?
                .unwrap_or(Value::Null),
            "delete" => json!(
                self.service
                    .delete(&id, scope.as_ref())
                    .await
                    .map_err(error)?
            ),
            "setEnabled" => {
                let enabled = op["enabled"].as_bool().unwrap();
                let scope = scope.unwrap_or(default_scope);
                self.service
                    .set_enabled(&id, enabled, Some(&scope))
                    .await
                    .map_err(error)?;
                Value::Null
            }
            "restart" => {
                let scope = scope.unwrap_or(default_scope);
                self.service
                    .restart(&id, Some(&scope))
                    .await
                    .map_err(error)?;
                Value::Null
            }
            "hasTaskBinding" => json!(
                self.service
                    .has_task_binding(
                        scope.as_ref().unwrap(),
                        op["targetTaskId"].as_str().unwrap()
                    )
                    .await
                    .map_err(error)?
            ),
            "runNow" => match self
                .service
                .run_now(&id, scope.as_ref())
                .await
                .map_err(error)?
            {
                Some(claimed) => {
                    let run_id = claimed.run["runId"].as_str().unwrap().to_owned();
                    self.ids
                        .push((run_id.clone(), format!("<run:{}>", self.runs.len())));
                    self.runs.push(run_id);
                    json!({"automation": claimed.automation, "run": claimed.run})
                }
                None => Value::Null,
            },
            "getBotDeliveryTarget" => self
                .repo
                .get_bot_delivery_target(&id, None)
                .await
                .map_err(error)?
                .unwrap_or(Value::Null),
            "getModelSelectionForDispatch" => self
                .repo
                .get_model_selection_for_dispatch(&id, &scope.unwrap().key())
                .await
                .map_err(error)?
                .unwrap_or(Value::Null),
            "fixRunModelSelection" => self
                .repo
                .fix_run_model_selection(&self.run_id(op), &op["selection"])
                .await
                .map_err(error)?,
            "markRunOutcome" => {
                self.repo
                    .mark_run_outcome(
                        &self.run_id(op),
                        op["outcome"].as_str().unwrap(),
                        op["error"].as_str().map(str::to_owned),
                    )
                    .await
                    .map_err(error)?;
                Value::Null
            }
            "markRunDispatch" => {
                self.repo
                    .mark_run_dispatch(
                        &self.run_id(op),
                        op["status"].as_str().unwrap(),
                        None,
                        op["error"].as_str().map(str::to_owned),
                    )
                    .await
                    .map_err(error)?;
                Value::Null
            }
            "markManualRunDispatched" => json!(
                self.repo
                    .mark_manual_run_dispatched(
                        &self.run_id(op),
                        op["sessionId"].as_str().map(str::to_owned),
                        op["dispatchedAt"].as_i64().unwrap(),
                    )
                    .await
                    .map_err(error)?
            ),
            "touchManualClaim" => {
                self.repo
                    .touch_manual_claim(&id, "/tmp/ws-a")
                    .await
                    .map_err(error)?;
                Value::Null
            }
            "releaseManualClaim" => {
                self.repo
                    .release_manual_claim(&id, "/tmp/ws-a")
                    .await
                    .map_err(error)?;
                Value::Null
            }
            "ensureRunClaimed" => {
                self.repo
                    .ensure_run_claimed(&RunIdentity {
                        run_id: self.run_id(op),
                        automation_id: id.clone(),
                        workspace_key: "/tmp/ws-a".into(),
                        scheduled_at: op["scheduledAt"].as_i64(),
                        trigger: op["trigger"].as_str().unwrap().into(),
                    })
                    .await
                    .map_err(error)?;
                Value::Null
            }
            "listRuns" => json!(
                self.service
                    .list_runs(&id, scope.as_ref())
                    .await
                    .map_err(error)?
            ),
            "deleteRun" => {
                self.service
                    .delete_run(&self.run_id(op), scope.as_ref())
                    .await
                    .map_err(error)?;
                Value::Null
            }
            "getRun" => self
                .repo
                .get_run(&self.run_id(op))
                .await
                .map_err(error)?
                .unwrap_or(Value::Null),
            other => panic!("unknown op {other}"),
        };
        Ok(value)
    }
}

#[tokio::test]
async fn service_and_repo_match_typescript() {
    let fixture: Value = serde_json::from_str(SERVICE).unwrap();
    let base_now = fixture["serviceNow"].as_i64().unwrap();
    for scenario in fixture["scenarios"].as_array().unwrap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks-index.sqlite");
        let clock = Arc::new(AtomicI64::new(base_now));
        let index_clock = clock.clone();
        let index = TasksIndex::open_with(
            path.clone(),
            Arc::new(|_| {}),
            Arc::new(move || index_clock.load(Ordering::SeqCst)),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        let repo = AutomationRepo::new(index);
        let service_clock = clock.clone();
        let zone: Arc<dyn Zone> = Arc::new(zone("America/New_York"));
        let mut replay = Replay {
            service: AutomationService::new(
                repo.clone(),
                zone,
                Arc::new(move || service_clock.load(Ordering::SeqCst)),
            ),
            repo,
            clock,
            automations: Vec::new(),
            runs: Vec::new(),
            ids: Vec::new(),
        };
        let name = scenario["name"].as_str().unwrap();
        for (step, (op, expected)) in scenario["ops"]
            .as_array()
            .unwrap()
            .iter()
            .zip(scenario["results"].as_array().unwrap())
            .enumerate()
        {
            if op["op"] == "sql" {
                if let Some(now) = op["now"].as_i64() {
                    replay.clock.store(now, Ordering::SeqCst);
                }
                Connection::open(&path)
                    .unwrap()
                    .execute_batch(op["sql"].as_str().unwrap())
                    .unwrap();
                continue;
            }
            let actual = match replay.apply(op).await {
                Ok(value) => json!({"value": value}),
                Err(error) => json!({"error": error}),
            };
            let actual = normalize(&actual, &replay.ids);
            assert_eq!(&actual, expected, "{name} step {step}: {op}");
            assert_eq!(
                actual.to_string(),
                expected.to_string(),
                "{name} step {step} key order: {op}"
            );
        }
        let dumps = json!({
            "automations": dump(&path, "SELECT * FROM automations ORDER BY created_at, title"),
            "automation_runs": dump(&path, "SELECT * FROM automation_runs ORDER BY created_at, run_id"),
        });
        assert_eq!(
            normalize(&dumps, &replay.ids),
            scenario["dumps"],
            "{name} tables"
        );
    }
}
