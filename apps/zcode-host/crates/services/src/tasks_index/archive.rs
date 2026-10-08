//! 归档清理与归档删除（taskIndexRepo.ts `archiveStaleTasks`、`deleteArchivedTask`）。
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, TransactionBehavior};
use serde_json::Value;

use super::meta::row_to_meta;
use super::repo::{
    WriteRecord, delete_task_grouping_references, get_task_row, read_task_row, write_record,
};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// 自动归档：已完成、无未读、未置顶、`updated_at` 早于截止时间的活跃行。只改 `archived` 列，
/// 返回更新前读出的 meta（与 TS 相同，`updated_at` 与 meta_json 都不变）。
pub(crate) fn archive_stale_tasks(
    conn: &mut Connection,
    now: i64,
    workspace_key: &str,
    older_than_days: f64,
    provider: Option<&str>,
) -> anyhow::Result<Vec<Value>> {
    let days = (older_than_days.floor() as i64).max(1);
    let cutoff = now - days * DAY_MS;
    let mut clauses = vec![
        "workspace_key = ?",
        "deleted = 0",
        "archived = 0",
        "pinned = 0",
        "unread_at IS NULL",
        "updated_at < ?",
        "task_status = 'completed'",
    ];
    let mut args = vec![
        SqlValue::Text(workspace_key.to_owned()),
        SqlValue::Integer(cutoff),
    ];
    if let Some(provider) = provider {
        clauses.push("provider = ?");
        args.push(SqlValue::Text(provider.to_owned()));
    }
    let rows = {
        let mut statement = conn.prepare(&format!(
            "SELECT
          workspace_key,
          workspace_path,
          workspace_identity,
          task_id,
          title,
          task_status,
          provider,
          mode,
          model,
          migration_source,
          forked_from_task_id,
          cron_automation_id,
          off_peak_task_id,
          created_at,
          updated_at,
          unread_at,
          last_unread_at,
          pinned,
          archived,
          deleted,
          title_overridden,
          searchable_text,
          meta_json
        FROM tasks
        WHERE {}
        ORDER BY updated_at DESC, created_at DESC, task_id DESC",
            clauses.join(" AND ")
        ))?;
        statement
            .query_map(rusqlite::params_from_iter(args.iter()), read_task_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for row in &rows {
        tx.execute(
            "UPDATE tasks
      SET archived = 1
      WHERE workspace_key = ? AND task_id = ?",
            (&row.workspace_key, &row.task_id),
        )?;
    }
    tx.commit()?;
    Ok(rows.iter().map(|row| row_to_meta(row).to_value()).collect())
}

/// 删除一个已归档的 task：归档检查与墓碑写入在同一个 IMMEDIATE 事务里；行不存在、已删除或
/// 未归档时返回 None（确认框停留期间可能被另一端恢复）。不清理 CLI 会话。
pub(crate) fn delete_archived_task(
    conn: &mut Connection,
    workspace_key: &str,
    task_id: &str,
) -> anyhow::Result<Option<Value>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let Some(row) = get_task_row(&tx, workspace_key, task_id)? else {
        tx.commit()?;
        return Ok(None);
    };
    if row.deleted || !row.archived {
        tx.commit()?;
        return Ok(None);
    }
    let meta = write_record(
        &tx,
        WriteRecord {
            meta: &row_to_meta(&row),
            pinned: row.pinned,
            archived: true,
            deleted: true,
            title_overridden: row.title_overridden,
            write_unread_at: false,
            searchable_text: None,
        },
    )?;
    delete_task_grouping_references(&tx, &row.workspace_key, &row.task_id)?;
    tx.commit()?;
    Ok(Some(meta.to_value()))
}
