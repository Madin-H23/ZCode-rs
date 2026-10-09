//! 结算与派发请求（scheduler/index.ts 的 settleDispatchResult、postDispatchRequest，
//! manualClaimRelease.ts，offPeakDispatchSettlement.ts）。
use std::collections::HashMap;

use anyhow::anyhow;
use serde_json::{Map, Value, json};
use zcode_host_kit::log;

use super::DispatchFailure;
use crate::automation::schedule::compute_automation_next_run_at;
use crate::automation::time::Zone;
use crate::tasks_index::automation_repo::{AutomationRepo, DispatchFailed, FailureKind};
use crate::tasks_index::off_peak_repo::{OffPeakRepo, RunningUpdate, TerminalUpdate};

const OFF_PEAK_DISPATCH_RETRY_BASE_MS: i64 = 30_000;
const OFF_PEAK_DISPATCH_RETRY_CAP_MS: i64 = 15 * 60_000;

/// 运行的触发方式（`ZCodeAutomationTrigger`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Trigger {
    Schedule,
    Manual,
}

/// 在途派发的上下文（runId → 它），结算与退出释放用。
#[derive(Debug, Clone, PartialEq)]
pub(super) struct InFlight {
    pub automation_id: String,
    pub workspace_key: String,
    pub trigger: Trigger,
}

/// JS 真值：字符串非空。
fn truthy_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value[key].as_str().filter(|text| !text.is_empty())
}

/// `computeAutomationNextRunAt`：有规则按规则，否则按 cron；表达式非法时报错（同 `new Cron` 抛错）。
pub(super) fn next_run_at(
    zone: &dyn Zone,
    automation: &Value,
    from: i64,
) -> anyhow::Result<Option<i64>> {
    compute_automation_next_run_at(
        zone,
        automation["cronExpr"].as_str().unwrap_or_default(),
        automation.get("scheduleRule").and_then(Value::as_object),
        from,
    )
    .map_err(|message| anyhow!(message))
}

/// `postDispatchRequest` 经 Main 翻译后的 `cron-run` 消息（空值字段省略，同 TS 的条件展开）。
pub(super) fn cron_run_message(
    automation: &Value,
    run_id: &str,
    fixed_selection: Option<&Value>,
) -> Value {
    let mut message = Map::new();
    message.insert("type".into(), json!(crate::automation::dispatch::CRON_RUN));
    message.insert("automationId".into(), automation["automationId"].clone());
    message.insert("runId".into(), json!(run_id));
    message.insert("prompt".into(), automation["prompt"].clone());
    if let Some(task) = truthy_str(automation, "targetTaskId") {
        message.insert("targetTaskId".into(), json!(task));
    }
    if let Some(selection) = fixed_selection.or_else(|| automation.get("modelSelection")) {
        message.insert("modelSelection".into(), selection.clone());
    }
    if let Some(mode) = truthy_str(automation, "mode") {
        message.insert("mode".into(), json!(mode));
    }
    message.insert("workspacePath".into(), automation["workspacePath"].clone());
    if let Some(identity) = truthy_str(automation, "workspaceIdentity") {
        message.insert("workspaceIdentity".into(), json!(identity));
    }
    Value::Object(message)
}

/// `offpeak-dispatch-request` 经 Main 翻译后的 `off-peak-run` 消息。
pub(super) fn off_peak_run_message(task: &Value) -> Value {
    let mut message = Map::new();
    message.insert(
        "type".into(),
        json!(crate::off_peak::dispatch::OFF_PEAK_RUN),
    );
    for key in [
        "offPeakTaskId",
        "prompt",
        "permissionMode",
        "modelSelection",
    ] {
        message.insert(key.into(), task[key].clone());
    }
    for key in ["conversationId", "sessionId", "serverTicketId"] {
        if let Some(value) = truthy_str(task, key) {
            message.insert(key.into(), json!(value));
        }
    }
    message.insert("workspacePath".into(), task["workspacePath"].clone());
    if let Some(identity) = truthy_str(task, "workspaceIdentity") {
        message.insert("workspaceIdentity".into(), json!(identity));
    }
    Value::Object(message)
}

pub(super) fn manual_claim_skipped(automation_id: &str, run_id: &str) -> String {
    format!(
        "manual claim release skipped: workspaceKey missing automation={automation_id} runId={run_id}"
    )
}

/// `releaseManualClaimForSettledRun`：在途上下文丢失时用 run 台账或 automation 兜回 workspaceKey。
/// 兜不回时记 error 并跳过（返回 false），锁等认领超时回收。
pub(super) async fn release_manual_claim(
    repo: &AutomationRepo,
    automation_id: &str,
    run_id: &str,
    workspace_key: Option<String>,
) -> anyhow::Result<bool> {
    let key = match workspace_key {
        Some(key) => Some(key),
        None => match repo.get_run(run_id).await? {
            Some(run) => run["workspaceKey"].as_str().map(str::to_owned),
            None => None,
        },
    };
    let key = match key {
        Some(key) => Some(key),
        None => repo
            .get(automation_id, None)
            .await?
            .and_then(|automation| automation["workspaceKey"].as_str().map(str::to_owned)),
    };
    let Some(key) = key else {
        log::error(format!(
            "[scheduler] {}",
            manual_claim_skipped(automation_id, run_id)
        ));
        return Ok(false);
    };
    repo.release_manual_claim(automation_id, &key).await?;
    Ok(true)
}

/// `settleDispatchResult`：automation 一次派发的结算。
pub(super) async fn settle_cron(
    repo: &AutomationRepo,
    zone: &dyn Zone,
    context: Option<InFlight>,
    run_id: &str,
    result: Result<String, DispatchFailure>,
    now: i64,
) -> anyhow::Result<()> {
    // 上下文丢失时从 runId 还原 automationId 与触发方式（与 Node 的迟到回报兜底相同）。
    let automation_id = context.as_ref().map_or_else(
        || run_id.split(':').next().unwrap_or_default().to_owned(),
        |context| context.automation_id.clone(),
    );
    let trigger = context.as_ref().map_or_else(
        || {
            if run_id.contains(":manual:") {
                Trigger::Manual
            } else {
                Trigger::Schedule
            }
        },
        |context| context.trigger,
    );
    let workspace_key = context.map(|context| context.workspace_key);
    match result {
        Ok(session_id) => {
            // `sessionId ?? null`：空串也照写（与 Node 相同）。
            let session_id = Some(session_id);
            if trigger == Trigger::Manual {
                // Host ok 只表示 prompt 已受理；single-flight 锁由 Host 在会话终态时释放。
                repo.mark_manual_run_dispatched(run_id, session_id, now)
                    .await?;
                return Ok(());
            }
            repo.mark_run_dispatch(run_id, "dispatched", session_id, None)
                .await?;
            let next = match repo.get(&automation_id, None).await? {
                Some(automation) => next_run_at(zone, &automation, now)?,
                None => None,
            };
            repo.mark_dispatched(&automation_id, now, next).await
        }
        Err(failure) => {
            repo.mark_run_dispatch(
                run_id,
                "failed_to_dispatch",
                None,
                Some(failure.error.clone()),
            )
            .await?;
            if trigger == Trigger::Manual {
                release_manual_claim(repo, &automation_id, run_id, workspace_key).await?;
                return Ok(());
            }
            // transient 达上限后循环任务跳到下一个正常 next_run_at。
            let next = match repo.get(&automation_id, None).await? {
                Some(automation) => next_run_at(zone, &automation, now)?,
                None => None,
            };
            repo.mark_dispatch_failed(
                &automation_id,
                DispatchFailed {
                    failed_at: now,
                    error: failure.error,
                    kind: failure.kind,
                    next_run_at: next,
                },
            )
            .await
        }
    }
}

/// 闲时任务的进程内退避表：offPeakTaskId → 下次允许派发时间 / 已失败次数。重启即清空。
#[derive(Debug, Default)]
pub(super) struct OffPeakRetry {
    retry_at: HashMap<String, i64>,
    attempts: HashMap<String, i64>,
}

impl OffPeakRetry {
    pub fn retry_at(&self, id: &str) -> i64 {
        self.retry_at.get(id).copied().unwrap_or(0)
    }

    fn clear(&mut self, id: &str) {
        self.retry_at.remove(id);
        self.attempts.remove(id);
    }

    #[cfg(test)]
    pub fn snapshot(
        &self,
    ) -> (
        std::collections::BTreeMap<String, i64>,
        std::collections::BTreeMap<String, i64>,
    ) {
        (
            self.retry_at.clone().into_iter().collect(),
            self.attempts.clone().into_iter().collect(),
        )
    }
}

/// `settleOffPeakDispatchResult`：只有 transient 回 queued 退避；permanent（缺模型、缺凭据等确定性
/// 配置错误）直接 failed，避免永久等待并反复消耗服务端 ticket。
pub(super) async fn settle_off_peak(
    repo: &OffPeakRepo,
    retry: &mut OffPeakRetry,
    id: &str,
    result: Result<String, DispatchFailure>,
    now: i64,
) -> anyhow::Result<()> {
    let failure = match result {
        Ok(session_id) => {
            retry.clear(id);
            let session_id = Some(session_id).filter(|id| !id.is_empty());
            let update = RunningUpdate {
                started_at: now,
                conversation_id: session_id.clone(),
                session_id,
                server_ticket_id: None,
            };
            if repo.mark_running(id, update).await?.is_none() {
                repo.release_claim(id, None, Some(now)).await?;
                log::warn(format!(
                    "[scheduler] off-peak dispatch result dropped (task no longer queued) task={id}"
                ));
            }
            return Ok(());
        }
        Err(failure) => failure,
    };
    let error = failure.error;
    if failure.kind == FailureKind::Permanent {
        retry.clear(id);
        let update = TerminalUpdate {
            status: "failed".into(),
            ended_at: now,
            failure_reason: Some(error.clone()),
            files_changed: None,
            dispatch_error: Some(error.clone()),
        };
        if repo.mark_terminal(id, update).await?.is_none() {
            repo.release_claim(id, None, Some(now)).await?;
            log::warn(format!(
                "[scheduler] off-peak permanent dispatch result dropped (task already changed) task={id}"
            ));
            return Ok(());
        }
        log::warn(format!(
            "[scheduler] off-peak dispatch permanently failed task={id}: {error}"
        ));
        return Ok(());
    }
    let attempts = retry.attempts.get(id).copied().unwrap_or(0) + 1;
    retry.attempts.insert(id.to_owned(), attempts);
    let exponent = u32::try_from((attempts - 1).max(0)).unwrap_or(u32::MAX);
    let backoff = 2_i64
        .checked_pow(exponent)
        .and_then(|factor| OFF_PEAK_DISPATCH_RETRY_BASE_MS.checked_mul(factor))
        .map_or(OFF_PEAK_DISPATCH_RETRY_CAP_MS, |backoff| {
            backoff.min(OFF_PEAK_DISPATCH_RETRY_CAP_MS)
        });
    retry.retry_at.insert(id.to_owned(), now + backoff);
    repo.release_claim(id, Some(error.clone()), Some(now))
        .await?;
    log::warn(format!(
        "[scheduler] off-peak dispatch failed task={id} attempts={attempts} retryIn={backoff}ms: {error}"
    ));
    Ok(())
}
