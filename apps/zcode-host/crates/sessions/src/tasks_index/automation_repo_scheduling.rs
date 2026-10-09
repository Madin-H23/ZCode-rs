//! 调度状态机用到的 automation 仓库方法（automationRepo.ts 的 claimDue、claimManualRuns、
//! upsertRunClaimed、markDispatched、markDispatchFailed、skipAndReschedule、releaseClaim）。
//!
//! U4 起调度状态机在 Host 内（docs/specs/rust-u4-app-host.md §2.4）。同一份 tasks-index 可能同时被
//! 另一个 App 实例的 Node scheduler 使用，所以 SQL 原样移植：认领都在 `BEGIN IMMEDIATE` 内以
//! `running 0→1` 完成，认领超时（10 分钟）后回收。TS 中 `markDispatched` / `markDispatchFailed`
//! 是不带事务的读后写，这里放进 `BEGIN IMMEDIATE`：单写者时结果相同，并发时不会丢失更新。
use rusqlite::{Connection, Row, named_params};
use serde_json::Value;

use super::{
    AutomationRepo, AutomationRow, CLAIM_STALE_MS, ClaimedManualRun, RunIdentity, RunRow, get_row,
    immediate, serialize_model_selection,
};

/// 派发失败退避（`DISPATCH_RETRY_BASE_MS` 等）。
pub const DISPATCH_RETRY_BASE_MS: i64 = 30_000;
pub const DISPATCH_RETRY_CAP_MS: i64 = 15 * 60_000;
pub const DISPATCH_MAX_ATTEMPTS: i64 = 5;

/// `computeRetryAt`：now + min(BASE × 2^(attempts−1), CAP)。
pub fn compute_retry_at(now: i64, attempts: i64) -> i64 {
    let exponent = u32::try_from((attempts - 1).max(0)).unwrap_or(u32::MAX);
    let backoff = 2_i64
        .checked_pow(exponent)
        .and_then(|factor| DISPATCH_RETRY_BASE_MS.checked_mul(factor))
        .map_or(DISPATCH_RETRY_CAP_MS, |backoff| {
            backoff.min(DISPATCH_RETRY_CAP_MS)
        });
    now + backoff
}

/// 派发失败的种类（`failureKind`）：transient 退避重试，permanent 直接终结。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Transient,
    Permanent,
}

/// `skipAndReschedule` 的参数。
#[derive(Debug, Clone, PartialEq)]
pub struct SkippedRun {
    pub automation_id: String,
    pub run_id: String,
    pub workspace_key: String,
    pub scheduled_at: Option<i64>,
    pub reason: String,
    pub next_run_at: Option<i64>,
    /// 纯一次性任务错过窗口即终态（completed + 停用 + 清空调度）。
    pub finalize: bool,
}

/// `markDispatchFailed` 的参数。
#[derive(Debug, Clone, PartialEq)]
pub struct DispatchFailed {
    pub failed_at: i64,
    pub error: String,
    pub kind: FailureKind,
    /// transient 达上限后，循环任务的下一个正常 next_run_at（调用方重算）。
    pub next_run_at: Option<i64>,
}

const STALE_CLAIM_SQL: &str = "UPDATE automations
        SET running = 0, claimed_at = NULL
        WHERE running = 1 AND claimed_at IS NOT NULL AND claimed_at <= :stale";

/// claimManualRuns 的联表查询（列别名与 TS 相同，`a_*` 为 automation、`r_*` 为 run）。
const MANUAL_RUNS_SQL: &str = "SELECT
            a.automation_id AS a_automation_id,
            a.title AS a_title,
            a.cron_expr AS a_cron_expr,
            a.prompt AS a_prompt,
            a.model AS a_model,
            a.provider AS a_provider,
            a.model_selection AS a_model_selection,
            a.mode AS a_mode,
            a.thought_level AS a_thought_level,
            a.workspace_key AS a_workspace_key,
            a.workspace_path AS a_workspace_path,
            a.workspace_identity AS a_workspace_identity,
            a.target_task_id AS a_target_task_id,
            a.location_kind AS a_location_kind,
            a.recurring AS a_recurring,
            a.max_runs AS a_max_runs,
            a.end_at AS a_end_at,
            a.schedule_rule AS a_schedule_rule,
            a.schedule_edited_by_user AS a_schedule_edited_by_user,
            a.run_count AS a_run_count,
            a.scheduled_run_count AS a_scheduled_run_count,
            a.enabled AS a_enabled,
            a.lifecycle_status AS a_lifecycle_status,
            a.next_run_at AS a_next_run_at,
            a.last_run_at AS a_last_run_at,
            a.running AS a_running,
            a.claimed_at AS a_claimed_at,
            a.dispatch_status AS a_dispatch_status,
            a.dispatch_attempts AS a_dispatch_attempts,
            a.retry_at AS a_retry_at,
            a.last_error AS a_last_error,
            a.bot_delivery_target AS a_bot_delivery_target,
            a.created_at AS a_created_at,
            a.updated_at AS a_updated_at,
            r.run_id AS r_run_id,
            r.automation_id AS r_automation_id,
            r.workspace_key AS r_workspace_key,
            r.scheduled_at AS r_scheduled_at,
            r.trigger AS r_trigger,
            r.model_selection AS r_model_selection,
            r.dispatch_status AS r_dispatch_status,
            r.outcome AS r_outcome,
            r.session_id AS r_session_id,
            r.error AS r_error,
            r.attempts AS r_attempts,
            r.created_at AS r_created_at,
            r.updated_at AS r_updated_at
          FROM automation_runs r
          JOIN automations a ON a.automation_id = r.automation_id
          WHERE r.trigger = 'manual'
            AND r.dispatch_status = 'claimed'
            AND a.running = 0
            AND (r.attempts = 0 OR r.updated_at <= :stale)
          ORDER BY r.created_at ASC";

fn read_manual_pair(row: &Row<'_>) -> rusqlite::Result<(AutomationRow, RunRow)> {
    let automation = AutomationRow {
        automation_id: row.get("a_automation_id")?,
        title: row.get("a_title")?,
        cron_expr: row.get("a_cron_expr")?,
        prompt: row.get("a_prompt")?,
        model: row.get("a_model")?,
        provider: row.get("a_provider")?,
        mode: row.get("a_mode")?,
        thought_level: row.get("a_thought_level")?,
        model_selection: row.get("a_model_selection")?,
        workspace_key: row.get("a_workspace_key")?,
        workspace_path: row.get("a_workspace_path")?,
        workspace_identity: row.get("a_workspace_identity")?,
        target_task_id: row.get("a_target_task_id")?,
        bot_delivery_target: row.get("a_bot_delivery_target")?,
        location_kind: row.get("a_location_kind")?,
        recurring: row.get("a_recurring")?,
        max_runs: row.get("a_max_runs")?,
        end_at: row.get("a_end_at")?,
        schedule_rule: row.get("a_schedule_rule")?,
        schedule_edited_by_user: row.get("a_schedule_edited_by_user")?,
        run_count: row.get("a_run_count")?,
        scheduled_run_count: row.get("a_scheduled_run_count")?,
        enabled: row.get("a_enabled")?,
        lifecycle_status: row.get("a_lifecycle_status")?,
        next_run_at: row.get("a_next_run_at")?,
        last_run_at: row.get("a_last_run_at")?,
        running: row.get("a_running")?,
        claimed_at: row.get("a_claimed_at")?,
        dispatch_status: row.get("a_dispatch_status")?,
        dispatch_attempts: row.get("a_dispatch_attempts")?,
        retry_at: row.get("a_retry_at")?,
        last_error: row.get("a_last_error")?,
        created_at: row.get("a_created_at")?,
        updated_at: row.get("a_updated_at")?,
    };
    let run = RunRow {
        run_id: row.get("r_run_id")?,
        automation_id: row.get("r_automation_id")?,
        workspace_key: row.get("r_workspace_key")?,
        scheduled_at: row.get("r_scheduled_at")?,
        trigger: row.get("r_trigger")?,
        model_selection: row.get("r_model_selection")?,
        dispatch_status: row.get("r_dispatch_status")?,
        outcome: row.get("r_outcome")?,
        session_id: row.get("r_session_id")?,
        error: row.get("r_error")?,
        attempts: row.get("r_attempts")?,
        created_at: row.get("r_created_at")?,
        updated_at: row.get("r_updated_at")?,
    };
    Ok((automation, run))
}

fn claim_due(conn: &mut Connection, now: i64) -> anyhow::Result<Vec<Value>> {
    immediate(conn, |tx| {
        // 截止日期是计划边界；过期任务先转终态，避免继续被正常 cron 或 retry 认领。
        tx.execute(
            "UPDATE automations
        SET lifecycle_status = 'completed', enabled = 0, next_run_at = NULL,
            retry_at = NULL, running = 0, claimed_at = NULL, updated_at = :now
        WHERE enabled = 1 AND end_at IS NOT NULL AND end_at < :now",
            named_params! {":now": now},
        )?;
        tx.execute(
            STALE_CLAIM_SQL,
            named_params! {":stale": now - CLAIM_STALE_MS},
        )?;
        // 有 retry_at（退避中）只按 retry_at 到期认领；否则按 next_run_at。
        let due = {
            let mut statement = tx.prepare(
                "SELECT * FROM automations
          WHERE enabled = 1 AND running = 0
            AND (
              (retry_at IS NOT NULL AND retry_at <= :now)
              OR (retry_at IS NULL AND next_run_at IS NOT NULL AND next_run_at <= :now)
            )",
            )?;
            statement
                .query_map(named_params! {":now": now}, AutomationRow::read)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut claimed = Vec::new();
        for row in due {
            let changed = tx.execute(
                "UPDATE automations
        SET running = 1, claimed_at = :now, dispatch_status = 'claimed', updated_at = :now
        WHERE automation_id = :id AND running = 0",
                named_params! {":id": row.automation_id, ":now": now},
            )?;
            if changed == 1 {
                // 与 TS 相同：返回值只覆盖认领三列，updated_at 仍是认领前的值。
                claimed.push(
                    AutomationRow {
                        running: 1,
                        claimed_at: Some(now),
                        dispatch_status: "claimed".into(),
                        ..row
                    }
                    .to_value()?,
                );
            }
        }
        Ok(claimed)
    })
}

fn claim_manual_runs(conn: &mut Connection, now: i64) -> anyhow::Result<Vec<ClaimedManualRun>> {
    immediate(conn, |tx| {
        tx.execute(
            STALE_CLAIM_SQL,
            named_params! {":stale": now - CLAIM_STALE_MS},
        )?;
        let rows = {
            let mut statement = tx.prepare(MANUAL_RUNS_SQL)?;
            statement
                .query_map(
                    named_params! {":stale": now - CLAIM_STALE_MS},
                    read_manual_pair,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut claimed = Vec::new();
        for (automation, run) in rows {
            let changed = tx.execute(
                "UPDATE automations
        SET running = 1, claimed_at = :now, updated_at = :now
        WHERE automation_id = :id AND running = 0",
                named_params! {":id": automation.automation_id, ":now": now},
            )?;
            if changed != 1 {
                continue;
            }
            tx.execute(
                "UPDATE automation_runs
        SET attempts = attempts + 1, updated_at = :now
        WHERE run_id = :run_id AND trigger = 'manual' AND dispatch_status = 'claimed'",
                named_params! {":run_id": run.run_id, ":now": now},
            )?;
            claimed.push(ClaimedManualRun {
                automation: AutomationRow {
                    running: 1,
                    claimed_at: Some(now),
                    updated_at: now,
                    ..automation
                }
                .to_value()?,
                run: RunRow {
                    attempts: run.attempts + 1,
                    updated_at: now,
                    ..run
                }
                .to_value(),
            });
        }
        Ok(claimed)
    })
}

fn mark_dispatched(
    conn: &mut Connection,
    automation_id: &str,
    dispatched_at: i64,
    next_run_at: Option<i64>,
) -> anyhow::Result<()> {
    immediate(conn, |tx| {
        // 已删除：丢弃回写，避免复活。
        let Some(row) = get_row(tx, automation_id, None)? else {
            return Ok(());
        };
        let scheduled_run_count = row.scheduled_run_count + 1;
        // 有限次任务按定时派发数判断上限（未设 max_runs 视为 1）；run_count 还含 manual run。
        let reached_max = row.recurring == 0 && scheduled_run_count >= row.max_runs.unwrap_or(1);
        let reached_end = row
            .end_at
            .is_some_and(|end_at| next_run_at.is_none_or(|next| next > end_at));
        let finished = reached_max || reached_end;
        tx.execute(
            "UPDATE automations
        SET run_count = :run_count,
            scheduled_run_count = :scheduled_run_count,
            last_run_at = :dispatched_at,
            dispatch_status = 'dispatched',
            dispatch_attempts = 0,
            retry_at = NULL,
            last_error = NULL,
            running = 0,
            claimed_at = NULL,
            lifecycle_status = :lifecycle_status,
            enabled = :enabled,
            next_run_at = :next_run_at,
            updated_at = :now
        WHERE automation_id = :id",
            named_params! {
                ":id": automation_id,
                ":run_count": row.run_count + 1,
                ":scheduled_run_count": scheduled_run_count,
                ":dispatched_at": dispatched_at,
                ":lifecycle_status": if finished { "completed" } else { "active" },
                ":enabled": i64::from(!finished),
                ":next_run_at": if finished { None } else { next_run_at },
                ":now": dispatched_at,
            },
        )?;
        Ok(())
    })
}

const FAIL_TERMINALLY_SQL: &str = "UPDATE automations
        SET dispatch_status = 'failed_to_dispatch', lifecycle_status = 'failed',
            enabled = 0, running = 0, claimed_at = NULL,
            last_error = :error, updated_at = :now
        WHERE automation_id = :id";

fn mark_dispatch_failed(
    conn: &mut Connection,
    automation_id: &str,
    failure: &DispatchFailed,
) -> anyhow::Result<()> {
    immediate(conn, |tx| {
        let Some(row) = get_row(tx, automation_id, None)? else {
            return Ok(());
        };
        let now = failure.failed_at;
        let error = &failure.error;
        if failure.kind == FailureKind::Permanent {
            tx.execute(
                FAIL_TERMINALLY_SQL,
                named_params! {":id": automation_id, ":error": error, ":now": now},
            )?;
            return Ok(());
        }
        let attempts = row.dispatch_attempts + 1;
        if attempts >= DISPATCH_MAX_ATTEMPTS {
            if row.recurring == 1 {
                // 循环任务：放弃本轮，跳下一个正常 next_run_at，清重试态回 idle。
                tx.execute(
                    "UPDATE automations
          SET dispatch_status = 'idle', dispatch_attempts = 0, retry_at = NULL,
              running = 0, claimed_at = NULL, next_run_at = :next_run_at,
              last_error = :error, updated_at = :now
          WHERE automation_id = :id",
                    named_params! {
                        ":id": automation_id,
                        ":next_run_at": failure.next_run_at,
                        ":error": error,
                        ":now": now,
                    },
                )?;
            } else {
                tx.execute(
                    FAIL_TERMINALLY_SQL,
                    named_params! {":id": automation_id, ":error": error, ":now": now},
                )?;
            }
            return Ok(());
        }
        // 未达上限：写退避 retry_at，复位 running 等下轮重认领。
        tx.execute(
            "UPDATE automations
      SET dispatch_status = 'failed_to_dispatch', dispatch_attempts = :attempts,
          retry_at = :retry_at, running = 0, claimed_at = NULL,
          last_error = :error, updated_at = :now
      WHERE automation_id = :id",
            named_params! {
                ":id": automation_id,
                ":attempts": attempts,
                ":retry_at": compute_retry_at(now, attempts),
                ":error": error,
                ":now": now,
            },
        )?;
        Ok(())
    })
}

fn skip_and_reschedule(conn: &mut Connection, skip: &SkippedRun, now: i64) -> anyhow::Result<()> {
    immediate(conn, |tx| {
        tx.execute(
            "INSERT INTO automation_runs (
          run_id, automation_id, workspace_key, scheduled_at, trigger,
          dispatch_status, error, attempts, created_at, updated_at
        ) VALUES (:run_id, :automation_id, :workspace_key, :scheduled_at, 'schedule', 'skipped', :reason, 0, :now, :now)
        ON CONFLICT(run_id) DO UPDATE SET
          dispatch_status = 'skipped', error = excluded.error, updated_at = excluded.updated_at",
            named_params! {
                ":run_id": skip.run_id,
                ":automation_id": skip.automation_id,
                ":workspace_key": skip.workspace_key,
                ":scheduled_at": skip.scheduled_at,
                ":reason": skip.reason,
                ":now": now,
            },
        )?;
        if skip.finalize {
            tx.execute(
                "UPDATE automations
          SET lifecycle_status = 'completed', enabled = 0, next_run_at = NULL,
              running = 0, claimed_at = NULL,
              dispatch_status = 'idle', dispatch_attempts = 0, retry_at = NULL,
              updated_at = :now
          WHERE automation_id = :id",
                named_params! {":id": skip.automation_id, ":now": now},
            )?;
        } else {
            tx.execute(
                "UPDATE automations
          SET next_run_at = :next_run_at, running = 0, claimed_at = NULL,
              dispatch_status = 'idle', dispatch_attempts = 0, retry_at = NULL,
              updated_at = :now
          WHERE automation_id = :id",
                named_params! {
                    ":id": skip.automation_id,
                    ":next_run_at": skip.next_run_at,
                    ":now": now,
                },
            )?;
        }
        Ok(())
    })
}

impl AutomationRepo {
    /// `claimDue`：single-flight 认领到期项（`running 0→1`），同时终结过期任务、回收僵尸认领。
    /// 返回认领到的 `ZCodeAutomation`。
    pub async fn claim_due(&self, now: i64) -> anyhow::Result<Vec<Value>> {
        self.index.call(move |conn, _| claim_due(conn, now)).await
    }

    /// `claimManualRuns`：认领排队中的 manual run（短暂占用 automation 的 single-flight 锁）。
    pub async fn claim_manual_runs(&self, now: i64) -> anyhow::Result<Vec<ClaimedManualRun>> {
        self.index
            .call(move |conn, _| claim_manual_runs(conn, now))
            .await
    }

    /// `upsertRunClaimed`：run_id 冲突即命中本轮 retry（attempts + 1），不新建。
    pub async fn upsert_run_claimed(
        &self,
        run: &RunIdentity,
        model_selection: Option<&Value>,
    ) -> anyhow::Result<()> {
        let serialized = serialize_model_selection(model_selection)?;
        let run = run.clone();
        self.index
            .call(move |conn, now| {
                conn.execute(
                    "INSERT INTO automation_runs (
          run_id, automation_id, workspace_key, scheduled_at, trigger,
          model_selection, dispatch_status, attempts, created_at, updated_at
        ) VALUES (:run_id, :automation_id, :workspace_key, :scheduled_at, :trigger, :model_selection, 'claimed', 0, :now, :now)
        ON CONFLICT(run_id) DO UPDATE SET
          dispatch_status = 'claimed',
          model_selection = COALESCE(automation_runs.model_selection, excluded.model_selection),
          outcome = NULL,
          error = NULL,
          attempts = attempts + 1,
          updated_at = excluded.updated_at",
                    named_params! {
                        ":run_id": run.run_id,
                        ":automation_id": run.automation_id,
                        ":workspace_key": run.workspace_key,
                        ":scheduled_at": run.scheduled_at,
                        ":trigger": run.trigger,
                        ":model_selection": serialized,
                        ":now": now,
                    },
                )?;
                Ok(())
            })
            .await
    }

    /// `markDispatched`：派发成功结算，计数加一、清重试态；有限次任务达上限或越过 end_at 转 completed。
    pub async fn mark_dispatched(
        &self,
        automation_id: &str,
        dispatched_at: i64,
        next_run_at: Option<i64>,
    ) -> anyhow::Result<()> {
        let automation_id = automation_id.to_owned();
        self.index
            .call(move |conn, _| mark_dispatched(conn, &automation_id, dispatched_at, next_run_at))
            .await
    }

    /// `markDispatchFailed`：transient 退避（达上限后循环任务跳下一周期、有限次任务 failed）；
    /// permanent 直接 failed。
    pub async fn mark_dispatch_failed(
        &self,
        automation_id: &str,
        failure: DispatchFailed,
    ) -> anyhow::Result<()> {
        let automation_id = automation_id.to_owned();
        self.index
            .call(move |conn, _| mark_dispatch_failed(conn, &automation_id, &failure))
            .await
    }

    /// `skipAndReschedule`：错过窗口，原子地记一条 skipped run 并前推 next_run_at（或终结一次性任务）。
    pub async fn skip_and_reschedule(&self, skip: SkippedRun) -> anyhow::Result<()> {
        self.index
            .call(move |conn, now| skip_and_reschedule(conn, &skip, now))
            .await
    }

    /// `releaseClaim`：退出时释放认领，清 running、保留 next_run_at，不记失败不推进。
    pub async fn release_claim(&self, automation_id: &str) -> anyhow::Result<()> {
        let automation_id = automation_id.to_owned();
        self.index
            .call(move |conn, now| {
                conn.execute(
                    "UPDATE automations
        SET running = 0, claimed_at = NULL, dispatch_status = 'idle', updated_at = :now
        WHERE automation_id = :id AND running = 1",
                    named_params! {":id": automation_id, ":now": now},
                )?;
                Ok(())
            })
            .await
    }
}
