//! 闲时任务编排服务（offPeakTaskService.ts）：创建即取号、取消 / 暂停 / 继续 / 删除 / 编辑的
//! 状态守卫、批量轮询同步（有非终态任务才轮询）、终态核销 outbox、票据过期续跑重取号。
//!
//! ```text
//! createTask ──► 校验 → 模型选择 → 绑定会话预检 → POST /ticket ──► 落库（queued，ready 则 schedulable）
//!                                                              └► 唤醒 scheduler（ready 时）
//! sync 周期（tokio 定时器）：补报未核销终态 → 无票补取号 → POST /ticket/status → 写回快照
//!                          间隔 = next_poll_after（钳制 5 s – 5 min），失败指数退避
//! ```
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};
use zcode_async::{Spawner, Task};
use zcode_host_kit::log;

use super::client::{ClientError, ServerClient};
use super::models;
use crate::providers::ProviderRuntime;
use crate::tasks_index::automation_repo::workspace_key;
use crate::tasks_index::off_peak_repo::{
    OffPeakRepo, SchedulingPatch, TerminalUpdate, TicketFields, is_bound_session_conflict,
    is_terminal,
};

const SYNC_MIN_INTERVAL_MS: i64 = 5_000;
const SYNC_MAX_INTERVAL_MS: i64 = 5 * 60_000;
const SYNC_FAILURE_BASE_MS: i64 = 10_000;
const CREATE_PERMISSION_MODES: [&str; 6] = ["yolo", "plan", "edit", "auto", "autoEdit", "build"];

pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;
pub type Hook = Arc<dyn Fn() + Send + Sync>;
pub type StopRunning = Arc<
    dyn Fn(
            String,
            String,
            Option<String>,
        ) -> futures_util::future::BoxFuture<'static, Result<(), String>>
        + Send
        + Sync,
>;

pub struct OffPeakService {
    /// 闲时任务服务的作用域（automation 区域的作用域）。
    spawner: Spawner,
    repo: OffPeakRepo,
    client: ServerClient,
    providers: ProviderRuntime,
    clock: Clock,
    wake: Hook,
    stop_running: Option<StopRunning>,
    sync: Mutex<SyncState>,
}

#[derive(Default)]
struct SyncState {
    stopped: bool,
    running: bool,
    failures: u32,
    timer: Option<Task<Option<()>>>,
}

fn non_empty_text(value: &Value) -> bool {
    value.as_str().is_some_and(|text| !text.trim().is_empty())
}

/// `classifyOffPeakCreateFailure`。
fn failure(stage: &str, error: Option<&ClientError>, provider_name: &str) -> Value {
    let (category, code) = match (stage, error) {
        ("client_validation", _) => ("client_validation", String::new()),
        ("local_persist", _) => ("local_persist", String::new()),
        (_, Some(ClientError::Server(error))) => (
            match error.biz_code {
                Some(3101) => "eligibility_3101",
                Some(3103) => "quota_3103",
                _ => "unknown",
            },
            error
                .biz_code
                .map(|code| code.to_string())
                .unwrap_or_default(),
        ),
        (_, Some(ClientError::InvalidResponse(_))) => ("invalid_response", String::new()),
        _ => ("network", String::new()),
    };
    json!({
        "ok": false,
        "failureStage": stage,
        "errorCategory": category,
        "errorCode": code,
        "providerName": provider_name,
    })
}

fn bound_failure(provider_name: &str) -> Value {
    json!({
        "ok": false,
        "failureStage": "client_validation",
        "errorCategory": "client_validation",
        "errorCode": "session_bound",
        "providerName": provider_name,
    })
}

fn text(value: &Value, key: &str) -> Option<String> {
    value[key].as_str().map(str::to_owned)
}

impl OffPeakService {
    /// 同步定时器与取消后的结算在 `spawner` 的作用域里。
    pub fn new(
        spawner: &Spawner,
        repo: OffPeakRepo,
        client: ServerClient,
        providers: ProviderRuntime,
        clock: Clock,
        wake: Hook,
        stop_running: Option<StopRunning>,
    ) -> Arc<Self> {
        Arc::new(Self {
            spawner: spawner.clone(),
            repo,
            client,
            providers,
            clock,
            wake,
            stop_running,
            sync: Mutex::new(SyncState {
                stopped: true,
                ..SyncState::default()
            }),
        })
    }

    pub fn repo(&self) -> &OffPeakRepo {
        &self.repo
    }

    pub fn client(&self) -> &ServerClient {
        &self.client
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SyncState> {
        self.sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn request_wake(&self) {
        (self.wake)();
    }

    /// `getCodingPlanSupport`（`resolveOffPeakCodingPlanSupport`）：每次按当前账号状态解析。
    pub async fn coding_plan_support(&self) -> Result<Value, String> {
        self.client.credential_source().support().await
    }

    /// `resolveTelemetryProviderName`：只取安全的 hostname，失败为空串。
    async fn provider_name(&self) -> String {
        match self.client.origin().await {
            Ok(origin) => url::Url::parse(&origin)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .unwrap_or_default(),
            Err(_) => String::new(),
        }
    }

    async fn resolve_selection(
        &self,
        model_id: Option<&str>,
        level: Option<&str>,
    ) -> Option<Value> {
        let (_, resolved) = self.providers.snapshot().await;
        // 旧行没有 Provider 身份：只能复用当前凭据链裁定的 family，不能按 Registry 顺序猜。
        let support = self.coding_plan_support().await.unwrap_or(Value::Null);
        let family = (support["supported"] == true)
            .then(|| support["providerFamily"].as_str())
            .flatten();
        models::resolve_model_selection(&resolved, family, model_id, level)
    }

    /// Host 派发前的窄检查：同一 Provider、同一模型仍可解析。
    pub async fn validate_dispatch_selection(&self, selection: &Value) -> bool {
        let level = selection["options"]["reasoningLevel"].as_str();
        let resolved = self
            .resolve_selection(selection["modelId"].as_str(), level)
            .await;
        resolved.is_some_and(|resolved| {
            resolved["providerId"] == selection["providerId"]
                && resolved["modelId"] == selection["modelId"]
        })
    }

    pub async fn take_number_availability(&self) -> Result<Value, String> {
        self.client
            .take_number_availability()
            .await
            .map_err(|error| error.to_string())
    }

    /// 创建即取号（成功才落库）；失败只返回稳定分类。
    pub async fn create_task(self: &Arc<Self>, params: &Value) -> Result<Value, String> {
        let provider_name = self.provider_name().await;
        let valid = non_empty_text(&params["title"])
            && non_empty_text(&params["prompt"])
            && non_empty_text(&params["workspacePath"])
            && params["permissionMode"]
                .as_str()
                .is_some_and(|mode| CREATE_PERMISSION_MODES.contains(&mode));
        if !valid {
            return Ok(failure("client_validation", None, &provider_name));
        }
        let selection = self
            .resolve_selection(
                params["modelSelection"]["modelId"].as_str(),
                params["modelSelection"]["options"]["reasoningLevel"].as_str(),
            )
            .await;
        let Some(selection) = selection else {
            return Ok(failure("client_validation", None, &provider_name));
        };
        let mut normalized = params.as_object().cloned().unwrap_or_default();
        normalized.insert("modelSelection".into(), selection);
        let key = workspace_key(
            params["workspacePath"].as_str().unwrap_or_default(),
            params["workspaceIdentity"].as_str(),
        );
        if let Some(bound) = params["boundSessionId"]
            .as_str()
            .filter(|id| !id.is_empty())
            && self
                .repo
                .has_active_bound_task(&key, bound)
                .await
                .map_err(|error| error.to_string())?
        {
            return Ok(bound_failure(&provider_name));
        }
        let id = format!("offpeak-{}", crate::agent::service::uuid_v4());
        let ticket = match self.client.take_ticket(&id).await {
            Ok(ticket) => ticket,
            Err(error) => return Ok(failure("ticket_request", Some(&error), &provider_name)),
        };
        let created = self
            .repo
            .create(
                Value::Object(normalized),
                TicketFields {
                    off_peak_task_id: Some(id.clone()),
                    server_ticket_id: Some(ticket.ticket_id.clone()),
                    queue_position: ticket.position,
                    registered_at: Some(ticket.registered_at),
                    schedulable: ticket.state == "ready",
                },
                None,
            )
            .await;
        let created = match created {
            Ok(created) => created,
            Err(error) if is_bound_session_conflict(&error) => {
                return Ok(bound_failure(&provider_name));
            }
            Err(_) => return Ok(failure("local_persist", None, &provider_name)),
        };
        if let Some(after) = ticket.next_poll_after_ms {
            let patch = SchedulingPatch {
                next_poll_at: Some(Some(self.now() + after)),
                ..SchedulingPatch::default()
            };
            if self
                .repo
                .update_scheduling_snapshot(&id, patch)
                .await
                .is_err()
            {
                return Ok(failure("local_persist", None, &provider_name));
            }
        }
        log::info(format!(
            "off-peak task created id={id} ticket={} state={}",
            ticket.ticket_id, ticket.state
        ));
        self.ensure_sync_scheduled(0);
        if ticket.state == "ready" {
            self.request_wake();
        }
        let mut result = Map::new();
        result.insert("ok".into(), json!(true));
        result.insert("task".into(), created);
        result.insert("ticketInitialState".into(), json!(ticket.state));
        if let Some(position) = ticket.position {
            result.insert("queuePosition".into(), json!(position));
        }
        result.insert("providerName".into(), json!(provider_name));
        Ok(Value::Object(result))
    }

    /// 取消：先落终态再停 loop，迟到的 stopped 回写被终态守卫丢弃。
    pub async fn cancel_task(self: &Arc<Self>, id: &str) -> anyhow::Result<Option<Value>> {
        let Some(existing) = self.repo.get(id).await? else {
            return Ok(None);
        };
        let status = existing["status"].as_str().unwrap_or_default();
        if is_terminal(status) {
            return Ok(Some(existing));
        }
        let update = TerminalUpdate {
            status: "cancelled".into(),
            ended_at: self.now(),
            ..TerminalUpdate::default()
        };
        let Some(cancelled) = self.repo.mark_terminal(id, update).await? else {
            return self.repo.get(id).await;
        };
        if status == "running"
            && let (Some(conversation), Some(stop)) =
                (existing["conversationId"].as_str(), &self.stop_running)
            && let Err(error) = stop(
                conversation.to_owned(),
                text(&existing, "workspacePath").unwrap_or_default(),
                text(&existing, "workspaceIdentity"),
            )
            .await
        {
            log::warn(format!(
                "off-peak cancel stop loop failed task={id}: {error}"
            ));
        }
        let service = self.clone();
        let settled = cancelled.clone();
        drop(self.spawner.spawn("off-peak-settle", async move {
            service.settle_one(&settled).await
        }));
        Ok(Some(cancelled))
    }

    pub async fn pause_task(&self, id: &str) -> anyhow::Result<Option<Value>> {
        self.repo.set_paused(id, true, Some(self.now())).await
    }

    /// Continue：票活着 = 恢复派发；票已废 = 此刻手动重取号回队尾。
    pub async fn continue_task(self: &Arc<Self>, id: &str) -> anyhow::Result<Option<Value>> {
        let Some(resumed) = self.repo.set_paused(id, false, Some(self.now())).await? else {
            return Ok(None);
        };
        let mut alive = false;
        if let Some(ticket_id) = resumed["serverTicketId"].as_str() {
            match self.client.batch_status(&[ticket_id.to_owned()]).await {
                Ok(status) => {
                    let entry = status
                        .tickets
                        .iter()
                        .find(|entry| entry.ticket_id == ticket_id);
                    alive = entry.is_some_and(|entry| {
                        entry.state != "expired" && entry.state != "not_found"
                    });
                    if let Some(entry) = entry.filter(|_| alive) {
                        self.repo
                            .update_scheduling_snapshot(
                                id,
                                SchedulingPatch {
                                    schedulable: Some(entry.state == "ready"),
                                    queue_position: Some(entry.position),
                                    now: Some(self.now()),
                                    ..SchedulingPatch::default()
                                },
                            )
                            .await?;
                    }
                }
                Err(error) => {
                    // 状态查询失败按票活处理，交给轮询兜底，不白耗一次取号额度。
                    log::warn(format!(
                        "off-peak continue status check failed task={id}: {error}"
                    ));
                    alive = true;
                }
            }
        }
        if !alive {
            self.retake_ticket(id).await;
        }
        self.ensure_sync_scheduled(0);
        self.repo.get(id).await
    }

    /// 删除：非终态先按取消处理，再删行。
    pub async fn delete_task(self: &Arc<Self>, id: &str) -> anyhow::Result<()> {
        let Some(existing) = self.repo.get(id).await? else {
            return Ok(());
        };
        if !is_terminal(existing["status"].as_str().unwrap_or_default()) {
            self.cancel_task(id).await?;
        }
        self.repo.delete(id).await
    }

    pub async fn delete_history(&self, id: &str) -> anyhow::Result<Option<Value>> {
        self.repo.mark_history_deleted(id, Some(self.now())).await
    }

    /// 编辑：只有 queued / paused 可编辑；模型必须能由当前 Registry 解析。
    pub async fn update_task(&self, id: &str, params: &Value) -> anyhow::Result<Option<Value>> {
        let Some(existing) = self.repo.get(id).await? else {
            return Ok(None);
        };
        if !matches!(existing["status"].as_str(), Some("queued" | "paused")) {
            return Ok(None);
        }
        if params.get("modelSelection") == Some(&Value::Null) {
            return Ok(None);
        }
        let candidate = params
            .get("modelSelection")
            .or_else(|| existing.get("modelSelection"))
            .cloned();
        let Some(candidate) = candidate else {
            return Ok(None);
        };
        let Some(selection) = self
            .resolve_selection(
                candidate["modelId"].as_str(),
                candidate["options"]["reasoningLevel"].as_str(),
            )
            .await
        else {
            return Ok(None);
        };
        let mut normalized = params.as_object().cloned().unwrap_or_default();
        normalized.insert("modelSelection".into(), selection);
        self.repo
            .update_editable_fields(id, Value::Object(normalized), Some(self.now()))
            .await
    }

    pub async fn list(&self) -> anyhow::Result<Vec<Value>> {
        let tasks = self.repo.list(None).await?;
        Ok(self.project_issues(tasks).await)
    }

    pub async fn get(&self, id: &str) -> anyhow::Result<Option<Value>> {
        let Some(task) = self.repo.get(id).await? else {
            return Ok(None);
        };
        Ok(self.project_issues(vec![task]).await.pop())
    }

    /// 只派生当前配置诊断，不写数据库。
    async fn project_issues(&self, tasks: Vec<Value>) -> Vec<Value> {
        let mut out = Vec::with_capacity(tasks.len());
        for mut task in tasks {
            if let Some(selection) = task.get("modelSelection").cloned() {
                let usable = is_terminal(task["status"].as_str().unwrap_or_default())
                    || self.validate_dispatch_selection(&selection).await;
                if !usable {
                    task["schedulable"] = json!(false);
                    let mut issue = Map::new();
                    issue.insert("code".into(), json!("repair-required"));
                    issue.insert("legacyModelId".into(), selection["modelId"].clone());
                    if let Some(level) = selection["options"]["reasoningLevel"].as_str() {
                        issue.insert("legacyReasoningLevel".into(), json!(level));
                    }
                    task["modelSelectionIssue"] = Value::Object(issue);
                }
            } else {
                task["schedulable"] = json!(false);
            }
            out.push(task);
        }
        out
    }

    /// 票据过期（active 到期 / ready 废票）：回队保会话 → 同 task_id 重取号。
    pub async fn handle_ticket_expired_during_run(
        self: &Arc<Self>,
        id: &str,
    ) -> anyhow::Result<()> {
        if self
            .repo
            .requeue_for_continuation(id, Some(self.now()))
            .await?
            .is_none()
        {
            log::info(format!(
                "off-peak ticket-expired requeue dropped (not running) task={id}"
            ));
            return Ok(());
        }
        self.retake_ticket(id).await;
        self.ensure_sync_scheduled(0);
        Ok(())
    }

    /// 同 task_id 重新取号；失败留给轮询周期重试。
    async fn retake_ticket(&self, id: &str) {
        match self.client.take_ticket(id).await {
            Ok(ticket) => {
                let patch = SchedulingPatch {
                    server_ticket_id: Some(ticket.ticket_id.clone()),
                    registered_at: Some(ticket.registered_at),
                    schedulable: Some(ticket.state == "ready"),
                    queue_position: Some(ticket.position),
                    next_poll_at: ticket
                        .next_poll_after_ms
                        .map(|after| Some(self.now() + after)),
                    now: Some(self.now()),
                };
                if let Err(error) = self.repo.update_scheduling_snapshot(id, patch).await {
                    log::warn(format!(
                        "off-peak re-take ticket write failed task={id}: {error:#}"
                    ));
                    return;
                }
                if ticket.state == "ready" {
                    self.request_wake();
                }
                log::info(format!(
                    "off-peak re-take ticket task={id} ticket={} state={}",
                    ticket.ticket_id, ticket.state
                ));
            }
            Err(error) => log::warn(format!("off-peak re-take ticket failed task={id}: {error}")),
        }
    }

    pub fn start_sync(self: &Arc<Self>) {
        {
            let mut state = self.lock();
            if !state.stopped {
                return;
            }
            state.stopped = false;
        }
        self.ensure_sync_scheduled(0);
    }

    pub fn stop_sync(&self) {
        let mut state = self.lock();
        state.stopped = true;
        if let Some(timer) = state.timer.take() {
            timer.abort();
        }
    }

    fn ensure_sync_scheduled(self: &Arc<Self>, delay_ms: i64) {
        let mut state = self.lock();
        if state.stopped {
            return;
        }
        if let Some(timer) = state.timer.take() {
            timer.abort();
        }
        let weak = Arc::downgrade(self);
        // 下一轮同步的定时器：常驻（关闭作用域时随取消信号结束，不算被中止的工作）。
        state.timer = Some(self.spawner.spawn_resident("off-peak-sync", async move {
            tokio::time::sleep(Duration::from_millis(delay_ms.max(0) as u64)).await;
            if let Some(service) = weak.upgrade() {
                service.lock().timer = None;
                service.run_sync_cycle().await;
            }
        }));
    }

    /// 单次同步周期。
    pub async fn run_sync_cycle(self: &Arc<Self>) {
        {
            let mut state = self.lock();
            if state.running {
                return;
            }
            state.running = true;
        }
        let result = self.sync_once().await;
        let next = match result {
            // 没有任务时 TS 的 finally 仍按最大间隔重排（注释说不重排，代码会重排；以代码为准）。
            Ok(None) => {
                self.lock().failures = 0;
                SYNC_MAX_INTERVAL_MS
            }
            Ok(Some(delay)) => {
                self.lock().failures = 0;
                delay
            }
            Err(error) => {
                let failures = {
                    let mut state = self.lock();
                    state.failures += 1;
                    state.failures
                };
                log::warn(format!(
                    "off-peak sync cycle failed (attempt {failures}): {error}"
                ));
                (SYNC_FAILURE_BASE_MS * 2i64.pow(failures.saturating_sub(1).min(16)))
                    .min(SYNC_MAX_INTERVAL_MS)
            }
        };
        self.lock().running = false;
        self.ensure_sync_scheduled(next.clamp(SYNC_MIN_INTERVAL_MS, SYNC_MAX_INTERVAL_MS));
    }

    /// 返回下次间隔；None 表示没有任务。
    async fn sync_once(self: &Arc<Self>) -> Result<Option<i64>, String> {
        let error = |error: anyhow::Error| error.to_string();
        for task in self.repo.list_unsettled_terminal().await.map_err(error)? {
            self.settle_one(&task).await;
        }
        let non_terminal = self
            .project_issues(self.repo.list_non_terminal().await.map_err(error)?)
            .await;
        for task in &non_terminal {
            if task.get("serverTicketId").is_none() && task["status"] == "queued" {
                self.retake_ticket(task["offPeakTaskId"].as_str().unwrap_or_default())
                    .await;
            }
        }
        let with_tickets: Vec<&Value> = non_terminal
            .iter()
            .filter(|task| task.get("serverTicketId").is_some())
            .collect();
        if with_tickets.is_empty() && non_terminal.is_empty() {
            return Ok(None);
        }
        if with_tickets.is_empty() {
            return Ok(Some(SYNC_MIN_INTERVAL_MS));
        }
        let ids: Vec<String> = with_tickets
            .iter()
            .filter_map(|task| task["serverTicketId"].as_str().map(str::to_owned))
            .collect();
        let status = self
            .client
            .batch_status(&ids)
            .await
            .map_err(|error| error.to_string())?;
        let mut became = false;
        for task in with_tickets {
            let Some(entry) = status
                .tickets
                .iter()
                .find(|entry| Some(entry.ticket_id.as_str()) == task["serverTicketId"].as_str())
            else {
                continue;
            };
            became = self
                .apply_ticket_status(task, &entry.state, entry.position)
                .await
                .map_err(error)?
                || became;
        }
        if became {
            self.request_wake();
        }
        Ok(Some(
            status.next_poll_after_ms.unwrap_or(SYNC_MIN_INTERVAL_MS),
        ))
    }

    /// 单票状态映射；返回是否翻为可派发。
    async fn apply_ticket_status(
        &self,
        task: &Value,
        state: &str,
        position: Option<i64>,
    ) -> anyhow::Result<bool> {
        let now = self.now();
        let id = task["offPeakTaskId"].as_str().unwrap_or_default();
        let patch = |patch: SchedulingPatch| SchedulingPatch {
            now: Some(now),
            ..patch
        };
        match state {
            "ready" => {
                let became = task["schedulable"] != true && task["status"] == "queued";
                self.repo
                    .update_scheduling_snapshot(
                        id,
                        patch(SchedulingPatch {
                            schedulable: Some(true),
                            queue_position: Some(position),
                            next_poll_at: Some(Some(now + SYNC_MIN_INTERVAL_MS)),
                            ..SchedulingPatch::default()
                        }),
                    )
                    .await?;
                Ok(became)
            }
            "queued" => {
                self.repo
                    .update_scheduling_snapshot(
                        id,
                        patch(SchedulingPatch {
                            schedulable: Some(false),
                            queue_position: Some(position),
                            ..SchedulingPatch::default()
                        }),
                    )
                    .await?;
                Ok(false)
            }
            "active" => {
                self.repo
                    .update_scheduling_snapshot(
                        id,
                        patch(SchedulingPatch {
                            queue_position: Some(None),
                            ..SchedulingPatch::default()
                        }),
                    )
                    .await?;
                Ok(false)
            }
            "expired" => {
                // 排队中的废票同 task_id 重取号；paused 停在原地等手动 Continue。
                if task["status"] == "queued" {
                    self.repo
                        .update_scheduling_snapshot(
                            id,
                            patch(SchedulingPatch {
                                schedulable: Some(false),
                                queue_position: Some(None),
                                ..SchedulingPatch::default()
                            }),
                        )
                        .await?;
                    self.retake_ticket(id).await;
                }
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    /// 终态核销：4xx 当作幂等 ack；网络 / 5xx 留到下个周期补报。
    async fn settle_one(&self, task: &Value) {
        let id = task["offPeakTaskId"].as_str().unwrap_or_default();
        let Some(ticket_id) = task["serverTicketId"].as_str() else {
            let _ = self.repo.mark_settled(id, self.now()).await;
            return;
        };
        match self.client.settle(ticket_id).await {
            Ok(()) => {
                let _ = self.repo.mark_settled(id, self.now()).await;
            }
            Err(ClientError::Server(error)) if error.http_status < 500 => {
                let _ = self.repo.mark_settled(id, self.now()).await;
            }
            Err(error) => log::warn(format!(
                "off-peak settle failed ticket={ticket_id}: {error}"
            )),
        }
    }
}

impl Drop for OffPeakService {
    fn drop(&mut self) {
        if let Some(timer) = self.lock().timer.take() {
            timer.abort();
        }
    }
}
