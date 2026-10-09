//! 调度状态机用到的闲时任务仓库方法（offPeakTaskRepo.ts 的 claimDue、markRunning、releaseClaim、
//! recoverInterrupted、countActive）。SQL 原样移植：同一份 tasks-index 可能同时被另一个 App 实例的
//! Node scheduler 使用，认领在 `BEGIN IMMEDIATE` 内以 `claim_running 0→1` 完成（docs/specs/
//! rust-u4-app-host.md §2.4）。
use rusqlite::{Connection, named_params};
use serde_json::Value;

use super::{OffPeakRepo, OffPeakRow, get_value};

/// 认领超时回收：claim_running=1 超过该时长仍未结算，视为持有者已崩溃（与 automation 的常量独立）。
pub const OFF_PEAK_CLAIM_STALE_MS: i64 = 10 * 60_000;

/// `markRunning` 的参数：首跑回填 conversation / session，续跑保留首段 started_at。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunningUpdate {
    pub started_at: i64,
    pub conversation_id: Option<String>,
    pub session_id: Option<String>,
    pub server_ticket_id: Option<String>,
}

fn claim_due(conn: &mut Connection, now: i64) -> anyhow::Result<Vec<Value>> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute(
        "UPDATE off_peak_tasks
        SET claim_running = 0, claimed_at = NULL
        WHERE claim_running = 1 AND claimed_at IS NOT NULL AND claimed_at <= :stale",
        named_params! {":stale": now - OFF_PEAK_CLAIM_STALE_MS},
    )?;
    let due = {
        let mut statement = tx.prepare(
            "SELECT * FROM off_peak_tasks
          WHERE status = 'queued' AND schedulable = 1 AND claim_running = 0
          ORDER BY queued_at ASC, created_at ASC",
        )?;
        statement
            .query_map([], OffPeakRow::read)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut claimed = Vec::new();
    for row in due {
        // 历史行可能没有 Provider 身份：留在列表等待修复，但不能被认领，也不阻塞后续任务。
        if row.model_selection().is_none() {
            continue;
        }
        let changed = tx.execute(
            "UPDATE off_peak_tasks
        SET claim_running = 1, claimed_at = :now, updated_at = :now
        WHERE off_peak_task_id = :id AND claim_running = 0",
            named_params! {":id": row.off_peak_task_id, ":now": now},
        )?;
        if changed == 1 {
            // 与 TS 相同：返回认领前读到的行（认领列不在 ZCodeOffPeakTask 里，updatedAt 是旧值）。
            claimed.push(row.to_value());
        }
    }
    tx.commit()?;
    Ok(claimed)
}

fn recover_interrupted(conn: &mut Connection, now: i64) -> anyhow::Result<usize> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let recovered = tx.execute(
        "UPDATE off_peak_tasks
          SET status = 'queued', claim_running = 0, claimed_at = NULL, updated_at = :now
          WHERE status = 'running'",
        named_params! {":now": now},
    )?;
    tx.execute(
        "UPDATE off_peak_tasks
        SET claim_running = 0, claimed_at = NULL, updated_at = :now
        WHERE claim_running = 1 AND claimed_at IS NOT NULL AND claimed_at <= :stale",
        named_params! {":now": now, ":stale": now - OFF_PEAK_CLAIM_STALE_MS},
    )?;
    tx.commit()?;
    Ok(recovered)
}

impl OffPeakRepo {
    /// `claimDue`：认领 queued 且 schedulable 的任务（FIFO：queued_at、created_at），同时回收僵尸认领。
    pub async fn claim_due(&self, now: i64) -> anyhow::Result<Vec<Value>> {
        self.index.call(move |conn, _| claim_due(conn, now)).await
    }

    /// `markRunning`：派发成功 queued → running 并释放认领；不在 queued 时（暂停、取消竞态）为 None。
    pub async fn mark_running(
        &self,
        id: &str,
        update: RunningUpdate,
    ) -> anyhow::Result<Option<Value>> {
        let id = id.to_owned();
        self.index
            .call(move |conn, _| {
                let changed = conn.execute(
                    "UPDATE off_peak_tasks
        SET status = 'running',
            started_at = COALESCE(started_at, :started_at),
            conversation_id = COALESCE(:conversation_id, conversation_id),
            session_id = COALESCE(:session_id, session_id),
            server_ticket_id = COALESCE(:server_ticket_id, server_ticket_id),
            claim_running = 0, claimed_at = NULL,
            last_error = NULL,
            updated_at = :started_at
        WHERE off_peak_task_id = :id AND status = 'queued'",
                    named_params! {
                        ":id": id,
                        ":started_at": update.started_at,
                        ":conversation_id": update.conversation_id,
                        ":session_id": update.session_id,
                        ":server_ticket_id": update.server_ticket_id,
                    },
                )?;
                if changed != 1 {
                    return Ok(None);
                }
                get_value(conn, &id).map(Some)
            })
            .await
    }

    /// `releaseClaim`：复位 single-flight 锁；带 error 时累计 attempt_count 并记 last_error。
    /// 不改 status：任务留在 queued 等下轮认领。`now` 缺省取仓库时钟。
    pub async fn release_claim(
        &self,
        id: &str,
        error: Option<String>,
        now: Option<i64>,
    ) -> anyhow::Result<()> {
        let id = id.to_owned();
        self.index
            .call(move |conn, clock| {
                conn.execute(
                    "UPDATE off_peak_tasks
        SET claim_running = 0, claimed_at = NULL,
            attempt_count = attempt_count + :attempt_inc,
            last_error = COALESCE(:error, last_error),
            updated_at = :now
        WHERE off_peak_task_id = :id AND claim_running = 1",
                    named_params! {
                        ":id": id,
                        ":attempt_inc": i64::from(error.as_deref().is_some_and(|text| !text.is_empty())),
                        ":error": error,
                        ":now": now.unwrap_or(clock),
                    },
                )?;
                Ok(())
            })
            .await
    }

    /// `recoverInterrupted`：启动时把残留的 running 置回 queued（保留会话供 resume），并清理超时认领。
    /// 调用方须保证此时没有在跑的闲时任务（属主是 App 级的 Scheduler）。返回回收的任务数。
    pub async fn recover_interrupted(&self, now: i64) -> anyhow::Result<usize> {
        self.index
            .call(move |conn, _| recover_interrupted(conn, now))
            .await
    }

    /// `countActive`：执行中（running）的任务数，防休眠的判据。
    pub async fn count_active(&self) -> anyhow::Result<u64> {
        self.index
            .call(|conn, _| {
                let count: i64 = conn.query_row(
                    "SELECT COUNT(*) AS n FROM off_peak_tasks WHERE status = 'running'",
                    [],
                    |row| row.get(0),
                )?;
                Ok(u64::try_from(count).unwrap_or_default())
            })
            .await
    }
}
