//! off_peak_tasks（packages/services/src/session/offPeakTaskRepo.ts）。
//!
//! 与 Node Host 和 Main 的 scheduler（Node）共用 tasks-index.sqlite：SQL 原样移植，Rust 是完全
//! 兼容的写者。只移植 Host 用到的方法；`claimDue`、`markRunning`、`releaseClaim`、
//! `recoverInterrupted` 由 scheduler 调用。
use anyhow::{Context as _, anyhow};
use rusqlite::{Connection, OptionalExtension, Row, named_params};
use serde_json::{Map, Value, json};

use super::TasksIndex;
use super::automation_repo::{parse_model_selection, workspace_key};
use crate::agent::service::uuid_v4;

pub const TERMINAL_STATUSES: [&str; 3] = ["completed", "failed", "cancelled"];
const TERMINAL_SQL_LIST: &str = "'completed', 'failed', 'cancelled'";

pub fn is_terminal(status: &str) -> bool {
    TERMINAL_STATUSES.contains(&status)
}

/// INSERT 撞上 idx_off_peak_bound_active（并发双创建的失败方）。
pub fn is_bound_session_conflict(error: &anyhow::Error) -> bool {
    format!("{error:#}").contains(
        "UNIQUE constraint failed: off_peak_tasks.workspace_key, off_peak_tasks.session_id",
    )
}

#[derive(Debug, Clone, PartialEq)]
struct OffPeakRow {
    off_peak_task_id: String,
    server_ticket_id: Option<String>,
    title: String,
    conversation_id: Option<String>,
    session_id: Option<String>,
    session_title: Option<String>,
    prompt: String,
    permission_mode: String,
    model_selection: Option<String>,
    workspace_key: String,
    workspace_path: String,
    workspace_identity: Option<String>,
    status: String,
    queued_at: i64,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    failure_reason: Option<String>,
    files_changed: Option<i64>,
    settled_at: Option<i64>,
    history_deleted_at: Option<i64>,
    registered_at: Option<i64>,
    schedulable: i64,
    queue_position: Option<i64>,
    next_poll_at: Option<i64>,
    created_at: i64,
    updated_at: i64,
}

impl OffPeakRow {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        let session_title = match row.as_ref().column_index("session_title") {
            Ok(index) => row.get(index)?,
            Err(_) => None,
        };
        Ok(Self {
            off_peak_task_id: row.get("off_peak_task_id")?,
            server_ticket_id: row.get("server_ticket_id")?,
            title: row.get("title")?,
            conversation_id: row.get("conversation_id")?,
            session_id: row.get("session_id")?,
            session_title,
            prompt: row.get("prompt")?,
            permission_mode: row.get("permission_mode")?,
            model_selection: row.get("model_selection")?,
            workspace_key: row.get("workspace_key")?,
            workspace_path: row.get("workspace_path")?,
            workspace_identity: row.get("workspace_identity")?,
            status: row.get("status")?,
            queued_at: row.get("queued_at")?,
            started_at: row.get("started_at")?,
            ended_at: row.get("ended_at")?,
            failure_reason: row.get("failure_reason")?,
            files_changed: row.get("files_changed")?,
            settled_at: row.get("settled_at")?,
            history_deleted_at: row.get("history_deleted_at")?,
            registered_at: row.get("registered_at")?,
            schedulable: row.get("schedulable")?,
            queue_position: row.get("queue_position")?,
            next_poll_at: row.get("next_poll_at")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }

    fn model_selection(&self) -> Option<Value> {
        let raw = self
            .model_selection
            .as_deref()
            .filter(|raw| !raw.is_empty())?;
        parse_model_selection(&serde_json::from_str(raw).ok()?)
    }

    /// `rowToTask`。
    fn to_value(&self) -> Value {
        let mut out = Map::new();
        let mut put = |key: &str, value: Value| {
            out.insert(key.into(), value);
        };
        let optional_text = |value: &Option<String>| value.as_ref().map(|text| json!(text));
        let optional_int = |value: Option<i64>| value.map(|number| json!(number));
        put("offPeakTaskId", json!(self.off_peak_task_id));
        if let Some(value) = optional_text(&self.server_ticket_id) {
            put("serverTicketId", value);
        }
        put("title", json!(self.title));
        if let Some(value) = optional_text(&self.conversation_id) {
            put("conversationId", value);
        }
        if let Some(value) = optional_text(&self.session_id) {
            put("sessionId", value);
        }
        if let Some(title) = self
            .session_title
            .as_ref()
            .filter(|title| !title.is_empty())
        {
            put("sessionTitle", json!(title));
        }
        put("prompt", json!(self.prompt));
        put("permissionMode", json!(self.permission_mode));
        match self.model_selection() {
            Some(selection) => put("modelSelection", selection),
            None => put("modelSelectionIssue", json!({"code": "repair-required"})),
        }
        put("workspaceKey", json!(self.workspace_key));
        put("workspacePath", json!(self.workspace_path));
        if let Some(value) = optional_text(&self.workspace_identity) {
            put("workspaceIdentity", value);
        }
        put("status", json!(self.status));
        put("queuedAt", json!(self.queued_at));
        for (key, value) in [
            ("startedAt", optional_int(self.started_at)),
            ("endedAt", optional_int(self.ended_at)),
            ("failureReason", optional_text(&self.failure_reason)),
            ("filesChanged", optional_int(self.files_changed)),
            ("settledAt", optional_int(self.settled_at)),
            ("historyDeletedAt", optional_int(self.history_deleted_at)),
            ("registeredAt", optional_int(self.registered_at)),
        ] {
            if let Some(value) = value {
                put(key, value);
            }
        }
        put("schedulable", json!(self.schedulable == 1));
        if let Some(value) = optional_int(self.queue_position) {
            put("queuePosition", value);
        }
        if let Some(value) = optional_int(self.next_poll_at) {
            put("nextPollAt", value);
        }
        put("createdAt", json!(self.created_at));
        put("updatedAt", json!(self.updated_at));
        Value::Object(out)
    }
}

fn get_row(conn: &Connection, id: &str) -> rusqlite::Result<Option<OffPeakRow>> {
    conn.query_row(
        "SELECT * FROM off_peak_tasks WHERE off_peak_task_id = ?",
        [id],
        OffPeakRow::read,
    )
    .optional()
}

fn get_value(conn: &Connection, id: &str) -> anyhow::Result<Value> {
    Ok(get_row(conn, id)?
        .context("off-peak task disappeared")?
        .to_value())
}

/// `serializeOffPeakModelSelection`。
fn serialize_selection(selection: &Value) -> anyhow::Result<String> {
    let object = selection
        .as_object()
        .ok_or_else(|| anyhow!("Invalid model selection"))?;
    let mut input = Map::new();
    for key in ["providerId", "modelId"] {
        if let Some(value) = object.get(key) {
            input.insert(key.into(), value.clone());
        }
    }
    if let Some(options) = object.get("options").and_then(Value::as_object)
        && !options.is_empty()
    {
        input.insert("options".into(), Value::Object(options.clone()));
    }
    Ok(parse_model_selection(&Value::Object(input))
        .ok_or_else(|| anyhow!("Invalid model selection"))?
        .to_string())
}

/// `create` 的服务端字段（取号结果）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TicketFields {
    pub off_peak_task_id: Option<String>,
    pub server_ticket_id: Option<String>,
    pub queue_position: Option<i64>,
    pub registered_at: Option<i64>,
    pub schedulable: bool,
}

/// `updateSchedulingSnapshot` 的补丁：外层 None = 不改。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SchedulingPatch {
    pub schedulable: Option<bool>,
    pub queue_position: Option<Option<i64>>,
    pub next_poll_at: Option<Option<i64>>,
    pub server_ticket_id: Option<String>,
    pub registered_at: Option<i64>,
    pub now: Option<i64>,
}

/// `markTerminal` 的参数。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TerminalUpdate {
    pub status: String,
    pub ended_at: i64,
    pub failure_reason: Option<String>,
    pub files_changed: Option<i64>,
    pub dispatch_error: Option<String>,
}

#[derive(Clone)]
pub struct OffPeakRepo {
    index: TasksIndex,
}

impl OffPeakRepo {
    pub fn new(index: TasksIndex) -> Self {
        Self { index }
    }

    /// 创建即入队（status=queued）；`params` 是 `ZCodeOffPeakTaskCreateParams`。
    pub async fn create(
        &self,
        params: Value,
        ticket: TicketFields,
        now: Option<i64>,
    ) -> anyhow::Result<Value> {
        let selection = serialize_selection(&params["modelSelection"])?;
        self.index
            .call(move |conn, clock| {
                let now = now.unwrap_or(clock);
                let id = ticket
                    .off_peak_task_id
                    .clone()
                    .unwrap_or_else(|| format!("offpeak-{}", uuid_v4()));
                let text = |key: &str| params[key].as_str().map(str::to_owned);
                let path = text("workspacePath").unwrap_or_default();
                let identity = text("workspaceIdentity");
                conn.execute(
                    "INSERT INTO off_peak_tasks (
          off_peak_task_id, server_ticket_id, title, conversation_id, session_id,
          prompt, permission_mode, model, thought_level, model_selection,
          workspace_key, workspace_path, workspace_identity,
          status, queued_at, registered_at, schedulable, queue_position,
          claim_running, attempt_count, created_at, updated_at
        ) VALUES (
          :off_peak_task_id, :server_ticket_id, :title, NULL, :session_id,
          :prompt, :permission_mode, NULL, NULL, :model_selection,
          :workspace_key, :workspace_path, :workspace_identity,
          'queued', :now, :registered_at, :schedulable, :queue_position,
          0, 0, :now, :now
        )",
                    named_params! {
                        ":off_peak_task_id": id,
                        ":server_ticket_id": ticket.server_ticket_id,
                        ":title": text("title"),
                        ":session_id": text("boundSessionId"),
                        ":prompt": text("prompt"),
                        ":permission_mode": text("permissionMode"),
                        ":model_selection": selection,
                        ":workspace_key": workspace_key(&path, identity.as_deref()),
                        ":workspace_path": path,
                        ":workspace_identity": identity,
                        ":registered_at": ticket.registered_at,
                        ":schedulable": i64::from(ticket.schedulable),
                        ":queue_position": ticket.queue_position,
                        ":now": now,
                    },
                )?;
                get_value(conn, &id)
            })
            .await
    }

    /// 按 `created_at` 倒序，联查 tasks 表的绑定会话标题。
    pub async fn list(&self, workspace_key: Option<String>) -> anyhow::Result<Vec<Value>> {
        self.index
            .call(move |conn, _| {
                let has_tasks: bool = conn
                    .query_row(
                        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'tasks'",
                        [],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                let sql = if has_tasks {
                    "SELECT t.*, s.title AS session_title FROM off_peak_tasks t
            LEFT JOIN tasks s ON s.workspace_key = t.workspace_key AND s.task_id = t.session_id
            WHERE (:workspace_key IS NULL OR t.workspace_key = :workspace_key)
            ORDER BY t.created_at DESC"
                } else {
                    "SELECT * FROM off_peak_tasks
            WHERE (:workspace_key IS NULL OR workspace_key = :workspace_key)
            ORDER BY created_at DESC"
                };
                let mut statement = conn.prepare(sql)?;
                let rows = statement
                    .query_map(
                        named_params! {":workspace_key": workspace_key},
                        OffPeakRow::read,
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows.iter().map(OffPeakRow::to_value).collect())
            })
            .await
    }

    pub async fn get(&self, id: &str) -> anyhow::Result<Option<Value>> {
        let id = id.to_owned();
        self.index
            .call(move |conn, _| Ok(get_row(conn, &id)?.map(|row| row.to_value())))
            .await
    }

    pub async fn delete(&self, id: &str) -> anyhow::Result<()> {
        let id = id.to_owned();
        self.index
            .call(move |conn, _| {
                conn.execute(
                    "DELETE FROM off_peak_tasks WHERE off_peak_task_id = ?",
                    [id],
                )?;
                Ok(())
            })
            .await
    }

    /// 仅隐藏 History 行：任务必须已经实际启动；重复调用幂等。
    pub async fn mark_history_deleted(
        &self,
        id: &str,
        now: Option<i64>,
    ) -> anyhow::Result<Option<Value>> {
        let id = id.to_owned();
        self.index
            .call(move |conn, clock| {
                let Some(row) = get_row(conn, &id)? else {
                    return Ok(None);
                };
                if row.started_at.is_none() || row.history_deleted_at.is_some() {
                    return Ok(Some(row.to_value()));
                }
                conn.execute(
                    "UPDATE off_peak_tasks
        SET history_deleted_at = :now, updated_at = :now
        WHERE off_peak_task_id = :id AND started_at IS NOT NULL",
                    named_params! {":id": id, ":now": now.unwrap_or(clock)},
                )?;
                get_value(conn, &id).map(Some)
            })
            .await
    }

    /// 本会话是否已有未终态绑定任务。
    pub async fn has_active_bound_task(
        &self,
        workspace_key: &str,
        session_id: &str,
    ) -> anyhow::Result<bool> {
        let (workspace_key, session_id) = (workspace_key.to_owned(), session_id.to_owned());
        self.index
            .call(move |conn, _| {
                let sql = format!(
                    "SELECT 1 AS hit FROM off_peak_tasks
         WHERE workspace_key = :workspace_key AND session_id = :session_id
           AND status NOT IN ({TERMINAL_SQL_LIST})
         LIMIT 1"
                );
                Ok(conn
                    .query_row(
                        &sql,
                        named_params! {":workspace_key": workspace_key, ":session_id": session_id},
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some())
            })
            .await
    }

    /// 编辑窗口期字段：仅 queued / paused 可编辑；`params` 是 `OffPeakUpdateTaskParams`。
    pub async fn update_editable_fields(
        &self,
        id: &str,
        params: Value,
        now: Option<i64>,
    ) -> anyhow::Result<Option<Value>> {
        let id = id.to_owned();
        self.index
            .call(move |conn, clock| {
                let Some(row) = get_row(conn, &id)? else {
                    return Ok(None);
                };
                if row.status != "queued" && row.status != "paused" {
                    return Ok(None);
                }
                let next_selection = match params.get("modelSelection") {
                    Some(Value::Null) => return Ok(None),
                    Some(selection) => Some(selection.clone()),
                    None => row.model_selection(),
                };
                let next_selection = next_selection
                    .ok_or_else(|| anyhow!("Off-Peak task 缺少有效 ModelSelection: {id}"))?;
                let text = |key: &str, current: &str| {
                    params
                        .get(key)
                        .and_then(Value::as_str)
                        .unwrap_or(current)
                        .to_owned()
                };
                conn.execute(
                    "UPDATE off_peak_tasks SET
          title = :title, prompt = :prompt, permission_mode = :permission_mode,
          model_selection = :model_selection, updated_at = :now
        WHERE off_peak_task_id = :id",
                    named_params! {
                        ":id": id,
                        ":title": text("title", &row.title),
                        ":prompt": text("prompt", &row.prompt),
                        ":permission_mode": text("permissionMode", &row.permission_mode),
                        ":model_selection": serialize_selection(&next_selection)?,
                        ":now": now.unwrap_or(clock),
                    },
                )?;
                get_value(conn, &id).map(Some)
            })
            .await
    }

    /// 轮询 / 重新取号写回：只覆盖显式传入的字段。
    pub async fn update_scheduling_snapshot(
        &self,
        id: &str,
        patch: SchedulingPatch,
    ) -> anyhow::Result<()> {
        let id = id.to_owned();
        self.index
            .call(move |conn, clock| {
                let Some(row) = get_row(conn, &id)? else {
                    return Ok(());
                };
                conn.execute(
                    "UPDATE off_peak_tasks SET
          schedulable = :schedulable,
          queue_position = :queue_position,
          next_poll_at = :next_poll_at,
          server_ticket_id = :server_ticket_id,
          registered_at = :registered_at,
          updated_at = :now
        WHERE off_peak_task_id = :id",
                    named_params! {
                        ":id": id,
                        ":schedulable": patch.schedulable.map_or(row.schedulable, i64::from),
                        ":queue_position": patch.queue_position.unwrap_or(row.queue_position),
                        ":next_poll_at": patch.next_poll_at.unwrap_or(row.next_poll_at),
                        ":server_ticket_id": patch.server_ticket_id.or(row.server_ticket_id),
                        ":registered_at": patch.registered_at.or(row.registered_at),
                        ":now": patch.now.unwrap_or(clock),
                    },
                )?;
                Ok(())
            })
            .await
    }

    /// 终态不可逆出：非终态才写入，返回写入后的任务。
    pub async fn mark_terminal(
        &self,
        id: &str,
        update: TerminalUpdate,
    ) -> anyhow::Result<Option<Value>> {
        let id = id.to_owned();
        self.index
            .call(move |conn, _| {
                let sql = format!(
                    "UPDATE off_peak_tasks
        SET status = :status,
            ended_at = :ended_at,
            failure_reason = :failure_reason,
            files_changed = COALESCE(:files_changed, files_changed),
            attempt_count = attempt_count + :dispatch_attempt_inc,
            last_error = COALESCE(:dispatch_error, last_error),
            schedulable = 0,
            claim_running = 0, claimed_at = NULL,
            updated_at = :ended_at
        WHERE off_peak_task_id = :id AND status NOT IN ({TERMINAL_SQL_LIST})"
                );
                let changed = conn.execute(
                    &sql,
                    named_params! {
                        ":id": id,
                        ":status": update.status,
                        ":ended_at": update.ended_at,
                        ":failure_reason": update.failure_reason,
                        ":files_changed": update.files_changed,
                        ":dispatch_attempt_inc": i64::from(update.dispatch_error.is_some()),
                        ":dispatch_error": update.dispatch_error,
                    },
                )?;
                if changed != 1 {
                    return Ok(None);
                }
                get_value(conn, &id).map(Some)
            })
            .await
    }

    /// Pause / Continue：queued ⇄ paused；派发在途（claim_running=1）不可切换。
    pub async fn set_paused(
        &self,
        id: &str,
        paused: bool,
        now: Option<i64>,
    ) -> anyhow::Result<Option<Value>> {
        let id = id.to_owned();
        self.index
            .call(move |conn, clock| {
                let (to, from) = if paused {
                    ("paused", "queued")
                } else {
                    ("queued", "paused")
                };
                let changed = conn.execute(
                    "UPDATE off_peak_tasks
        SET status = :to, updated_at = :now
        WHERE off_peak_task_id = :id AND status = :from AND claim_running = 0",
                    named_params! {":id": id, ":to": to, ":from": from, ":now": now.unwrap_or(clock)},
                )?;
                if changed != 1 {
                    return Ok(None);
                }
                get_value(conn, &id).map(Some)
            })
            .await
    }

    /// 票据过期的续跑回队：running → queued，保留会话供 resume 接续。
    pub async fn requeue_for_continuation(
        &self,
        id: &str,
        now: Option<i64>,
    ) -> anyhow::Result<Option<Value>> {
        let id = id.to_owned();
        self.index
            .call(move |conn, clock| {
                let changed = conn.execute(
                    "UPDATE off_peak_tasks
        SET status = 'queued', schedulable = 0, queue_position = NULL,
            claim_running = 0, claimed_at = NULL, updated_at = :now
        WHERE off_peak_task_id = :id AND status = 'running'",
                    named_params! {":id": id, ":now": now.unwrap_or(clock)},
                )?;
                if changed != 1 {
                    return Ok(None);
                }
                get_value(conn, &id).map(Some)
            })
            .await
    }

    pub async fn list_non_terminal(&self) -> anyhow::Result<Vec<Value>> {
        self.select(format!(
            "SELECT * FROM off_peak_tasks
        WHERE status NOT IN ({TERMINAL_SQL_LIST})
        ORDER BY queued_at ASC"
        ))
        .await
    }

    pub async fn list_unsettled_terminal(&self) -> anyhow::Result<Vec<Value>> {
        self.select(format!(
            "SELECT * FROM off_peak_tasks
        WHERE status IN ({TERMINAL_SQL_LIST}) AND settled_at IS NULL
        ORDER BY ended_at ASC"
        ))
        .await
    }

    async fn select(&self, sql: String) -> anyhow::Result<Vec<Value>> {
        self.index
            .call(move |conn, _| {
                let mut statement = conn.prepare(&sql)?;
                let rows = statement
                    .query_map([], OffPeakRow::read)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows.iter().map(OffPeakRow::to_value).collect())
            })
            .await
    }

    /// settle 服务端 ack 后回填；仅终态行可核销。
    pub async fn mark_settled(&self, id: &str, settled_at: i64) -> anyhow::Result<()> {
        let id = id.to_owned();
        self.index
            .call(move |conn, _| {
                let sql = format!(
                    "UPDATE off_peak_tasks
        SET settled_at = :settled_at, updated_at = :settled_at
        WHERE off_peak_task_id = :id AND status IN ({TERMINAL_SQL_LIST})"
                );
                conn.execute(&sql, named_params! {":id": id, ":settled_at": settled_at})?;
                Ok(())
            })
            .await
    }
}
