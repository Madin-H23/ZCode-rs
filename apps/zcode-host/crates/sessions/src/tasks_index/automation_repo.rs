//! automations / automation_runs（packages/services/src/session/automationRepo.ts）。
//!
//! 与 Node Host 和 Main 的 scheduler（Node）共用 tasks-index.sqlite，Rust 是完全兼容的写者：
//! SQL 原样移植，读改写都在 `BEGIN IMMEDIATE` 内。行到 JSON 的映射（`rowToAutomation` /
//! `rowToRun`）保持 TS 对象字面量的键序，可空列为 NULL 时省略字段。
//! 只移植 Host 用到的方法；U4 起调度状态机（`claimDue` 等）也在 Host 内，见 automation_repo_scheduling.rs。
use anyhow::{Context as _, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, Row, named_params};
use serde_json::{Map, Value, json};

use super::TasksIndex;
use crate::agent::service::uuid_v4;

#[path = "automation_repo_scheduling.rs"]
mod scheduling;
pub use scheduling::{
    DISPATCH_MAX_ATTEMPTS, DISPATCH_RETRY_BASE_MS, DISPATCH_RETRY_CAP_MS, DispatchFailed,
    FailureKind, SkippedRun, compute_retry_at,
};

pub const AUTOMATION_CREATE_LIMIT: i64 = 20;
pub const AUTOMATION_CREATE_LIMIT_ERROR_CODE: &str = "AUTOMATION_CREATE_LIMIT_REACHED";
/// 认领超时回收：running=1 超过该时长仍未结算，视为持有者已崩溃。
pub const CLAIM_STALE_MS: i64 = 10 * 60_000;
const TASK_MODES: [&str; 6] = ["yolo", "plan", "edit", "auto", "autoEdit", "build"];

/// `assertValidAutomationMode`。
pub fn assert_valid_mode(mode: Option<&Value>) -> anyhow::Result<()> {
    match mode {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(mode)) if TASK_MODES.contains(&mode.as_str()) => Ok(()),
        Some(Value::String(mode)) => bail!("Invalid automation mode: {mode}"),
        Some(other) => bail!("Invalid automation mode: {other}"),
    }
}

fn trimmed_non_empty(value: &Value) -> Option<String> {
    let text = value.as_str()?.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// `modelSelectionSchema.safeParse`：strict，字符串 trim 后非空；输出键序 providerId、modelId、options。
pub fn parse_model_selection(value: &Value) -> Option<Value> {
    let object = value.as_object()?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "providerId" | "modelId" | "options"))
    {
        return None;
    }
    let mut out = Map::new();
    out.insert(
        "providerId".into(),
        json!(trimmed_non_empty(object.get("providerId")?)?),
    );
    out.insert(
        "modelId".into(),
        json!(trimmed_non_empty(object.get("modelId")?)?),
    );
    if let Some(options) = object.get("options") {
        let options = options.as_object()?;
        if options.keys().any(|key| key != "reasoningLevel") {
            return None;
        }
        let mut parsed = Map::new();
        if let Some(level) = options.get("reasoningLevel") {
            parsed.insert("reasoningLevel".into(), json!(trimmed_non_empty(level)?));
        }
        out.insert("options".into(), Value::Object(parsed));
    }
    Some(Value::Object(out))
}

/// `serializeAutomationModelSelection`：只取三个字段，空 options 省略；非法时报错。
pub fn serialize_model_selection(selection: Option<&Value>) -> anyhow::Result<Option<String>> {
    let Some(selection) = selection.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let object = selection
        .as_object()
        .ok_or_else(|| anyhow!("Invalid model selection: expected object"))?;
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
    let parsed = parse_model_selection(&Value::Object(input))
        .ok_or_else(|| anyhow!("Invalid model selection"))?;
    Ok(Some(parsed.to_string()))
}

fn read_serialized_model_selection(value: Option<&str>) -> Option<Value> {
    let value = value.filter(|value| !value.is_empty())?;
    parse_model_selection(&serde_json::from_str(value).ok()?)
}

/// `zcodeAutomationBotDeliveryTargetSchema.safeParse`。
pub fn parse_bot_delivery_target(value: &Value) -> Option<Value> {
    let object = value.as_object()?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "provider" | "botId" | "providerUserId" | "chatType"
        )
    }) {
        return None;
    }
    let provider = object.get("provider")?.as_str()?;
    let chat_type = object.get("chatType")?.as_str()?;
    if !matches!(provider, "feishu" | "lark" | "weixin")
        || !matches!(chat_type, "private" | "group")
    {
        return None;
    }
    Some(json!({
        "provider": provider,
        "botId": trimmed_non_empty(object.get("botId")?)?,
        "providerUserId": trimmed_non_empty(object.get("providerUserId")?)?,
        "chatType": chat_type,
    }))
}

/// `resolveWorkspaceKey`。
pub fn workspace_key(workspace_path: &str, workspace_identity: Option<&str>) -> String {
    workspace_identity
        .map(str::trim)
        .filter(|identity| !identity.is_empty())
        .unwrap_or(workspace_path)
        .to_owned()
}

/// `automations` 的一行（列名同表）。
#[derive(Debug, Clone, PartialEq)]
pub struct AutomationRow {
    pub automation_id: String,
    pub title: String,
    pub cron_expr: String,
    pub prompt: String,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub mode: Option<String>,
    pub thought_level: Option<String>,
    pub model_selection: Option<String>,
    pub workspace_key: String,
    pub workspace_path: String,
    pub workspace_identity: Option<String>,
    pub target_task_id: Option<String>,
    pub bot_delivery_target: Option<String>,
    pub location_kind: String,
    pub recurring: i64,
    pub max_runs: Option<i64>,
    pub end_at: Option<i64>,
    pub schedule_rule: Option<String>,
    pub schedule_edited_by_user: i64,
    pub run_count: i64,
    pub scheduled_run_count: i64,
    pub enabled: i64,
    pub lifecycle_status: String,
    pub next_run_at: Option<i64>,
    pub last_run_at: Option<i64>,
    pub running: i64,
    pub claimed_at: Option<i64>,
    pub dispatch_status: String,
    pub dispatch_attempts: i64,
    pub retry_at: Option<i64>,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl AutomationRow {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            automation_id: row.get("automation_id")?,
            title: row.get("title")?,
            cron_expr: row.get("cron_expr")?,
            prompt: row.get("prompt")?,
            model: row.get("model")?,
            provider: row.get("provider")?,
            mode: row.get("mode")?,
            thought_level: row.get("thought_level")?,
            model_selection: row.get("model_selection")?,
            workspace_key: row.get("workspace_key")?,
            workspace_path: row.get("workspace_path")?,
            workspace_identity: row.get("workspace_identity")?,
            target_task_id: row.get("target_task_id")?,
            bot_delivery_target: row.get("bot_delivery_target")?,
            location_kind: row.get("location_kind")?,
            recurring: row.get("recurring")?,
            max_runs: row.get("max_runs")?,
            end_at: row.get("end_at")?,
            schedule_rule: row.get("schedule_rule")?,
            schedule_edited_by_user: row.get("schedule_edited_by_user")?,
            run_count: row.get("run_count")?,
            scheduled_run_count: row.get("scheduled_run_count")?,
            enabled: row.get("enabled")?,
            lifecycle_status: row.get("lifecycle_status")?,
            next_run_at: row.get("next_run_at")?,
            last_run_at: row.get("last_run_at")?,
            running: row.get("running")?,
            claimed_at: row.get("claimed_at")?,
            dispatch_status: row.get("dispatch_status")?,
            dispatch_attempts: row.get("dispatch_attempts")?,
            retry_at: row.get("retry_at")?,
            last_error: row.get("last_error")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }

    /// `rowToAutomation`。
    pub fn to_value(&self) -> anyhow::Result<Value> {
        let mut out = Map::new();
        let mut put = |key: &str, value: Value| {
            out.insert(key.into(), value);
        };
        put("automationId", json!(self.automation_id));
        put("title", json!(self.title));
        put("cronExpr", json!(self.cron_expr));
        put("prompt", json!(self.prompt));
        if let Some(selection) = read_serialized_model_selection(self.model_selection.as_deref()) {
            put("modelSelection", selection);
        }
        // 历史空串、非法值按未设置兼容。
        if let Some(mode) = self
            .mode
            .as_deref()
            .filter(|mode| TASK_MODES.contains(mode))
        {
            put("mode", json!(mode));
        }
        put("workspaceKey", json!(self.workspace_key));
        put("workspacePath", json!(self.workspace_path));
        if let Some(identity) = &self.workspace_identity {
            put("workspaceIdentity", json!(identity));
        }
        if let Some(task) = &self.target_task_id {
            put("targetTaskId", json!(task));
        }
        let location = if self.location_kind == "remote" {
            "remote"
        } else {
            "local"
        };
        put("locationKind", json!(location));
        put("recurring", json!(self.recurring == 1));
        if let Some(max_runs) = self.max_runs {
            put("maxRuns", json!(max_runs));
        }
        if let Some(end_at) = self.end_at {
            put("endAt", json!(end_at));
        }
        if let Some(rule) = self
            .schedule_rule
            .as_deref()
            .filter(|rule| !rule.is_empty())
        {
            let rule: Value = serde_json::from_str(rule)
                .with_context(|| format!("invalid schedule_rule of {}", self.automation_id))?;
            put("scheduleRule", rule);
        }
        if self.schedule_edited_by_user == 1 {
            put("scheduleEditedByUser", json!(true));
        }
        put("runCount", json!(self.run_count));
        put("enabled", json!(self.enabled == 1));
        put("lifecycleStatus", json!(self.lifecycle_status));
        if let Some(next) = self.next_run_at {
            put("nextRunAt", json!(next));
        }
        if let Some(last) = self.last_run_at {
            put("lastRunAt", json!(last));
        }
        put("dispatchStatus", json!(self.dispatch_status));
        put("dispatchAttempts", json!(self.dispatch_attempts));
        if let Some(retry_at) = self.retry_at {
            put("retryAt", json!(retry_at));
        }
        if let Some(error) = &self.last_error {
            put("lastError", json!(error));
        }
        put("createdAt", json!(self.created_at));
        put("updatedAt", json!(self.updated_at));
        Ok(Value::Object(out))
    }
}

/// `automation_runs` 的一行。
#[derive(Debug, Clone, PartialEq)]
pub struct RunRow {
    pub run_id: String,
    pub automation_id: String,
    pub workspace_key: String,
    pub scheduled_at: Option<i64>,
    pub trigger: String,
    pub model_selection: Option<String>,
    pub dispatch_status: String,
    pub outcome: Option<String>,
    pub session_id: Option<String>,
    pub error: Option<String>,
    pub attempts: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl RunRow {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            run_id: row.get("run_id")?,
            automation_id: row.get("automation_id")?,
            workspace_key: row.get("workspace_key")?,
            scheduled_at: row.get("scheduled_at")?,
            trigger: row.get("trigger")?,
            model_selection: row.get("model_selection")?,
            dispatch_status: row.get("dispatch_status")?,
            outcome: row.get("outcome")?,
            session_id: row.get("session_id")?,
            error: row.get("error")?,
            attempts: row.get("attempts")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }

    /// `rowToRun`。
    pub fn to_value(&self) -> Value {
        let mut out = Map::new();
        out.insert("runId".into(), json!(self.run_id));
        out.insert("automationId".into(), json!(self.automation_id));
        out.insert("workspaceKey".into(), json!(self.workspace_key));
        if let Some(at) = self.scheduled_at {
            out.insert("scheduledAt".into(), json!(at));
        }
        out.insert("trigger".into(), json!(self.trigger));
        if let Some(selection) = read_serialized_model_selection(self.model_selection.as_deref()) {
            out.insert("modelSelection".into(), selection);
        }
        out.insert("dispatchStatus".into(), json!(self.dispatch_status));
        if let Some(outcome) = &self.outcome {
            out.insert("outcome".into(), json!(outcome));
        }
        if let Some(session) = &self.session_id {
            out.insert("sessionId".into(), json!(session));
        }
        if let Some(error) = &self.error {
            out.insert("error".into(), json!(error));
        }
        out.insert("attempts".into(), json!(self.attempts));
        out.insert("createdAt".into(), json!(self.created_at));
        out.insert("updatedAt".into(), json!(self.updated_at));
        Value::Object(out)
    }
}

fn get_row(
    conn: &Connection,
    automation_id: &str,
    workspace_key: Option<&str>,
) -> rusqlite::Result<Option<AutomationRow>> {
    conn.query_row(
        "SELECT * FROM automations
        WHERE automation_id = :id
          AND (:workspace_key IS NULL OR workspace_key = :workspace_key)",
        named_params! {":id": automation_id, ":workspace_key": workspace_key},
        AutomationRow::read,
    )
    .optional()
}

/// 在 `BEGIN IMMEDIATE` 内执行；出错回滚（同 TS 的 try/ROLLBACK）。
fn immediate<T>(
    conn: &mut Connection,
    body: impl FnOnce(&rusqlite::Transaction<'_>) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let result = body(&tx)?;
    tx.commit()?;
    Ok(result)
}

/// `create` 的入参（`ZCodeAutomationCreateParams` 经 service 归一化后的持久化字段）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NewAutomation {
    pub title: String,
    pub cron_expr: String,
    pub prompt: String,
    pub model_selection: Option<Value>,
    pub mode: Option<Value>,
    pub workspace_path: String,
    pub workspace_identity: Option<String>,
    pub target_task_id: Option<String>,
    pub bot_delivery_target: Option<Value>,
    pub recurring: bool,
    pub max_runs: Option<i64>,
    pub end_at: Option<i64>,
    pub schedule_rule: Option<Value>,
}

/// `update` 的调度选项。`next_run_at`：外层 None = 不改，`Some(None)` = 置空。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UpdateOptions {
    pub next_run_at: Option<Option<i64>>,
    pub lifecycle_status: Option<String>,
    pub reset_retry: bool,
}

/// `runNow` 的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ClaimedManualRun {
    pub automation: Value,
    pub run: Value,
}

/// automation 仓库：与 tasks-index 共用同一个 DB 线程。
#[derive(Clone)]
pub struct AutomationRepo {
    index: TasksIndex,
}

/// 补丁字段：键缺失 = 不改（undefined），`null` = 清除。
fn patch_field<'a>(patch: &'a Value, key: &str) -> Option<&'a Value> {
    patch.as_object().and_then(|object| object.get(key))
}

fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Value::String(text) => !text.is_empty(),
        _ => true,
    }
}

impl AutomationRepo {
    pub fn new(index: TasksIndex) -> Self {
        Self { index }
    }

    pub async fn create(
        &self,
        params: NewAutomation,
        next_run_at: Option<i64>,
        lifecycle_status: Option<String>,
    ) -> anyhow::Result<Value> {
        assert_valid_mode(params.mode.as_ref())?;
        let model_selection = serialize_model_selection(params.model_selection.as_ref())?
            .unwrap_or_else(|| "null".to_owned());
        self.index
            .call(move |conn, now| {
                let automation_id = format!("automation-{}", uuid_v4());
                let key = workspace_key(&params.workspace_path, params.workspace_identity.as_deref());
                immediate(conn, |tx| {
                    let count: i64 =
                        tx.query_row("SELECT COUNT(*) AS count FROM automations", [], |row| {
                            row.get(0)
                        })?;
                    if count >= AUTOMATION_CREATE_LIMIT {
                        bail!(
                            "[{AUTOMATION_CREATE_LIMIT_ERROR_CODE}] At most {AUTOMATION_CREATE_LIMIT} automations may be retained. Delete an existing automation before creating another."
                        );
                    }
                    let lifecycle = lifecycle_status.as_deref().unwrap_or("active");
                    tx.execute(
                        "INSERT INTO automations (
          automation_id, title, cron_expr, prompt, model, provider, model_selection,
          workspace_key, workspace_path, workspace_identity, target_task_id, bot_delivery_target, location_kind,
          recurring, max_runs, end_at, schedule_rule, schedule_edited_by_user,
          run_count, enabled, lifecycle_status,
          next_run_at, last_run_at, running, claimed_at,
          dispatch_status, dispatch_attempts, retry_at, last_error,
          mode, thought_level,
          created_at, updated_at
        ) VALUES (
          :automation_id, :title, :cron_expr, :prompt, NULL, NULL, :model_selection,
          :workspace_key, :workspace_path, :workspace_identity, :target_task_id, :bot_delivery_target, 'local',
          :recurring, :max_runs, :end_at, :schedule_rule, 0,
          0, :enabled, :lifecycle_status,
          :next_run_at, NULL, 0, NULL,
          'idle', 0, NULL, NULL,
          :mode, NULL,
          :now, :now
        )",
                        named_params! {
                            ":automation_id": automation_id,
                            ":title": params.title,
                            ":cron_expr": params.cron_expr,
                            ":prompt": params.prompt,
                            ":model_selection": model_selection,
                            ":workspace_key": key,
                            ":workspace_path": params.workspace_path,
                            ":workspace_identity": params.workspace_identity,
                            ":target_task_id": params.target_task_id,
                            ":bot_delivery_target": params.bot_delivery_target.as_ref().map(Value::to_string),
                            ":recurring": i64::from(params.recurring),
                            ":max_runs": params.max_runs,
                            ":end_at": params.end_at,
                            ":schedule_rule": params.schedule_rule.as_ref().map(Value::to_string),
                            ":enabled": i64::from(lifecycle != "completed"),
                            ":lifecycle_status": lifecycle,
                            ":next_run_at": next_run_at,
                            ":mode": params.mode.as_ref().and_then(Value::as_str),
                            ":now": now,
                        },
                    )?;
                    Ok(())
                })?;
                get_row(conn, &automation_id, None)?
                    .context("created automation disappeared")?
                    .to_value()
            })
            .await
    }

    /// 按 `created_at` 倒序；`workspace_key` 为 None 时列出全部。
    pub async fn list(&self, workspace_key: Option<String>) -> anyhow::Result<Vec<Value>> {
        self.index
            .call(move |conn, _| {
                let mut statement = conn.prepare(
                    "SELECT * FROM automations
        WHERE (:workspace_key IS NULL OR workspace_key = :workspace_key)
        ORDER BY created_at DESC",
                )?;
                let rows = statement
                    .query_map(
                        named_params! {":workspace_key": workspace_key},
                        AutomationRow::read,
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows.iter().map(AutomationRow::to_value).collect()
            })
            .await
    }

    pub async fn get_row(
        &self,
        automation_id: &str,
        workspace_key: Option<String>,
    ) -> anyhow::Result<Option<AutomationRow>> {
        let automation_id = automation_id.to_owned();
        self.index
            .call(move |conn, _| Ok(get_row(conn, &automation_id, workspace_key.as_deref())?))
            .await
    }

    pub async fn get(
        &self,
        automation_id: &str,
        workspace_key: Option<String>,
    ) -> anyhow::Result<Option<Value>> {
        self.get_row(automation_id, workspace_key)
            .await?
            .map(|row| row.to_value())
            .transpose()
    }

    /// `getModelSelectionForDispatch`：SQL NULL 是缺配置，JSON null 是跟随 workspace。
    pub async fn get_model_selection_for_dispatch(
        &self,
        automation_id: &str,
        workspace_key: &str,
    ) -> anyhow::Result<Option<Value>> {
        let row = self
            .get_row(automation_id, Some(workspace_key.to_owned()))
            .await?
            .ok_or_else(|| anyhow!("Automation 不存在或不属于当前工作区"))?;
        if let Some(selection) = read_serialized_model_selection(row.model_selection.as_deref()) {
            return Ok(Some(selection));
        }
        if row.model_selection.as_deref() != Some("null") {
            bail!("Automation 模型选择不可用，请重新选择模型与思考档位");
        }
        Ok(None)
    }

    pub async fn get_bot_delivery_target(
        &self,
        automation_id: &str,
        workspace_key: Option<String>,
    ) -> anyhow::Result<Option<Value>> {
        let row = self.get_row(automation_id, workspace_key).await?;
        Ok(row
            .and_then(|row| row.bot_delivery_target)
            .filter(|raw| !raw.is_empty())
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .and_then(|value| parse_bot_delivery_target(&value)))
    }

    pub async fn has_task_binding(
        &self,
        workspace_key: String,
        target_task_id: String,
    ) -> anyhow::Result<bool> {
        self.index
            .call(move |conn, _| {
                Ok(conn
                    .query_row(
                        "SELECT 1 AS bound FROM automations
        WHERE workspace_key = :workspace_key AND target_task_id = :target_task_id
        LIMIT 1",
                        named_params! {":workspace_key": workspace_key, ":target_task_id": target_task_id},
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some())
            })
            .await
    }

    pub async fn get_scheduled_run_count(
        &self,
        automation_id: &str,
        workspace_key: Option<String>,
    ) -> anyhow::Result<Option<i64>> {
        Ok(self
            .get_row(automation_id, workspace_key)
            .await?
            .map(|row| row.scheduled_run_count))
    }

    /// `update`：`patch` 是 `ZCodeAutomationUpdateParams`（键缺失保留、null 清除）。
    pub async fn update(
        &self,
        automation_id: &str,
        patch: Value,
        options: UpdateOptions,
        workspace_key: Option<String>,
    ) -> anyhow::Result<Option<Value>> {
        assert_valid_mode(patch_field(&patch, "mode"))?;
        let model_selection = match patch_field(&patch, "modelSelection") {
            None => None,
            Some(value) => {
                Some(serialize_model_selection(Some(value))?.unwrap_or_else(|| "null".into()))
            }
        };
        let automation_id = automation_id.to_owned();
        self.index
            .call(move |conn, now| {
                let Some(existing) = get_row(conn, &automation_id, workspace_key.as_deref())?
                else {
                    return Ok(None);
                };
                let text = |key: &str, current: &str| -> String {
                    patch_field(&patch, key)
                        .and_then(Value::as_str)
                        .map_or_else(|| current.to_owned(), str::to_owned)
                };
                let optional_int = |key: &str, current: Option<i64>| -> Option<i64> {
                    match patch_field(&patch, key) {
                        None => current,
                        Some(value) => value.as_i64(),
                    }
                };
                let mut next = existing.clone();
                next.title = text("title", &existing.title);
                next.cron_expr = text("cronExpr", &existing.cron_expr);
                next.prompt = text("prompt", &existing.prompt);
                if let Some(selection) = model_selection {
                    next.model_selection = Some(selection);
                }
                if let Some(mode) = patch_field(&patch, "mode") {
                    next.mode = mode.as_str().map(str::to_owned);
                }
                if let Some(recurring) = patch_field(&patch, "recurring") {
                    next.recurring = i64::from(js_truthy(recurring));
                }
                next.max_runs = optional_int("maxRuns", existing.max_runs);
                next.end_at = optional_int("endAt", existing.end_at);
                if let Some(rule) = patch_field(&patch, "scheduleRule") {
                    next.schedule_rule = js_truthy(rule).then(|| rule.to_string());
                }
                if let Some(edited) = patch_field(&patch, "scheduleEditedByUser") {
                    next.schedule_edited_by_user = i64::from(js_truthy(edited));
                }
                if let Some(next_run_at) = options.next_run_at {
                    next.next_run_at = next_run_at;
                }
                if let Some(status) = &options.lifecycle_status {
                    next.lifecycle_status = status.clone();
                    // enabled 完整由 lifecycleStatus 推导：只有显式改生命周期时才动它。
                    next.enabled = i64::from(status == "active");
                }
                if options.reset_retry {
                    next.dispatch_attempts = 0;
                    next.retry_at = None;
                    next.dispatch_status = "idle".into();
                }
                next.updated_at = now;
                write_row(conn, &next)?;
                next.to_value().map(Some)
            })
            .await
    }

    pub async fn delete(
        &self,
        automation_id: &str,
        workspace_key: Option<String>,
    ) -> anyhow::Result<bool> {
        let automation_id = automation_id.to_owned();
        self.index
            .call(move |conn, _| {
                Ok(conn.execute(
                    "DELETE FROM automations
        WHERE automation_id = :id
          AND (:workspace_key IS NULL OR workspace_key = :workspace_key)",
                    named_params! {":id": automation_id, ":workspace_key": workspace_key},
                )? > 0)
            })
            .await
    }

    /// 暂停 / 恢复：paused ↔ active，保留 next_run_at / run_count。
    pub async fn set_enabled(
        &self,
        automation_id: &str,
        enabled: bool,
        workspace_key: Option<String>,
    ) -> anyhow::Result<()> {
        let automation_id = automation_id.to_owned();
        self.index
            .call(move |conn, now| {
                conn.execute(
                    "UPDATE automations
        SET enabled = :enabled,
            lifecycle_status = :lifecycle_status,
            updated_at = :now
        WHERE automation_id = :id
          AND (:workspace_key IS NULL OR workspace_key = :workspace_key)",
                    named_params! {
                        ":id": automation_id,
                        ":enabled": i64::from(enabled),
                        ":lifecycle_status": if enabled { "active" } else { "paused" },
                        ":now": now,
                        ":workspace_key": workspace_key,
                    },
                )?;
                Ok(())
            })
            .await
    }

    /// 终态任务手动重跑：回 active、清计数与重试态。
    pub async fn restart(
        &self,
        automation_id: &str,
        next_run_at: Option<i64>,
        workspace_key: Option<String>,
    ) -> anyhow::Result<()> {
        let automation_id = automation_id.to_owned();
        self.index
            .call(move |conn, now| {
                conn.execute(
                    "UPDATE automations
        SET lifecycle_status = 'active',
            enabled = 1,
            run_count = 0,
            scheduled_run_count = 0,
            dispatch_attempts = 0,
            retry_at = NULL,
            dispatch_status = 'idle',
            running = 0,
            claimed_at = NULL,
            next_run_at = :next_run_at,
            last_error = NULL,
            updated_at = :now
        WHERE automation_id = :id
          AND (:workspace_key IS NULL OR workspace_key = :workspace_key)",
                    named_params! {
                        ":id": automation_id,
                        ":next_run_at": next_run_at,
                        ":now": now,
                        ":workspace_key": workspace_key,
                    },
                )?;
                Ok(())
            })
            .await
    }
}

fn write_row(conn: &Connection, row: &AutomationRow) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE automations SET
          title = :title, cron_expr = :cron_expr, prompt = :prompt, model = :model, provider = :provider,
          model_selection = :model_selection,
          mode = :mode, thought_level = :thought_level,
          recurring = :recurring, max_runs = :max_runs, end_at = :end_at,
          schedule_rule = :schedule_rule,
          schedule_edited_by_user = :schedule_edited_by_user,
          next_run_at = :next_run_at, lifecycle_status = :lifecycle_status,
          dispatch_attempts = :dispatch_attempts, retry_at = :retry_at, dispatch_status = :dispatch_status,
          enabled = :enabled, updated_at = :updated_at
        WHERE automation_id = :automation_id",
        named_params! {
            ":automation_id": row.automation_id,
            ":title": row.title,
            ":cron_expr": row.cron_expr,
            ":prompt": row.prompt,
            ":model": row.model,
            ":provider": row.provider,
            ":model_selection": row.model_selection,
            ":mode": row.mode,
            ":thought_level": row.thought_level,
            ":recurring": row.recurring,
            ":max_runs": row.max_runs,
            ":end_at": row.end_at,
            ":schedule_rule": row.schedule_rule,
            ":schedule_edited_by_user": row.schedule_edited_by_user,
            ":next_run_at": row.next_run_at,
            ":lifecycle_status": row.lifecycle_status,
            ":dispatch_attempts": row.dispatch_attempts,
            ":retry_at": row.retry_at,
            ":dispatch_status": row.dispatch_status,
            ":enabled": row.enabled,
            ":updated_at": row.updated_at,
        },
    )?;
    Ok(())
}

/// `ensureRunClaimed` / `markRunOutcome` 等用到的 run 身份。
#[derive(Debug, Clone, PartialEq)]
pub struct RunIdentity {
    pub run_id: String,
    pub automation_id: String,
    pub workspace_key: String,
    pub scheduled_at: Option<i64>,
    pub trigger: String,
}

impl AutomationRepo {
    /// 立即运行：回收僵尸认领后抢 single-flight 锁，并插入由当前 Host 直接派发的 manual run。
    pub async fn run_now(
        &self,
        automation_id: &str,
        now: i64,
        workspace_key: Option<String>,
    ) -> anyhow::Result<Option<ClaimedManualRun>> {
        let automation_id = automation_id.to_owned();
        self.index
            .call(move |conn, _| {
                immediate(conn, |tx| {
                    tx.execute(
                        "UPDATE automations
        SET running = 0, claimed_at = NULL
        WHERE running = 1 AND claimed_at IS NOT NULL AND claimed_at <= :stale",
                        named_params! {":stale": now - CLAIM_STALE_MS},
                    )?;
                    let Some(row) = get_row(tx, &automation_id, workspace_key.as_deref())? else {
                        return Ok(None);
                    };
                    let claimed = tx.execute(
                        "UPDATE automations
          SET running = 1, claimed_at = :now, updated_at = :now
          WHERE automation_id = :id
            AND running = 0
            AND (:workspace_key IS NULL OR workspace_key = :workspace_key)",
                        named_params! {":id": automation_id, ":now": now, ":workspace_key": workspace_key},
                    )?;
                    if claimed != 1 {
                        return Ok(None);
                    }
                    let run_id = format!("{automation_id}:manual:{}", uuid_v4());
                    tx.execute(
                        "INSERT INTO automation_runs (
          run_id, automation_id, workspace_key, scheduled_at, trigger,
          model_selection, dispatch_status, attempts, created_at, updated_at
        ) VALUES (
          :run_id, :automation_id, :workspace_key, :scheduled_at, 'manual',
          NULL, 'claimed', 1, :now, :now
        )",
                        named_params! {
                            ":run_id": run_id,
                            ":automation_id": automation_id,
                            ":workspace_key": row.workspace_key,
                            ":scheduled_at": now,
                            ":now": now,
                        },
                    )?;
                    let automation = AutomationRow {
                        running: 1,
                        claimed_at: Some(now),
                        updated_at: now,
                        ..row.clone()
                    }
                    .to_value()?;
                    let run = RunRow {
                        run_id,
                        automation_id: automation_id.clone(),
                        workspace_key: row.workspace_key,
                        scheduled_at: Some(now),
                        trigger: "manual".into(),
                        model_selection: None,
                        dispatch_status: "claimed".into(),
                        outcome: None,
                        session_id: None,
                        error: None,
                        attempts: 1,
                        created_at: now,
                        updated_at: now,
                    }
                    .to_value();
                    Ok(Some(ClaimedManualRun { automation, run }))
                })
            })
            .await
    }

    /// manual run 结束后只释放 single-flight 锁。
    pub async fn release_manual_claim(
        &self,
        automation_id: &str,
        workspace_key: &str,
    ) -> anyhow::Result<()> {
        self.claim_update(
            "UPDATE automations
        SET running = 0, claimed_at = NULL, updated_at = :now
        WHERE automation_id = :id
          AND workspace_key = :workspace_key
          AND running = 1",
            automation_id,
            workspace_key,
        )
        .await
    }

    /// Host 仍持有 manual run 时续租，避免被 scheduler 当作僵尸回收。
    pub async fn touch_manual_claim(
        &self,
        automation_id: &str,
        workspace_key: &str,
    ) -> anyhow::Result<()> {
        self.claim_update(
            "UPDATE automations
        SET claimed_at = :now, updated_at = :now
        WHERE automation_id = :id
          AND workspace_key = :workspace_key
          AND running = 1",
            automation_id,
            workspace_key,
        )
        .await
    }

    async fn claim_update(
        &self,
        sql: &'static str,
        automation_id: &str,
        workspace_key: &str,
    ) -> anyhow::Result<()> {
        let (automation_id, workspace_key) = (automation_id.to_owned(), workspace_key.to_owned());
        self.index
            .call(move |conn, now| {
                conn.execute(
                    sql,
                    named_params! {":id": automation_id, ":workspace_key": workspace_key, ":now": now},
                )?;
                Ok(())
            })
            .await
    }

    /// 确保 run 历史存在（outcome 回写兜底，不增加 attempts）。
    pub async fn ensure_run_claimed(&self, run: &RunIdentity) -> anyhow::Result<()> {
        let run = run.clone();
        self.index
            .call(move |conn, now| {
                conn.execute(
                    "INSERT INTO automation_runs (
          run_id, automation_id, workspace_key, scheduled_at, trigger,
          dispatch_status, attempts, created_at, updated_at
        ) VALUES (:run_id, :automation_id, :workspace_key, :scheduled_at, :trigger, 'claimed', 0, :now, :now)
        ON CONFLICT(run_id) DO NOTHING",
                    named_params! {
                        ":run_id": run.run_id,
                        ":automation_id": run.automation_id,
                        ":workspace_key": run.workspace_key,
                        ":scheduled_at": run.scheduled_at,
                        ":trigger": run.trigger,
                        ":now": now,
                    },
                )?;
                Ok(())
            })
            .await
    }

    /// 首次形成 Submission 时原子固定 run 的模型选择；之后只能读回原值。
    pub async fn fix_run_model_selection(
        &self,
        run_id: &str,
        selection: &Value,
    ) -> anyhow::Result<Value> {
        let serialized = serialize_model_selection(Some(selection))?;
        let run_id = run_id.to_owned();
        self.index
            .call(move |conn, now| {
                conn.execute(
                    "UPDATE automation_runs
       SET model_selection = COALESCE(model_selection, :model_selection), updated_at = :now
       WHERE run_id = :run_id",
                    named_params! {":run_id": run_id, ":model_selection": serialized, ":now": now},
                )?;
                let stored: Option<Option<String>> = conn
                    .query_row(
                        "SELECT model_selection FROM automation_runs WHERE run_id = :run_id",
                        named_params! {":run_id": run_id},
                        |row| row.get(0),
                    )
                    .optional()?;
                read_serialized_model_selection(stored.flatten().as_deref())
                    .ok_or_else(|| anyhow!("Automation run 不存在或无法固定模型选择: {run_id}"))
            })
            .await
    }

    /// 派发结果回写 run（`failed_to_dispatch` 记 error）。
    pub async fn mark_run_dispatch(
        &self,
        run_id: &str,
        dispatch_status: &str,
        session_id: Option<String>,
        error: Option<String>,
    ) -> anyhow::Result<()> {
        let (run_id, dispatch_status) = (run_id.to_owned(), dispatch_status.to_owned());
        self.index
            .call(move |conn, now| {
                conn.execute(
                    "UPDATE automation_runs
        SET dispatch_status = :dispatch_status,
            session_id = COALESCE(:session_id, session_id),
            error = :error,
            updated_at = :now
        WHERE run_id = :run_id",
                    named_params! {
                        ":run_id": run_id,
                        ":dispatch_status": dispatch_status,
                        ":session_id": session_id,
                        ":error": error,
                        ":now": now,
                    },
                )?;
                Ok(())
            })
            .await
    }

    /// manual run 首次派发成功：原子更新 run 台账与累计运行次数（以首次进入 dispatched 为幂等边界）。
    pub async fn mark_manual_run_dispatched(
        &self,
        run_id: &str,
        session_id: Option<String>,
        dispatched_at: i64,
    ) -> anyhow::Result<bool> {
        let run_id = run_id.to_owned();
        self.index
            .call(move |conn, _| {
                immediate(conn, |tx| {
                    let run: Option<(String, String, String)> = tx
                        .query_row(
                            "SELECT automation_id, workspace_key, dispatch_status
          FROM automation_runs
          WHERE run_id = :run_id AND trigger = 'manual'",
                            named_params! {":run_id": run_id},
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                        )
                        .optional()?;
                    let Some((automation_id, workspace_key, status)) = run else {
                        return Ok(false);
                    };
                    if status == "dispatched" {
                        return Ok(false);
                    }
                    tx.execute(
                        "UPDATE automation_runs
        SET dispatch_status = 'dispatched',
            session_id = COALESCE(:session_id, session_id),
            error = NULL,
            updated_at = :now
        WHERE run_id = :run_id AND trigger = 'manual' AND dispatch_status <> 'dispatched'",
                        named_params! {":run_id": run_id, ":session_id": session_id, ":now": dispatched_at},
                    )?;
                    let changed = tx.execute(
                        "UPDATE automations
          SET run_count = run_count + 1,
              last_run_at = :dispatched_at,
              updated_at = :now
          WHERE automation_id = :automation_id AND workspace_key = :workspace_key",
                        named_params! {
                            ":automation_id": automation_id,
                            ":workspace_key": workspace_key,
                            ":dispatched_at": dispatched_at,
                            ":now": dispatched_at,
                        },
                    )?;
                    Ok(changed > 0)
                })
            })
            .await
    }

    /// 运行结果（running / succeeded / failed / stopped）；已有终态时 running 不覆盖。
    pub async fn mark_run_outcome(
        &self,
        run_id: &str,
        outcome: &str,
        error: Option<String>,
    ) -> anyhow::Result<()> {
        let (run_id, outcome) = (run_id.to_owned(), outcome.to_owned());
        self.index
            .call(move |conn, now| {
                conn.execute(
                    "UPDATE automation_runs
        SET outcome = CASE
              WHEN :outcome = 'running' AND outcome IS NOT NULL AND outcome <> 'running' THEN outcome
              ELSE :outcome
            END,
            error = CASE
              WHEN :outcome = 'running' AND outcome IS NOT NULL AND outcome <> 'running' THEN error
              ELSE COALESCE(:error, error)
            END,
            updated_at = :now
        WHERE run_id = :run_id",
                    named_params! {":run_id": run_id, ":outcome": outcome, ":error": error, ":now": now},
                )?;
                Ok(())
            })
            .await
    }

    pub async fn list_runs(
        &self,
        automation_id: &str,
        workspace_key: Option<String>,
    ) -> anyhow::Result<Vec<Value>> {
        let automation_id = automation_id.to_owned();
        self.index
            .call(move |conn, _| {
                let mut statement = conn.prepare(
                    "SELECT * FROM automation_runs
        WHERE automation_id = :id
          AND (:workspace_key IS NULL OR workspace_key = :workspace_key)
        ORDER BY created_at DESC",
                )?;
                let rows = statement
                    .query_map(
                        named_params! {":id": automation_id, ":workspace_key": workspace_key},
                        RunRow::read,
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows.iter().map(RunRow::to_value).collect())
            })
            .await
    }

    pub async fn get_run(&self, run_id: &str) -> anyhow::Result<Option<Value>> {
        let run_id = run_id.to_owned();
        self.index
            .call(move |conn, _| {
                Ok(conn
                    .query_row(
                        "SELECT * FROM automation_runs WHERE run_id = :run_id",
                        named_params! {":run_id": run_id},
                        RunRow::read,
                    )
                    .optional()?
                    .map(|row| row.to_value()))
            })
            .await
    }

    pub async fn delete_run(
        &self,
        run_id: &str,
        workspace_key: Option<String>,
    ) -> anyhow::Result<()> {
        let run_id = run_id.to_owned();
        self.index
            .call(move |conn, _| {
                conn.execute(
                    "DELETE FROM automation_runs
        WHERE run_id = :run_id
          AND (:workspace_key IS NULL OR workspace_key = :workspace_key)",
                    named_params! {":run_id": run_id, ":workspace_key": workspace_key},
                )?;
                Ok(())
            })
            .await
    }
}
