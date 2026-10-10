//! 调度状态机（docs/specs/rust-u4-app-host.md §2.4、§6）：真实的 tasks-index 与仓库，派发器换成由
//! 测试逐个应答的替身；时钟注入，不等待 20 s 的轮询。
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use rusqlite::Connection;
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, oneshot};

use super::{
    CronDispatch, DispatchFailure, OffPeakDispatch, Scheduler, SchedulerDeps, SchedulerHandle,
    SchedulerOptions, WakeSignal,
};
use crate::automation::dispatch::CronRunRequest;
use crate::automation::schedule::compute_automation_next_run_at;
use crate::automation::service::AutomationService;
use crate::automation::time::{ChronoZone, Zone};
use crate::off_peak::dispatch::OffPeakRunRequest;
use crate::tasks_index::TasksIndex;
use crate::tasks_index::automation_repo::{AutomationRepo, FailureKind, compute_retry_at};
use crate::tasks_index::off_peak_repo::OffPeakRepo;

const BASE: i64 = 1_791_300_000_000;
const CRON: &str = "*/5 * * * *";
const SELECTION: &str = r#"{"providerId":"p","modelId":"m","options":{"reasoningLevel":"high"}}"#;
const WAIT: Duration = Duration::from_secs(10);
const QUIET: Duration = Duration::from_millis(300);

type Reply = oneshot::Sender<Result<String, DispatchFailure>>;

/// 派发替身：每个请求交给测试，测试决定何时、以什么结果应答。
struct Fake<R> {
    requests: mpsc::Sender<(R, Reply)>,
}

impl<R: Send + 'static> Fake<R> {
    fn call(&self, request: R) -> BoxFuture<'static, Result<String, DispatchFailure>> {
        let requests = self.requests.clone();
        async move {
            let (reply, result) = oneshot::channel();
            if requests.send((request, reply)).await.is_err() {
                return Err(transient("test ended"));
            }
            result
                .await
                .unwrap_or_else(|_| Err(transient("reply dropped")))
        }
        .boxed()
    }
}

impl CronDispatch for Fake<CronRunRequest> {
    fn dispatch(
        &self,
        request: CronRunRequest,
    ) -> BoxFuture<'static, Result<String, DispatchFailure>> {
        self.call(request)
    }
}

impl OffPeakDispatch for Fake<OffPeakRunRequest> {
    fn dispatch(
        &self,
        request: OffPeakRunRequest,
    ) -> BoxFuture<'static, Result<String, DispatchFailure>> {
        self.call(request)
    }
}

fn transient(error: &str) -> DispatchFailure {
    DispatchFailure {
        error: error.into(),
        kind: FailureKind::Transient,
    }
}

fn permanent(error: &str) -> DispatchFailure {
    DispatchFailure {
        error: error.into(),
        kind: FailureKind::Permanent,
    }
}

fn zone() -> Arc<dyn Zone> {
    Arc::new(ChronoZone(
        "America/New_York".parse::<chrono_tz::Tz>().unwrap(),
    ))
}

fn next_after(from: i64) -> i64 {
    compute_automation_next_run_at(zone().as_ref(), CRON, None, from)
        .unwrap()
        .unwrap()
}

fn sql_literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".into(),
        Value::String(text) => format!("'{text}'"),
        other => other.to_string(),
    }
}

fn insert(path: &PathBuf, table: &str, columns: Map<String, Value>) {
    let names: Vec<&str> = columns.keys().map(String::as_str).collect();
    let values: Vec<String> = columns.values().map(sql_literal).collect();
    Connection::open(path)
        .unwrap()
        .execute_batch(&format!(
            "INSERT INTO {table} ({}) VALUES ({})",
            names.join(", "),
            values.join(", ")
        ))
        .unwrap();
}

fn with_defaults(defaults: Value, overrides: Value) -> Map<String, Value> {
    let mut columns = defaults.as_object().unwrap().clone();
    columns.extend(overrides.as_object().unwrap().clone());
    columns
}

/// 一个数据库文件上的仓库、派发替身与 Scheduler。
struct Harness {
    _dir: Option<tempfile::TempDir>,
    path: PathBuf,
    clock: Arc<AtomicI64>,
    index: TasksIndex,
    cron: mpsc::Receiver<(CronRunRequest, Reply)>,
    off_peak: mpsc::Receiver<(OffPeakRunRequest, Reply)>,
    cron_fake: Arc<Fake<CronRunRequest>>,
    off_peak_fake: Arc<Fake<OffPeakRunRequest>>,
    counts: Arc<Mutex<Vec<u64>>>,
}

async fn open_index(path: PathBuf, clock: &Arc<AtomicI64>) -> TasksIndex {
    let clock = clock.clone();
    TasksIndex::open_with(
        path,
        Arc::new(|_| {}),
        Arc::new(move || clock.load(Ordering::SeqCst)),
        Duration::from_secs(5),
    )
    .await
    .unwrap()
}

impl Harness {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks-index.sqlite");
        let mut harness = Self::on(path, Arc::new(AtomicI64::new(BASE))).await;
        harness._dir = Some(dir);
        harness
    }

    /// 同一个文件上的另一套连接与替身（模拟另一个调度器）。
    async fn on(path: PathBuf, clock: Arc<AtomicI64>) -> Self {
        let index = open_index(path.clone(), &clock).await;
        let (cron_requests, cron) = mpsc::channel(64);
        let (off_peak_requests, off_peak) = mpsc::channel(64);
        Self {
            _dir: None,
            path,
            clock,
            index,
            cron,
            off_peak,
            cron_fake: Arc::new(Fake {
                requests: cron_requests,
            }),
            off_peak_fake: Arc::new(Fake {
                requests: off_peak_requests,
            }),
            counts: Arc::default(),
        }
    }

    fn now(&self) -> i64 {
        self.clock.load(Ordering::SeqCst)
    }

    fn set_now(&self, now: i64) {
        self.clock.store(now, Ordering::SeqCst);
    }

    fn start_with(&self, wake: WakeSignal, tick_interval: Duration) -> SchedulerHandle {
        let clock = self.clock.clone();
        let repo = AutomationRepo::new(self.index.clone());
        let service_clock = clock.clone();
        let counts = self.counts.clone();
        Scheduler::start(
            &zcode_host_base::testing::spawner(),
            SchedulerDeps {
                automations: Arc::new(AutomationService::new(
                    repo,
                    zone(),
                    Arc::new(move || service_clock.load(Ordering::SeqCst)),
                )),
                off_peak: OffPeakRepo::new(self.index.clone()),
                cron: self.cron_fake.clone(),
                off_peak_dispatch: self.off_peak_fake.clone(),
                active_count: Arc::new(move |count| counts.lock().unwrap().push(count)),
                wake,
            },
            SchedulerOptions {
                clock: Arc::new(move || clock.load(Ordering::SeqCst)),
                tick_interval,
            },
        )
    }

    /// 轮询间隔很长：轮次只由启动与唤醒触发。
    fn start(&self) -> SchedulerHandle {
        self.start_with(WakeSignal::new(), Duration::from_secs(3600))
    }

    fn automation(&self, id: &str, overrides: Value) {
        insert(
            &self.path,
            "automations",
            with_defaults(
                json!({
                    "automation_id": id, "title": id, "cron_expr": CRON, "prompt": format!("run {id}"),
                    "model_selection": "null", "workspace_key": "/tmp/ws-a",
                    "workspace_path": "/tmp/ws-a", "created_at": 1, "updated_at": 1,
                }),
                overrides,
            ),
        );
    }

    fn manual_run(&self, run_id: &str, automation_id: &str) {
        insert(
            &self.path,
            "automation_runs",
            with_defaults(
                json!({
                    "run_id": run_id, "automation_id": automation_id, "workspace_key": "/tmp/ws-a",
                    "scheduled_at": BASE - 10, "trigger": "manual", "dispatch_status": "claimed",
                    "attempts": 0, "created_at": BASE - 10, "updated_at": BASE - 10,
                }),
                json!({}),
            ),
        );
    }

    fn off_peak_task(&self, id: &str, overrides: Value) {
        insert(
            &self.path,
            "off_peak_tasks",
            with_defaults(
                json!({
                    "off_peak_task_id": id, "server_ticket_id": format!("ticket-{id}"), "title": id,
                    "prompt": format!("do {id}"), "permission_mode": "yolo",
                    "model_selection": SELECTION, "workspace_key": "/tmp/ws-a",
                    "workspace_path": "/tmp/ws-a", "status": "queued", "queued_at": 10,
                    "schedulable": 1, "created_at": 10, "updated_at": 10,
                }),
                overrides,
            ),
        );
    }

    /// 一行（列名 → JSON）；没有时为 Null。
    fn row(&self, sql: &str) -> Value {
        let conn = Connection::open(&self.path).unwrap();
        let mut statement = conn.prepare(sql).unwrap();
        let names: Vec<String> = statement
            .column_names()
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        let mut rows = statement.query([]).unwrap();
        let Some(row) = rows.next().unwrap() else {
            return Value::Null;
        };
        let mut object = Map::new();
        for (index, name) in names.iter().enumerate() {
            let value = match row.get_ref(index).unwrap() {
                rusqlite::types::ValueRef::Null => Value::Null,
                rusqlite::types::ValueRef::Integer(value) => json!(value),
                rusqlite::types::ValueRef::Real(value) => json!(value),
                rusqlite::types::ValueRef::Text(text) => json!(String::from_utf8_lossy(text)),
                rusqlite::types::ValueRef::Blob(_) => Value::Null,
            };
            object.insert(name.clone(), value);
        }
        Value::Object(object)
    }

    fn automation_row(&self, id: &str) -> Value {
        self.row(&format!(
            "SELECT * FROM automations WHERE automation_id = '{id}'"
        ))
    }

    fn run_row(&self, run_id: &str) -> Value {
        self.row(&format!(
            "SELECT * FROM automation_runs WHERE run_id = '{run_id}'"
        ))
    }

    fn off_peak_row(&self, id: &str) -> Value {
        self.row(&format!(
            "SELECT * FROM off_peak_tasks WHERE off_peak_task_id = '{id}'"
        ))
    }

    async fn eventually(&self, what: &str, check: impl Fn(&Self) -> bool) {
        tokio::time::timeout(WAIT, async {
            while !check(self) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    async fn next_cron(&mut self) -> (CronRunRequest, Reply) {
        tokio::time::timeout(WAIT, self.cron.recv())
            .await
            .expect("cron dispatch within timeout")
            .expect("cron dispatch channel open")
    }

    async fn next_off_peak(&mut self) -> (OffPeakRunRequest, Reply) {
        tokio::time::timeout(WAIT, self.off_peak.recv())
            .await
            .expect("off-peak dispatch within timeout")
            .expect("off-peak dispatch channel open")
    }

    async fn no_cron(&mut self) {
        if let Ok(Some((request, _))) = tokio::time::timeout(QUIET, self.cron.recv()).await {
            panic!("unexpected cron dispatch {}", request.run_id);
        }
    }

    async fn no_off_peak(&mut self) {
        if let Ok(Some((request, _))) = tokio::time::timeout(QUIET, self.off_peak.recv()).await {
            panic!("unexpected off-peak dispatch {}", request.off_peak_task_id);
        }
    }

    fn counts(&self) -> Vec<u64> {
        self.counts.lock().unwrap().clone()
    }
}

#[tokio::test]
async fn due_automation_is_claimed_dispatched_and_settled() {
    let mut harness = Harness::new().await;
    harness.automation(
        "a-1",
        json!({"next_run_at": BASE - 1000, "target_task_id": "task-1", "mode": "plan",
               "workspace_identity": " id-a ", "workspace_key": "id-a"}),
    );
    harness.automation(
        "a-once",
        json!({"next_run_at": BASE - 500, "recurring": 0, "model_selection": SELECTION}),
    );
    let scheduler = harness.start();
    let mut requests = HashMap::new();
    for _ in 0..2 {
        let (request, reply) = harness.next_cron().await;
        requests.insert(request.automation_id.clone(), (request, reply));
    }
    let (request, reply) = requests.remove("a-1").unwrap();
    assert_eq!(request.run_id, format!("a-1:{}", BASE - 1000));
    assert_eq!(request.prompt, "run a-1");
    assert_eq!(request.target_task_id.as_deref(), Some("task-1"));
    assert_eq!(request.mode.as_deref(), Some("plan"));
    assert_eq!(request.workspace_identity.as_deref(), Some(" id-a "));
    assert_eq!(request.model_selection, None);
    // 派发在途：认领与 run 台账已落库。
    let row = harness.automation_row("a-1");
    assert_eq!(
        (row["running"].clone(), row["dispatch_status"].clone()),
        (json!(1), json!("claimed"))
    );
    let run = harness.run_row(&request.run_id);
    assert_eq!(run["dispatch_status"], "claimed");
    assert_eq!(run["attempts"], 0);
    assert_eq!(run["workspace_key"], "id-a");

    harness.set_now(BASE + 5_000);
    reply.send(Ok("session-1".into())).unwrap();
    harness
        .eventually("a-1 settled", |h| {
            h.automation_row("a-1")["dispatch_status"] == "dispatched"
        })
        .await;
    let row = harness.automation_row("a-1");
    assert_eq!(row["running"], 0);
    assert_eq!(row["run_count"], 1);
    assert_eq!(row["scheduled_run_count"], 1);
    assert_eq!(row["last_run_at"], BASE + 5_000);
    assert_eq!(row["next_run_at"], next_after(BASE + 5_000));
    assert_eq!(row["lifecycle_status"], "active");
    let run = harness.run_row(&request.run_id);
    assert_eq!(run["dispatch_status"], "dispatched");
    assert_eq!(run["session_id"], "session-1");

    // 一次性任务派发成功即终结；固定的模型选择随请求下发。
    let (request, reply) = requests.remove("a-once").unwrap();
    assert_eq!(
        request.model_selection,
        Some(json!({"providerId": "p", "modelId": "m", "options": {"reasoningLevel": "high"}}))
    );
    reply.send(Ok("session-2".into())).unwrap();
    harness
        .eventually("a-once completed", |h| {
            h.automation_row("a-once")["lifecycle_status"] == "completed"
        })
        .await;
    let row = harness.automation_row("a-once");
    assert_eq!(
        (row["enabled"].clone(), row["next_run_at"].clone()),
        (json!(0), Value::Null)
    );
    harness.no_cron().await;
    scheduler.dispose().await;
}

#[tokio::test]
async fn missed_windows_are_skipped_rescheduled_or_finalized() {
    let mut harness = Harness::new().await;
    let grace = 5 * 60_000;
    harness.automation("a-rec", json!({"next_run_at": BASE - grace - 60_000}));
    // 边界含在内：恰好早 5 分钟也算错过。
    harness.automation("a-edge", json!({"next_run_at": BASE - grace}));
    harness.automation("a-inside", json!({"next_run_at": BASE - grace + 1}));
    harness.automation(
        "a-once",
        json!({"next_run_at": BASE - 2 * grace, "recurring": 0}),
    );
    // 重试中的任务不按 misfire 处理。
    harness.automation(
        "a-retry",
        json!({"next_run_at": BASE - 6 * grace, "retry_at": BASE - 1, "dispatch_attempts": 1,
               "dispatch_status": "failed_to_dispatch"}),
    );
    let scheduler = harness.start();
    let mut dispatched = Vec::new();
    for _ in 0..2 {
        let (request, reply) = harness.next_cron().await;
        dispatched.push(request.run_id.clone());
        drop(reply);
    }
    dispatched.sort();
    assert_eq!(
        dispatched,
        vec![
            format!("a-inside:{}", BASE - grace + 1),
            format!("a-retry:{}", BASE - 6 * grace)
        ]
    );
    for (id, scheduled) in [("a-rec", BASE - grace - 60_000), ("a-edge", BASE - grace)] {
        let row = harness.automation_row(id);
        assert_eq!(row["next_run_at"], next_after(BASE), "{id}");
        assert_eq!(row["dispatch_status"], "idle", "{id}");
        assert_eq!(row["running"], 0, "{id}");
        assert_eq!(row["run_count"], 0, "{id}");
        let run = harness.run_row(&format!("{id}:{scheduled}"));
        assert_eq!(run["dispatch_status"], "skipped", "{id}");
        assert_eq!(run["error"], "computer_asleep_or_app_not_running", "{id}");
    }
    let row = harness.automation_row("a-once");
    assert_eq!(row["lifecycle_status"], "completed");
    assert_eq!(row["enabled"], 0);
    assert_eq!(row["next_run_at"], Value::Null);
    assert_eq!(
        harness.run_row(&format!("a-once:{}", BASE - 2 * grace))["dispatch_status"],
        "skipped"
    );
    scheduler.dispose().await;
}

#[tokio::test]
async fn transient_failures_back_off_until_the_limit_then_skip_the_cycle() {
    let mut harness = Harness::new().await;
    harness.automation("a-r", json!({"next_run_at": BASE - 1000}));
    let scheduler = harness.start();
    let run_id = format!("a-r:{}", BASE - 1000);
    for attempt in 1..=5_i64 {
        let (request, reply) = harness.next_cron().await;
        // 重试复用同一个 runId（scheduledAt 取 next_run_at）。
        assert_eq!(request.run_id, run_id);
        reply.send(Err(transient("busy"))).unwrap();
        if attempt == 5 {
            break;
        }
        let now = harness.now();
        harness
            .eventually("backoff written", |h| {
                h.automation_row("a-r")["dispatch_attempts"] == attempt
            })
            .await;
        let row = harness.automation_row("a-r");
        assert_eq!(row["retry_at"], compute_retry_at(now, attempt));
        assert_eq!(row["dispatch_status"], "failed_to_dispatch");
        assert_eq!(row["running"], 0);
        let run = harness.run_row(&run_id);
        assert_eq!(run["dispatch_status"], "failed_to_dispatch");
        assert_eq!(run["error"], "busy");
        assert_eq!(run["attempts"], attempt - 1);
        // 退避未到：唤醒也不派发。
        scheduler.wake();
        harness.no_cron().await;
        harness.set_now(compute_retry_at(now, attempt));
        scheduler.wake();
    }
    let now = harness.now();
    harness
        .eventually("cycle skipped", |h| {
            h.automation_row("a-r")["dispatch_status"] == "idle"
        })
        .await;
    let row = harness.automation_row("a-r");
    assert_eq!(row["dispatch_attempts"], 0);
    assert_eq!(row["retry_at"], Value::Null);
    assert_eq!(row["next_run_at"], next_after(now));
    assert_eq!(row["last_error"], "busy");
    assert_eq!(row["lifecycle_status"], "active");
    scheduler.dispose().await;
}

#[tokio::test]
async fn finite_automation_fails_at_the_limit_and_permanent_failure_is_terminal() {
    let mut harness = Harness::new().await;
    harness.automation(
        "a-limit",
        json!({"next_run_at": BASE - 1000, "recurring": 0, "max_runs": 3, "dispatch_attempts": 4,
               "retry_at": BASE - 1, "dispatch_status": "failed_to_dispatch"}),
    );
    harness.automation("a-perm", json!({"next_run_at": BASE - 1000}));
    let scheduler = harness.start();
    for _ in 0..2 {
        let (request, reply) = harness.next_cron().await;
        let failure = if request.automation_id == "a-perm" {
            permanent("no model")
        } else {
            transient("busy")
        };
        reply.send(Err(failure)).unwrap();
    }
    for (id, error) in [("a-limit", "busy"), ("a-perm", "no model")] {
        harness
            .eventually("terminal failure", |h| {
                h.automation_row(id)["lifecycle_status"] == "failed"
            })
            .await;
        let row = harness.automation_row(id);
        assert_eq!(row["enabled"], 0, "{id}");
        assert_eq!(row["running"], 0, "{id}");
        assert_eq!(row["dispatch_status"], "failed_to_dispatch", "{id}");
        assert_eq!(row["last_error"], error, "{id}");
    }
    scheduler.wake();
    harness.no_cron().await;
    scheduler.dispose().await;
}

#[tokio::test]
async fn manual_run_counts_without_advancing_the_schedule() {
    let mut harness = Harness::new().await;
    harness.automation("a-m", json!({"next_run_at": BASE + 3_600_000}));
    harness.automation("a-f", json!({"next_run_at": BASE + 3_600_000}));
    harness.manual_run("a-m:manual:1", "a-m");
    harness.manual_run("a-f:manual:1", "a-f");
    let scheduler = harness.start();
    let mut replies = HashMap::new();
    for _ in 0..2 {
        let (request, reply) = harness.next_cron().await;
        replies.insert(request.run_id.clone(), reply);
    }
    assert_eq!(harness.run_row("a-m:manual:1")["attempts"], 1);
    assert_eq!(harness.automation_row("a-m")["running"], 1);

    harness.set_now(BASE + 1_000);
    replies
        .remove("a-m:manual:1")
        .unwrap()
        .send(Ok("session-m".into()))
        .unwrap();
    harness
        .eventually("manual dispatched", |h| {
            h.run_row("a-m:manual:1")["dispatch_status"] == "dispatched"
        })
        .await;
    let row = harness.automation_row("a-m");
    assert_eq!(row["run_count"], 1);
    assert_eq!(row["last_run_at"], BASE + 1_000);
    assert_eq!(row["scheduled_run_count"], 0);
    assert_eq!(row["next_run_at"], BASE + 3_600_000);
    // Host 受理只表示 prompt 已发出：锁由 Host 在会话终态时释放，scheduler 不释放。
    assert_eq!(row["running"], 1);
    assert_eq!(harness.run_row("a-m:manual:1")["session_id"], "session-m");

    // 派发失败：记 failed_to_dispatch 并释放锁，不计次数。
    replies
        .remove("a-f:manual:1")
        .unwrap()
        .send(Err(transient("boom")))
        .unwrap();
    harness
        .eventually("manual failure", |h| {
            h.automation_row("a-f")["running"] == 0
        })
        .await;
    let run = harness.run_row("a-f:manual:1");
    assert_eq!(
        (run["dispatch_status"].clone(), run["error"].clone()),
        (json!("failed_to_dispatch"), json!("boom"))
    );
    let row = harness.automation_row("a-f");
    assert_eq!(
        (row["run_count"].clone(), row["lifecycle_status"].clone()),
        (json!(0), json!("active"))
    );
    scheduler.dispose().await;
}

#[tokio::test]
async fn manual_run_waits_for_the_in_flight_dispatch_and_runs_after_it_settles() {
    let mut harness = Harness::new().await;
    harness.automation("a-w", json!({"next_run_at": BASE - 1000}));
    harness.manual_run("a-w:manual:1", "a-w");
    let scheduler = harness.start();
    let (request, reply) = harness.next_cron().await;
    assert_eq!(request.run_id, format!("a-w:{}", BASE - 1000));
    // 同一 automation 已有派发在途（single-flight）：manual run 留在队列。
    scheduler.wake();
    harness.no_cron().await;
    assert_eq!(harness.run_row("a-w:manual:1")["attempts"], 0);
    // 结算释放锁后立即补一轮，不等 20 s 轮询。
    reply.send(Ok("session-s".into())).unwrap();
    let (request, _reply) = harness.next_cron().await;
    assert_eq!(request.run_id, "a-w:manual:1");
    assert_eq!(harness.automation_row("a-w")["scheduled_run_count"], 1);
    scheduler.dispose().await;
}

#[tokio::test]
async fn off_peak_recovers_claims_backs_off_and_reports_active_count() {
    let mut harness = Harness::new().await;
    // 上一个 App 实例残留的 running：启动时回 queued，会话保留供续跑。
    harness.off_peak_task(
        "o-run",
        json!({"status": "running", "queued_at": 1, "conversation_id": "c-run", "session_id": "c-run"}),
    );
    harness.off_peak_task(
        "o-q",
        json!({"queued_at": 10, "workspace_identity": "id-q"}),
    );
    harness.off_peak_task("o-hidden", json!({"schedulable": 0}));
    let wake = WakeSignal::new();
    let scheduler = harness.start_with(wake.clone(), Duration::from_secs(3600));
    let (first, first_reply) = harness.next_off_peak().await;
    let (second, second_reply) = harness.next_off_peak().await;
    // FIFO：queued_at 早的先派发。
    assert_eq!(first.off_peak_task_id, "o-run");
    assert_eq!(first.conversation_id.as_deref(), Some("c-run"));
    assert_eq!(first.server_ticket_id.as_deref(), Some("ticket-o-run"));
    assert_eq!(second.off_peak_task_id, "o-q");
    assert_eq!(second.workspace_identity.as_deref(), Some("id-q"));
    assert_eq!(
        second.model_selection,
        json!({"providerId": "p", "modelId": "m", "options": {"reasoningLevel": "high"}})
    );
    harness
        .eventually("first report", |h| h.counts() == vec![0])
        .await;

    harness.set_now(BASE + 100);
    second_reply.send(Ok("session-q".into())).unwrap();
    first_reply.send(Err(transient("gateway busy"))).unwrap();
    harness
        .eventually("o-q running", |h| {
            h.off_peak_row("o-q")["status"] == "running"
        })
        .await;
    harness
        .eventually("o-run released", |h| {
            h.off_peak_row("o-run")["claim_running"] == 0
        })
        .await;
    let row = harness.off_peak_row("o-q");
    assert_eq!(
        (row["conversation_id"].clone(), row["started_at"].clone()),
        (json!("session-q"), json!(BASE + 100))
    );
    let row = harness.off_peak_row("o-run");
    assert_eq!(
        (row["status"].clone(), row["attempt_count"].clone()),
        (json!("queued"), json!(1))
    );
    assert_eq!(row["last_error"], "gateway busy");

    // 闲时任务变为可调度时的唤醒（与同步模块共用同一信号）：退避中只认领再释放，不派发。
    wake.wake();
    harness.no_off_peak().await;
    assert_eq!(harness.off_peak_row("o-run")["claim_running"], 0);
    assert_eq!(harness.counts(), vec![0, 1]);

    harness.set_now(BASE + 100 + 30_000);
    wake.wake();
    let (retry, reply) = harness.next_off_peak().await;
    assert_eq!(retry.off_peak_task_id, "o-run");
    reply.send(Err(permanent("no credentials"))).unwrap();
    harness
        .eventually("o-run failed", |h| {
            h.off_peak_row("o-run")["status"] == "failed"
        })
        .await;
    let row = harness.off_peak_row("o-run");
    assert_eq!(row["failure_reason"], "no credentials");
    assert_eq!(row["attempt_count"], 2);
    // 计数没变：不重复上报。
    wake.wake();
    harness.no_off_peak().await;
    assert_eq!(harness.counts(), vec![0, 1]);
    scheduler.dispose().await;
}

#[tokio::test]
async fn dispose_releases_in_flight_claims() {
    let mut harness = Harness::new().await;
    harness.automation("a-d", json!({"next_run_at": BASE - 1000}));
    harness.automation("a-dm", json!({"next_run_at": BASE + 3_600_000}));
    harness.manual_run("a-dm:manual:1", "a-dm");
    harness.off_peak_task("o-d", json!({}));
    let scheduler = harness.start();
    let _pending = (
        harness.next_cron().await,
        harness.next_cron().await,
        harness.next_off_peak().await,
    );
    assert_eq!(harness.automation_row("a-d")["running"], 1);
    assert_eq!(harness.off_peak_row("o-d")["claim_running"], 1);
    scheduler.dispose().await;
    let row = harness.automation_row("a-d");
    assert_eq!(
        (row["running"].clone(), row["dispatch_status"].clone()),
        (json!(0), json!("idle"))
    );
    assert_eq!(row["next_run_at"], BASE - 1000);
    assert_eq!(harness.automation_row("a-dm")["running"], 0);
    let row = harness.off_peak_row("o-d");
    assert_eq!(
        (row["claim_running"].clone(), row["attempt_count"].clone()),
        (json!(0), json!(0))
    );
    // 重复 dispose 立即返回；之后的唤醒无效。
    scheduler.dispose().await;
    scheduler.wake();
    harness.no_cron().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_schedulers_on_one_database_dispatch_each_task_once() {
    let mut first = Harness::new().await;
    let mut second = Harness::on(first.path.clone(), first.clock.clone()).await;
    let schedulers = [first.start(), second.start()];
    // 两边都完成启动（recoverInterrupted 与第一轮）之后才有任务：启动回收假设自己是 App 级单例，
    // 会把另一个调度器正在执行的闲时任务置回 queued（Node 相同，见 rust-u4-app-host.md §2.4）。
    for scheduler in &schedulers {
        first
            .eventually("startup", |_| scheduler.ticks() >= 1)
            .await;
    }
    for index in 0..20 {
        first.automation(
            &format!("a-{index:02}"),
            json!({"next_run_at": BASE - 1000 - index, "created_at": index}),
        );
    }
    for index in 0..10 {
        first.off_peak_task(&format!("o-{index:02}"), json!({"queued_at": index}));
    }
    let mut seen: HashMap<String, usize> = HashMap::new();
    let deadline = tokio::time::Instant::now() + WAIT;
    while seen.len() < 30 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "dispatched so far: {seen:?}"
        );
        for scheduler in &schedulers {
            scheduler.wake();
        }
        let pick = tokio::time::timeout(Duration::from_millis(50), async {
            tokio::select! {
                Some((request, reply)) = first.cron.recv() => { let _ = reply.send(Ok(format!("s-{}", request.run_id))); request.run_id }
                Some((request, reply)) = second.cron.recv() => { let _ = reply.send(Ok(format!("s-{}", request.run_id))); request.run_id }
                Some((request, reply)) = first.off_peak.recv() => { let _ = reply.send(Ok("s".into())); request.off_peak_task_id }
                Some((request, reply)) = second.off_peak.recv() => { let _ = reply.send(Ok("s".into())); request.off_peak_task_id }
            }
        })
        .await;
        if let Ok(id) = pick {
            *seen.entry(id).or_default() += 1;
        }
    }
    // 结算后再唤醒几轮：任何任务都不会被第二次派发。
    for scheduler in &schedulers {
        scheduler.wake();
    }
    first.no_cron().await;
    second.no_cron().await;
    first.no_off_peak().await;
    second.no_off_peak().await;
    assert!(seen.values().all(|count| *count == 1), "{seen:?}");
    for scheduler in &schedulers {
        scheduler.dispose().await;
    }
    for index in 0..20 {
        let row = first.automation_row(&format!("a-{index:02}"));
        assert_eq!(row["scheduled_run_count"], 1, "{row}");
    }
}

#[tokio::test]
async fn wakes_collapse_into_one_follow_up_tick() {
    let harness = Harness::new().await;
    // 启动前的唤醒并入第一轮。
    let wake = WakeSignal::new();
    for _ in 0..3 {
        wake.wake();
    }
    let scheduler = harness.start_with(wake, Duration::from_secs(3600));
    harness
        .eventually("first tick", |_| scheduler.ticks() >= 1)
        .await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(scheduler.ticks(), 1);

    // 让下一轮卡在写锁上，期间多次唤醒：结束后只补一轮。
    let blocker = Connection::open(&harness.path).unwrap();
    blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
    scheduler.wake();
    harness
        .eventually("second tick started", |_| scheduler.ticks() == 2)
        .await;
    for _ in 0..5 {
        scheduler.wake();
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    blocker.execute_batch("COMMIT").unwrap();
    harness
        .eventually("follow-up tick", |_| scheduler.ticks() == 3)
        .await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(scheduler.ticks(), 3);
    scheduler.dispose().await;
}

#[tokio::test]
async fn periodic_tick_dispatches_due_work_without_a_wake() {
    let mut harness = Harness::new().await;
    let scheduler = harness.start_with(WakeSignal::new(), Duration::from_millis(100));
    harness
        .eventually("first tick", |_| scheduler.ticks() >= 1)
        .await;
    harness.automation("a-p", json!({"next_run_at": BASE - 1000}));
    let (request, _reply) = harness.next_cron().await;
    assert_eq!(request.run_id, format!("a-p:{}", BASE - 1000));
    scheduler.dispose().await;
}
