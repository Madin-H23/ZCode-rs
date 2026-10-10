//! Scheduler 的 actor：独占调度状态，按轮次认领与派发，结算派发结果（scheduler/index.ts 的 tick、
//! handleClaimed、handleClaimedManual、handleOffPeakClaimed、reportOffPeakActiveCount、dispose）。
use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use futures_util::stream::FuturesUnordered;
use futures_util::{FutureExt, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};
use zcode_async::{OwnedTask, TaskError};
use zcode_host_kit::log;

use super::settle::{self, InFlight, OffPeakRetry, Trigger};
use super::{Command, DispatchFailure, SchedulerDeps, WakeSignal};
use crate::automation::dispatch::CronRunRequest;
use crate::automation::schedule::is_one_shot;
use crate::automation::service::Clock;
use crate::off_peak::dispatch::OffPeakRunRequest;
use crate::tasks_index::automation_repo::{
    AutomationRepo, ClaimedManualRun, FailureKind, RunIdentity, SkippedRun, workspace_key,
};

/// misfire 宽限：next_run_at 早于 now 超过该值，视为关机、休眠或退出期间错过的窗口，记 skipped 不补跑。
pub(super) const MISFIRE_GRACE_MS: i64 = 5 * 60_000;
pub(super) const MISSED_WINDOW_REASON: &str = "computer_asleep_or_app_not_running";

/// 一次派发任务的结果，回到 actor 结算。
enum Settlement {
    Cron {
        run_id: String,
        result: Result<String, DispatchFailure>,
    },
    OffPeak {
        id: String,
        result: Result<String, DispatchFailure>,
    },
}

/// 派发任务 panic 时按 transient 失败结算（Node 中 Host 崩溃的派发同样只会被退避重试）。
async fn caught(
    dispatch: futures_util::future::BoxFuture<'static, Result<String, DispatchFailure>>,
) -> Result<String, DispatchFailure> {
    AssertUnwindSafe(dispatch)
        .catch_unwind()
        .await
        .unwrap_or_else(|_| {
            Err(DispatchFailure {
                error: "dispatch task panicked".into(),
                kind: FailureKind::Transient,
            })
        })
}

pub(super) struct Actor {
    deps: SchedulerDeps,
    clock: Clock,
    /// runId → 在途派发上下文；等派发结果回来后结算。
    in_flight: HashMap<String, InFlight>,
    /// 在途的闲时任务：只用于退出时释放认领。
    off_peak_in_flight: HashSet<String>,
    retry: OffPeakRetry,
    /// 上次上报的执行中计数；None 表示还没上报过。
    last_active_count: Option<u64>,
    /// 在途派发：由 actor 持有的结构化任务（丢弃即中止），actor 等待并结算它们。
    dispatches: FuturesUnordered<OwnedTask<Settlement>>,
    #[cfg(test)]
    ticks: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Actor {
    pub(super) fn new(deps: SchedulerDeps, clock: Clock) -> Self {
        Self {
            deps,
            clock,
            in_flight: HashMap::new(),
            off_peak_in_flight: HashSet::new(),
            retry: OffPeakRetry::default(),
            last_active_count: None,
            dispatches: FuturesUnordered::new(),
            #[cfg(test)]
            ticks: std::sync::Arc::default(),
        }
    }

    #[cfg(test)]
    pub(super) fn tick_counter(&self) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
        self.ticks.clone()
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    fn repo(&self) -> &AutomationRepo {
        self.deps.automations.repo()
    }

    pub(super) async fn run(mut self, mut commands: mpsc::Receiver<Command>, every: Duration) {
        let wake = self.deps.wake.clone();
        // 闲时任务中断恢复：Scheduler 是 App 级单例、先于任何派发启动，此刻 DB 里的 running
        // 必属上一个 App 实例（或崩溃前的 Host）残留，安全置回 queued（会话保留供 resume 续跑）。
        match self.deps.off_peak.recover_interrupted(self.now()).await {
            Ok(0) => {}
            Ok(recovered) => log::info(format!(
                "[scheduler] off-peak recovered {recovered} interrupted task(s) back to queued"
            )),
            Err(error) => log::error(format!(
                "[scheduler] off-peak recoverInterrupted failed: {error:#}"
            )),
        }
        log::info("[scheduler] cron scheduler started");
        let mut timer = tokio::time::interval_at(Instant::now() + every, every);
        timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
        self.tick(&wake).await;
        let disposed = loop {
            tokio::select! {
                biased;
                command = commands.recv() => break command,
                Some(joined) = self.dispatches.next(), if !self.dispatches.is_empty() => {
                    self.settle(joined, &wake).await;
                }
                () = wake.notified() => self.tick(&wake).await,
                _ = timer.tick() => self.tick(&wake).await,
            }
        };
        self.dispose().await;
        if let Some(Command::Dispose(reply)) = disposed {
            let _ = reply.send(());
        }
    }

    /// 一轮。本轮开始前到达的唤醒并入本轮；执行期间到达的唤醒留在信号里（只记一次），回到主循环后
    /// 立即再来一轮（Node 的 do … while (tickRequested && !disposed)：dispose 优先于补的这一轮）。
    async fn tick(&mut self, wake: &WakeSignal) {
        wake.take_pending();
        #[cfg(test)]
        self.ticks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Err(error) = self.tick_once().await {
            log::error(format!("[scheduler] tick failed: {error:#}"));
        }
    }

    async fn tick_once(&mut self) -> anyhow::Result<()> {
        let now = self.now();
        for automation in self.repo().claim_due(now).await? {
            self.handle_claimed(&automation, now).await?;
        }
        for manual in self.repo().claim_manual_runs(now).await? {
            self.handle_claimed_manual(manual);
        }
        for task in self.deps.off_peak.claim_due(now).await? {
            self.handle_off_peak_claimed(&task, now).await?;
        }
        // 防休眠：上报执行中计数，Main 据此与设置决定 powerSaveBlocker。
        self.report_active_count().await;
        Ok(())
    }

    async fn handle_claimed(&mut self, automation: &Value, now: i64) -> anyhow::Result<()> {
        let automation_id = automation["automationId"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let next_run_at = automation["nextRunAt"].as_i64();
        // 派发时间戳优先用 next_run_at（重试期间不变，保证 runId 稳定），退到 retry_at / now。
        let scheduled_at = next_run_at
            .or_else(|| automation["retryAt"].as_i64())
            .unwrap_or(now);
        let run_id = format!("{automation_id}:{scheduled_at}");
        let key = workspace_key(
            automation["workspacePath"].as_str().unwrap_or_default(),
            automation["workspaceIdentity"].as_str(),
        );
        let is_retry = automation["dispatchAttempts"].as_i64().unwrap_or(0) > 0;
        // misfire：首轮（非重试）且计划触发时间已远早于 now → 认定错过窗口，跳过不补跑。
        let missed = !is_retry && next_run_at.is_some_and(|next| next <= now - MISFIRE_GRACE_MS);
        if missed {
            // 纯一次性任务错过窗口即终态，不能再从兼容性 scheduleRule 推导出下一次执行。
            let finalize = is_one_shot(
                automation["recurring"].as_bool().unwrap_or(false),
                automation["maxRuns"].as_i64(),
            );
            let next = if finalize {
                None
            } else {
                settle::next_run_at(self.deps.automations.zone(), automation, now)?
            };
            self.repo()
                .skip_and_reschedule(SkippedRun {
                    automation_id: automation_id.clone(),
                    run_id,
                    workspace_key: key,
                    scheduled_at: Some(scheduled_at),
                    reason: MISSED_WINDOW_REASON.into(),
                    next_run_at: next,
                    finalize,
                })
                .await?;
            log::info(format!(
                "[scheduler] skip missed window automation={automation_id} scheduledAt={scheduled_at}{}",
                if finalize { " finalized=one-shot" } else { "" }
            ));
            return Ok(());
        }
        // 正常派发：先落 / 更新 run 台账（claimed），再派发。模型选择由目标 Host 首次派发时固定。
        let run = RunIdentity {
            run_id: run_id.clone(),
            automation_id: automation_id.clone(),
            workspace_key: key.clone(),
            scheduled_at: Some(scheduled_at),
            trigger: "schedule".into(),
        };
        self.repo().upsert_run_claimed(&run, None).await?;
        let fixed = self
            .repo()
            .get_run(&run_id)
            .await?
            .and_then(|run| run.get("modelSelection").cloned());
        self.in_flight.insert(
            run_id.clone(),
            InFlight {
                automation_id,
                workspace_key: key,
                trigger: Trigger::Schedule,
            },
        );
        self.dispatch_cron(automation, &run_id, fixed.as_ref());
        Ok(())
    }

    fn handle_claimed_manual(&mut self, claimed: ClaimedManualRun) {
        let automation = &claimed.automation;
        let run_id = claimed.run["runId"].as_str().unwrap_or_default().to_owned();
        self.in_flight.insert(
            run_id.clone(),
            InFlight {
                automation_id: automation["automationId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                workspace_key: workspace_key(
                    automation["workspacePath"].as_str().unwrap_or_default(),
                    automation["workspaceIdentity"].as_str(),
                ),
                trigger: Trigger::Manual,
            },
        );
        self.dispatch_cron(automation, &run_id, claimed.run.get("modelSelection"));
    }

    /// 派发在独立任务中执行；请求按 `cron-run` 的形状构造并按同一 schema 校验。
    fn dispatch_cron(&mut self, automation: &Value, run_id: &str, fixed: Option<&Value>) {
        let message = settle::cron_run_message(automation, run_id, fixed);
        let Some(request) = CronRunRequest::parse(&message) else {
            // 与 Node Host 收到不合法的 cron-run 相同：不派发、不结算，等认领超时回收。
            log::warn(format!(
                "[scheduler] ignored invalid cron dispatch request runId={run_id}"
            ));
            return;
        };
        let dispatch = self.deps.cron.dispatch(request);
        let run_id = run_id.to_owned();
        self.dispatches.push(zcode_async::owned(async move {
            Settlement::Cron {
                run_id,
                result: caught(dispatch).await,
            }
        }));
    }

    /// 退避中的任务立即释放认领等下轮（进程内退避表；每轮 claim + release 两次写，任务数小，可忽略）。
    async fn handle_off_peak_claimed(&mut self, task: &Value, now: i64) -> anyhow::Result<()> {
        let id = task["offPeakTaskId"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if self.retry.retry_at(&id) > now {
            return self.deps.off_peak.release_claim(&id, None, Some(now)).await;
        }
        self.off_peak_in_flight.insert(id.clone());
        let message = settle::off_peak_run_message(task);
        let Some(request) = OffPeakRunRequest::parse(&message) else {
            log::warn(format!(
                "[scheduler] ignored invalid off-peak dispatch request task={id}"
            ));
            return Ok(());
        };
        let dispatch = self.deps.off_peak_dispatch.dispatch(request);
        let task_id = id.clone();
        self.dispatches.push(zcode_async::owned(async move {
            Settlement::OffPeak {
                id: task_id,
                result: caught(dispatch).await,
            }
        }));
        log::info(format!("[scheduler] off-peak dispatch requested task={id}"));
        Ok(())
    }

    /// 执行中计数：只在值变化时上报。
    async fn report_active_count(&mut self) {
        match self.deps.off_peak.count_active().await {
            Ok(count) if self.last_active_count != Some(count) => {
                self.last_active_count = Some(count);
                (self.deps.active_count)(count);
            }
            Ok(_) => {}
            Err(error) => log::warn(format!(
                "[scheduler] off-peak active count report failed: {error:#}"
            )),
        }
    }

    async fn settle(&mut self, joined: Result<Settlement, TaskError>, wake: &WakeSignal) {
        let now = self.now();
        match joined {
            Ok(Settlement::Cron { run_id, result }) => {
                let context = self.in_flight.remove(&run_id);
                let settled = settle::settle_cron(
                    self.repo(),
                    self.deps.automations.zone(),
                    context,
                    &run_id,
                    result,
                    now,
                )
                .await;
                match settled {
                    // manual run 可能因同一 automation 已有派发在途而暂未认领；结算释放 single-flight
                    // 锁后立即再来一轮，不等 20 s 轮询。
                    Ok(()) => wake.wake(),
                    Err(error) => log::error(format!(
                        "[scheduler] settle dispatch result failed runId={run_id}: {error:#}"
                    )),
                }
            }
            Ok(Settlement::OffPeak { id, result }) => {
                self.off_peak_in_flight.remove(&id);
                if let Err(error) =
                    settle::settle_off_peak(&self.deps.off_peak, &mut self.retry, &id, result, now)
                        .await
                {
                    log::error(format!(
                        "[scheduler] settle off-peak dispatch result failed task={id}: {error:#}"
                    ));
                }
            }
            // 派发任务只在退出时被取消；panic 已在任务内转成失败。
            Err(error) => log::error(format!("[scheduler] dispatch task ended: {error}")),
        }
    }

    /// 退出：先结算已经完成的派发，再取消在途派发，最后释放仍在途的认领（避免下次启动等认领超时）。
    async fn dispose(&mut self) {
        let wake = self.deps.wake.clone();
        for dispatch in self.dispatches.iter() {
            dispatch.abort();
        }
        while let Some(joined) = self.dispatches.next().await {
            // 取消前已经完成的派发（prompt 已发出）照常结算，不能当作未派发释放。
            if !matches!(&joined, Err(TaskError::Aborted)) {
                self.settle(joined, &wake).await;
            }
        }
        for (_, context) in std::mem::take(&mut self.in_flight) {
            // 尽力而为：退出路径不报错。
            let _ = match context.trigger {
                Trigger::Manual => {
                    self.repo()
                        .release_manual_claim(&context.automation_id, &context.workspace_key)
                        .await
                }
                Trigger::Schedule => self.repo().release_claim(&context.automation_id).await,
            };
        }
        for id in std::mem::take(&mut self.off_peak_in_flight) {
            let _ = self.deps.off_peak.release_claim(&id, None, None).await;
        }
        log::info("[scheduler] cron scheduler disposed");
    }
}
