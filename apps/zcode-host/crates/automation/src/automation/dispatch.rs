//! 定时任务派发（packages/desktop/src/host/index.ts 的 dispatchCronRun / dispatchManualAutomationRun
//! 与 cronRunLifecycle.ts、automationModelSelection.ts、cronBotDelivery.ts）。
//!
//! Host 内的 Scheduler（docs/specs/rust-u4-app-host.md §2.4）认领到期项后调用派发器，请求沿用
//! `cron-run` 的形状；`runAutomationNow` 直接派发 manual run。两者都走 [`Dispatcher::dispatch`]，
//! 再按会话终态结算 run。
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};
use zcode_async::Task;
use zcode_host_kit::log;

use super::Area;
use super::driver::{CreateSession, ResumeSession, SendText, TaskDriver, TerminalOutcome};
use super::service::AutomationService;
use crate::agent::WorkspaceTarget;
use crate::providers::ProviderRuntime;
use crate::tasks_index::automation_repo::{RunIdentity, workspace_key};

pub const CRON_RUN: &str = "cron-run";
const MANUAL_CLAIM_HEARTBEAT: Duration = Duration::from_secs(60);

/// `hostCronRunMessageSchema`（manual 直派也用同一形状）。
#[derive(Debug, Clone, PartialEq)]
pub struct CronRunRequest {
    pub automation_id: String,
    pub run_id: String,
    pub prompt: String,
    pub target_task_id: Option<String>,
    pub model_selection: Option<Value>,
    pub mode: Option<String>,
    pub workspace_path: String,
    pub workspace_identity: Option<String>,
}

fn non_empty(value: &Value, key: &str) -> Option<String> {
    value[key]
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
}

impl CronRunRequest {
    /// 按 zod schema 校验；不合法时为 None（Main 的消息解析失败，Node Host 直接忽略）。
    pub fn parse(message: &Value) -> Option<Self> {
        Some(Self {
            automation_id: non_empty(message, "automationId")?,
            run_id: non_empty(message, "runId")?,
            prompt: non_empty(message, "prompt")?,
            target_task_id: match message.get("targetTaskId") {
                None | Some(Value::Null) => None,
                Some(_) => Some(non_empty(message, "targetTaskId")?),
            },
            model_selection: message
                .get("modelSelection")
                .filter(|value| !value.is_null())
                .cloned(),
            mode: message["mode"].as_str().map(str::to_owned),
            workspace_path: non_empty(message, "workspacePath")?,
            workspace_identity: message["workspaceIdentity"].as_str().map(str::to_owned),
        })
    }

    fn target(&self) -> WorkspaceTarget {
        WorkspaceTarget {
            workspace_path: self.workspace_path.clone(),
            workspace_identity: self.workspace_identity.clone(),
            remote_session_id: None,
        }
    }
}

/// `isRemoteWorkspaceIdentity`（remote-workspace-identity.ts 的解析规则）。
pub fn is_remote_workspace_identity(identity: &str) -> bool {
    let Some(rest) = identity.strip_prefix("remote:") else {
        return false;
    };
    let Some((kind, mut rest)) = rest.split_once(':') else {
        return false;
    };
    let segments = match kind {
        "ssh" => 3,
        "wsl" | "docker" => 1,
        _ => return false,
    };
    for _ in 0..segments {
        match rest.split_once(':') {
            Some((segment, tail)) if !segment.is_empty() => rest = tail,
            _ => return false,
        }
    }
    if kind == "wsl" && !rest.starts_with('/') {
        match rest.split_once(':') {
            Some((user, tail)) if !user.is_empty() => rest = tail,
            _ => return false,
        }
    }
    rest.starts_with('/')
}

/// `parseCronRunScheduledAt`：`<automationId>:<scheduledAt>[:...]`。
fn scheduled_at(run_id: &str, automation_id: &str) -> Option<i64> {
    let rest = run_id.strip_prefix(&format!("{automation_id}:"))?;
    let value: i64 = rest.split(':').next()?.parse().ok()?;
    (value > 0).then_some(value)
}

struct Tracked {
    run: RunIdentity,
    target: WorkspaceTarget,
    session_id: String,
    heartbeat: Option<Task<()>>,
}

pub struct Dispatcher {
    area: Arc<Area>,
    service: Arc<AutomationService>,
    driver: Arc<TaskDriver>,
    tracked: Mutex<HashMap<String, Tracked>>,
}

fn track_key(workspace_key: &str, session_id: &str, input_id: &str) -> String {
    format!("{workspace_key}\u{0}{session_id}\u{0}{input_id}")
}

fn reasoning_level(selection: &Value) -> Option<String> {
    selection["options"]["reasoningLevel"]
        .as_str()
        .filter(|level| !level.is_empty())
        .map(str::to_owned)
}

impl Dispatcher {
    pub fn start(
        area: Arc<Area>,
        service: Arc<AutomationService>,
        driver: Arc<TaskDriver>,
    ) -> Arc<Self> {
        let dispatcher = Arc::new(Self {
            area,
            service,
            driver: driver.clone(),
            tracked: Mutex::new(HashMap::new()),
        });
        let mut outcomes = Box::pin(driver.terminal_outcomes());
        let weak = Arc::downgrade(&dispatcher);
        drop(
            dispatcher
                .area
                .spawner
                .spawn_resident("automation-settle", async move {
                    while let Some(outcome) = outcomes.next().await {
                        let Some(dispatcher) = weak.upgrade() else {
                            return;
                        };
                        dispatcher.settle(outcome).await;
                    }
                }),
        );
        dispatcher
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Tracked>> {
        self.tracked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn untrack(&self, key: &str) {
        if let Some(tracked) = self.lock().remove(key)
            && let Some(heartbeat) = tracked.heartbeat
        {
            heartbeat.abort();
        }
    }

    /// `resolveAutomationSubmissionModelSelection`：已固定的 run 直接复用；否则读持久层的新版意图，
    /// 经 model-selection 视图校验；跟随 workspace 时取目标 Host 的首选模型。
    async fn resolve_selection(
        &self,
        request: &CronRunRequest,
        key: &str,
    ) -> Result<Value, String> {
        let repo = self.service.repo();
        let fixed = repo
            .get_run(&request.run_id)
            .await
            .map_err(|error| error.to_string())?
            .and_then(|run| run.get("modelSelection").cloned());
        if let Some(fixed) = fixed {
            return Ok(fixed);
        }
        let selection = repo
            .get_model_selection_for_dispatch(&request.automation_id, key)
            .await
            .map_err(|error| error.to_string())?;
        let providers: &ProviderRuntime = &self.area.providers;
        let (revision, resolved) = providers.snapshot().await;
        match selection {
            Some(selection) => {
                let view =
                    resolved.model_selection_view(revision, Some(&json!({"selection": selection})));
                let effective = &view["effectiveSelection"];
                if view
                    .get("selectionIssue")
                    .is_some_and(|issue| !issue.is_null())
                    || reasoning_level(effective).is_none()
                {
                    return Err("Automation 模型选择不可用，请重新选择模型与思考档位".into());
                }
                Ok(effective.clone())
            }
            None => {
                let view = resolved.model_selection_view(revision, None);
                let preferred = &view["preferredSelection"];
                if reasoning_level(preferred).is_none() {
                    return Err("Automation 无法从目标 Host 解析首选模型".into());
                }
                Ok(preferred.clone())
            }
        }
    }

    /// 把一次 cron / manual run 提交给本 Host；返回承载本次运行的会话 id。
    pub async fn dispatch(self: &Arc<Self>, request: &CronRunRequest) -> Result<String, String> {
        // 远端 workspace 不在本阶段：远程身份只能失败，不能落到本地 Host 执行。
        if request
            .workspace_identity
            .as_deref()
            .is_some_and(is_remote_workspace_identity)
        {
            return Err("Automation 目标 Remote Host 当前不可用".into());
        }
        let key = workspace_key(
            &request.workspace_path,
            request.workspace_identity.as_deref(),
        );
        let selection = self.resolve_selection(request, &key).await?;
        let selection = self
            .service
            .repo()
            .fix_run_model_selection(&request.run_id, &selection)
            .await
            .map_err(|error| error.to_string())?;
        let trigger = if request.run_id.contains(":manual:") {
            "manual"
        } else {
            "schedule"
        };
        let run = RunIdentity {
            run_id: request.run_id.clone(),
            automation_id: request.automation_id.clone(),
            workspace_key: key.clone(),
            scheduled_at: scheduled_at(&request.run_id, &request.automation_id),
            trigger: trigger.into(),
        };
        let mut tracked_key = None;
        let result = self
            .submit(request, &run, &selection, &mut tracked_key)
            .await;
        if let Err(error) = &result {
            if let Some(tracked_key) = tracked_key {
                self.untrack(&tracked_key);
            }
            self.record_outcome(&run, "failed", Some(error.clone()))
                .await;
        }
        result
    }

    async fn submit(
        self: &Arc<Self>,
        request: &CronRunRequest,
        run: &RunIdentity,
        selection: &Value,
        tracked_key: &mut Option<String>,
    ) -> Result<String, String> {
        let target = request.target();
        let level = reasoning_level(selection);
        let error = |error: zcode_host_kit::RpcError| error.message;
        let task_id = match &request.target_task_id {
            Some(task_id) => {
                // 绑定会话在重启或切换 workspace 后通常不处于 active：先恢复，再应用保存的运行参数。
                self.driver
                    .resume_session(
                        &target,
                        task_id,
                        ResumeSession {
                            thought_level: level.clone(),
                            automation_id: Some(request.automation_id.clone()),
                            off_peak_task_id: None,
                        },
                    )
                    .await
                    .map_err(error)?;
                self.driver
                    .set_automation_session_config(
                        &target,
                        task_id,
                        selection,
                        level.as_deref(),
                        request.mode.as_deref(),
                    )
                    .await
                    .map_err(error)?;
                task_id.clone()
            }
            None => {
                let meta = self
                    .driver
                    .create_session(
                        &target,
                        CreateSession {
                            model: Some(selection.clone()),
                            thought_level: level.clone(),
                            mode: request.mode.clone(),
                            automation_id: Some(request.automation_id.clone()),
                            off_peak_task_id: None,
                            defer_persistence: false,
                        },
                    )
                    .await
                    .map_err(error)?;
                meta["taskId"].as_str().unwrap_or_default().to_owned()
            }
        };
        self.watch_bot_delivery(request, &run.workspace_key, &target, &task_id)
            .await;
        // 先登记终态订阅、记 running，再发 prompt，避免快速完成的轮次漏掉结算。
        let key = track_key(&run.workspace_key, &task_id, &run.run_id);
        let heartbeat = (run.trigger == "manual").then(|| self.start_heartbeat(run));
        self.untrack(&key);
        self.lock().insert(
            key.clone(),
            Tracked {
                run: run.clone(),
                target: target.clone(),
                session_id: task_id.clone(),
                heartbeat,
            },
        );
        *tracked_key = Some(key);
        self.record_outcome(run, "running", None).await;
        self.driver
            .send_text(
                &target,
                &task_id,
                SendText {
                    input_id: run.run_id.clone(),
                    text: request.prompt.clone(),
                    automation_id: Some(request.automation_id.clone()),
                    ..SendText::default()
                },
            )
            .await
            .map_err(error)?;
        // 绑定会话（targetTaskId）只是原会话的后续输入，不计 session_create（hostSessionCreateTelemetry）。
        if request.target_task_id.is_none() {
            crate::integration::report_session_create(
                &self.area.bridge,
                &self.area.context.environment.vars,
                crate::integration::SessionCreate {
                    session_id: &task_id,
                    message_id: &run.run_id,
                    source: crate::integration::CreateSource::AutomationScheduled,
                    workspace_identity: request.workspace_identity.as_deref(),
                },
            );
        }
        Ok(task_id)
    }

    async fn watch_bot_delivery(
        &self,
        request: &CronRunRequest,
        key: &str,
        target: &WorkspaceTarget,
        task_id: &str,
    ) {
        let Some(delivery) = self.area.bot_delivery.get() else {
            return;
        };
        let result = match self
            .service
            .repo()
            .get_bot_delivery_target(&request.automation_id, Some(key.to_owned()))
            .await
        {
            Ok(Some(bot_target)) => {
                delivery
                    .watch_automation_run(bot_target, task_id.to_owned(), target.clone())
                    .await
            }
            Ok(None) => Ok(()),
            Err(error) => Err(error.to_string()),
        };
        // Bot 回推是 best-effort 辅助通道，失败不能阻断派发与结算。
        if let Err(error) = result {
            log::warn(format!(
                "automation Bot delivery subscription failed automation={} provider=unknown: {error}",
                request.automation_id
            ));
        }
    }

    /// manual run 执行期间每 60 s 续租一次锁，避免长任务被 scheduler 当作僵尸回收。
    fn start_heartbeat(&self, run: &RunIdentity) -> Task<()> {
        let repo = self.service.repo().clone();
        let run = run.clone();
        self.area
            .spawner
            .spawn("automation-claim-heartbeat", async move {
                let mut interval = tokio::time::interval(MANUAL_CLAIM_HEARTBEAT);
                interval.tick().await;
                loop {
                    interval.tick().await;
                    if let Err(error) = repo
                        .touch_manual_claim(&run.automation_id, &run.workspace_key)
                        .await
                    {
                        log::warn(format!(
                            "续租 manual automation claim 失败 automation={} runId={}: {error:#}",
                            run.automation_id, run.run_id
                        ));
                    }
                }
            })
    }

    /// `recordCronRunOutcomeBestEffort`。
    async fn record_outcome(&self, run: &RunIdentity, outcome: &str, error: Option<String>) {
        let repo = self.service.repo();
        let result = async {
            repo.ensure_run_claimed(run).await?;
            repo.mark_run_outcome(&run.run_id, outcome, error).await
        }
        .await;
        if let Err(error) = result {
            log::warn(format!(
                "回写定时任务运行结果失败 automation={} runId={}: {error:#}",
                run.automation_id, run.run_id
            ));
        }
    }

    async fn release_manual_claim(&self, run: &RunIdentity) {
        if let Err(error) = self
            .service
            .repo()
            .release_manual_claim(&run.automation_id, &run.workspace_key)
            .await
        {
            log::warn(format!(
                "释放 manual automation claim 失败 automation={} runId={}: {error:#}",
                run.automation_id, run.run_id
            ));
        }
    }

    /// 会话终态：只结算本 Host 派发、且 inputId 匹配的轮次。
    async fn settle(&self, outcome: TerminalOutcome) {
        let Some(input_id) = &outcome.input_id else {
            return;
        };
        let key = track_key(&outcome.workspace_key, &outcome.session_id, input_id);
        let Some(tracked) = self.lock().remove(&key) else {
            return;
        };
        if let Some(heartbeat) = &tracked.heartbeat {
            heartbeat.abort();
        }
        self.record_outcome(&tracked.run, outcome.outcome, None)
            .await;
        if tracked.run.trigger == "manual" {
            self.release_manual_claim(&tracked.run).await;
        }
        // 定时任务在后台完成后统一置为未读，打开 task 时再由导航链路清除。
        self.driver
            .mark_unread(&tracked.target, &tracked.session_id)
            .await;
    }

    /// `runAutomationNow`：抢锁成功后直接派发，返回 `{status}`。
    pub async fn run_now(
        self: &Arc<Self>,
        automation_id: &str,
        scope: &super::service::Scope,
    ) -> Result<Value, String> {
        let claimed = self
            .service
            .run_now(automation_id, Some(scope))
            .await
            .map_err(|error| error.to_string())?;
        let Some(claimed) = claimed else {
            // single-flight 已拒绝重复运行：不能伪装成新 run 已入队。
            return Ok(json!({"status": "duplicate"}));
        };
        let automation = &claimed.automation;
        let run = &claimed.run;
        let request = CronRunRequest {
            automation_id: automation["automationId"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            run_id: run["runId"].as_str().unwrap_or_default().to_owned(),
            prompt: automation["prompt"].as_str().unwrap_or_default().to_owned(),
            target_task_id: automation["targetTaskId"].as_str().map(str::to_owned),
            model_selection: run
                .get("modelSelection")
                .or_else(|| automation.get("modelSelection"))
                .cloned(),
            mode: automation["mode"].as_str().map(str::to_owned),
            workspace_path: automation["workspacePath"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            workspace_identity: automation["workspaceIdentity"].as_str().map(str::to_owned),
        };
        log::info(format!(
            "direct manual automation dispatch started automation={} runId={}",
            request.automation_id, request.run_id
        ));
        let session_id = match self.dispatch(&request).await {
            Ok(session_id) => session_id,
            Err(error) => {
                log::warn(format!(
                    "direct manual automation dispatch failed automation={} runId={}: {error}",
                    request.automation_id, request.run_id
                ));
                self.settle_manual_failure(&request, &error).await;
                return Err(error);
            }
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as i64)
            .unwrap_or_default();
        // prompt 已 accepted：台账回写失败不能伪装成派发失败，锁仍由真实终态释放。
        if let Err(error) = self
            .service
            .repo()
            .mark_manual_run_dispatched(&request.run_id, Some(session_id.clone()), now)
            .await
        {
            log::warn(format!(
                "回写 manual automation dispatched 状态与运行次数失败 automation={} runId={}: {error:#}",
                request.automation_id, request.run_id
            ));
        }
        log::info(format!(
            "direct manual automation dispatch accepted automation={} runId={} taskId={session_id}",
            request.automation_id, request.run_id
        ));
        Ok(json!({"status": "queued"}))
    }

    /// `settleManualDispatchFailureBestEffort`：记 failed_to_dispatch 并释放锁。
    async fn settle_manual_failure(&self, request: &CronRunRequest, error: &str) {
        let key = workspace_key(
            &request.workspace_path,
            request.workspace_identity.as_deref(),
        );
        if let Err(write_error) = self
            .service
            .repo()
            .mark_run_dispatch(
                &request.run_id,
                "failed_to_dispatch",
                None,
                Some(error.to_owned()),
            )
            .await
        {
            log::warn(format!(
                "回写 manual automation 派发失败状态失败 automation={} runId={}: {write_error:#}",
                request.automation_id, request.run_id
            ));
        }
        self.release_manual_claim(&RunIdentity {
            run_id: request.run_id.clone(),
            automation_id: request.automation_id.clone(),
            workspace_key: key,
            scheduled_at: None,
            trigger: "manual".into(),
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_remote_identities_and_scheduled_runs() {
        assert!(is_remote_workspace_identity(
            "remote:ssh:host:22:me:/srv/app"
        ));
        assert!(is_remote_workspace_identity("remote:wsl:Ubuntu:/home/me"));
        assert!(is_remote_workspace_identity(
            "remote:wsl:Ubuntu:me:/home/me"
        ));
        assert!(is_remote_workspace_identity("remote:docker:box:/work"));
        assert!(!is_remote_workspace_identity("remote:ssh:host:22:/srv"));
        assert!(!is_remote_workspace_identity("/Users/me/project"));
        assert_eq!(
            scheduled_at("a-1:1700000000000", "a-1"),
            Some(1_700_000_000_000)
        );
        assert_eq!(scheduled_at("a-1:manual:uuid", "a-1"), None);
    }
}
