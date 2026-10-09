//! automation 管理服务（packages/services/src/session/automationService.ts 与
//! automationIntervalCarrier.ts）：校验、下次触发时间、生命周期重算，存储交给 [`AutomationRepo`]。
//!
//! 入参保持为 JSON 对象：键缺失表示 TS 的 `undefined`（不修改），`null` 表示显式清除。
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};

use super::cron;
use super::schedule::{
    Rule, build_interval_schedule_rule, build_relative_delay_schedule,
    compute_automation_next_run_at, compute_initial_next_run_at,
    infer_minute_interval_schedule_rule, schedule_rule_definition,
};
use super::time::Zone;
use crate::tasks_index::automation_repo::{
    AutomationRepo, ClaimedManualRun, NewAutomation, UpdateOptions, workspace_key,
};

pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

const MAX_MONTHLY_INTERVAL: f64 = 1_200.0;
const MAX_SESSION_INTERVAL: f64 = 200.0;
const RULE_UNITS: [&str; 6] = ["minute", "hourly", "daily", "weekly", "monthly", "yearly"];

/// 写 / 单查操作的 workspace 归属。
#[derive(Debug, Clone, PartialEq)]
pub struct Scope {
    pub workspace_path: String,
    pub workspace_identity: Option<String>,
}

impl Scope {
    pub fn key(&self) -> String {
        workspace_key(&self.workspace_path, self.workspace_identity.as_deref())
    }

    /// 从 `{workspacePath, workspaceIdentity?}` 读取；没有 workspacePath 时为 None（不加归属过滤）。
    pub fn from_params(params: &Value) -> Option<Self> {
        let path = params.get("workspacePath")?.as_str()?;
        if path.is_empty() {
            return None;
        }
        Some(Self {
            workspace_path: path.to_owned(),
            workspace_identity: params
                .get("workspaceIdentity")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }
}

fn scope_key(scope: Option<&Scope>) -> Option<String> {
    scope.map(Scope::key)
}

fn invalid_cron(cron_expr: Option<&Value>) -> anyhow::Error {
    let shown = match cron_expr {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => "undefined".into(),
    };
    anyhow!("非法的 cron 表达式：{shown}")
}

fn invalid_rule(message: &str) -> anyhow::Error {
    anyhow!("非法的定时任务调度规则：{message}")
}

fn invalid_relative(message: &str) -> anyhow::Error {
    anyhow!("非法的相对时间定时任务：{message}")
}

fn invalid_carrier(message: &str) -> anyhow::Error {
    anyhow!("非法的自定义重复入参：{message}")
}

fn js_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Some(Value::String(text)) => !text.is_empty(),
        Some(_) => true,
    }
}

fn integer(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite() && number.fract() == 0.0)
}

fn only_integers_in_range(values: Option<&Value>, min: f64, max: f64) -> bool {
    values.and_then(Value::as_array).is_some_and(|values| {
        !values.is_empty()
            && values
                .iter()
                .all(|value| integer(Some(value)).is_some_and(|value| value >= min && value <= max))
    })
}

/// `assertValidAutomationScheduleRule`。
pub fn assert_valid_schedule_rule(rule: &Value) -> Result<()> {
    let empty = Map::new();
    let rule = rule.as_object().unwrap_or(&empty);
    let unit = rule.get("unit").and_then(Value::as_str).unwrap_or_default();
    if !RULE_UNITS.contains(&unit) {
        return Err(invalid_rule("unit 不受支持"));
    }
    let interval = integer(rule.get("interval")).filter(|interval| *interval >= 1.0);
    let Some(interval) = interval else {
        return Err(invalid_rule("interval 必须是正整数"));
    };
    if unit == "monthly" && interval > MAX_MONTHLY_INTERVAL {
        return Err(invalid_rule("monthly interval 不能超过 1200"));
    }
    if !integer(rule.get("hour")).is_some_and(|hour| (0.0..=23.0).contains(&hour)) {
        return Err(invalid_rule("hour 必须是 0-23 的整数"));
    }
    if !integer(rule.get("minute")).is_some_and(|minute| (0.0..=59.0).contains(&minute)) {
        return Err(invalid_rule("minute 必须是 0-59 的整数"));
    }
    let monthly_mode = rule.get("monthlyMode");
    if monthly_mode.is_some_and(|mode| !matches!(mode.as_str(), Some("date" | "weekday"))) {
        return Err(invalid_rule("monthlyMode 不受支持"));
    }
    let weekdays = rule.get("weekdays");
    if js_truthy(weekdays) && !only_integers_in_range(weekdays, 0.0, 6.0) {
        return Err(invalid_rule("weekdays 必须是 0-6 的非空整数数组"));
    }
    if unit == "weekly" && !only_integers_in_range(weekdays, 0.0, 6.0) {
        return Err(invalid_rule("weekly 规则必须包含有效的 weekdays"));
    }
    if unit == "monthly" {
        let weekday_mode = monthly_mode.and_then(Value::as_str) == Some("weekday");
        if weekday_mode && !only_integers_in_range(weekdays, 0.0, 6.0) {
            return Err(invalid_rule("monthly weekday 规则必须包含有效的 weekdays"));
        }
        if !weekday_mode && !only_integers_in_range(rule.get("monthDays"), 1.0, 31.0) {
            return Err(invalid_rule("monthly date 规则必须包含有效的 monthDays"));
        }
    }
    let months = rule.get("months");
    if js_truthy(months) && !only_integers_in_range(months, 1.0, 12.0) {
        return Err(invalid_rule("months 必须是 1-12 的非空整数数组"));
    }
    let month_days = rule.get("monthDays");
    if js_truthy(month_days) && !only_integers_in_range(month_days, 1.0, 31.0) {
        return Err(invalid_rule("monthDays 必须是 1-31 的非空整数数组"));
    }
    Ok(())
}

/// `assertValidAutomationIntervalCarrier`：键缺失表示 undefined。
fn assert_valid_carrier(params: &Map<String, Value>, schedule_rule: Option<&Value>) -> Result<()> {
    let unit = params.get("intervalUnit");
    let interval = params.get("interval");
    if unit.is_none() != interval.is_none() {
        return Err(invalid_carrier(
            "intervalUnit 与 interval 必须同时提交或同时省略",
        ));
    }
    if let Some(interval) = interval
        && !integer(Some(interval))
            .is_some_and(|interval| (1.0..=MAX_SESSION_INTERVAL).contains(&interval))
    {
        return Err(invalid_carrier("interval 必须是 1-200 的整数"));
    }
    if unit.is_some() && params.contains_key("relativeDelayMinutes") {
        return Err(invalid_carrier(
            "intervalUnit 是周期 carrier，不能与一次性 relativeDelayMinutes 同时提交",
        ));
    }
    if unit.is_some() && schedule_rule.is_some() {
        return Err(invalid_carrier(
            "intervalUnit carrier 不能与直传 scheduleRule 同时提交",
        ));
    }
    if unit.is_some() && params.get("recurring") == Some(&Value::Bool(false)) {
        return Err(invalid_carrier(
            "intervalUnit carrier 必须使用 recurring=true",
        ));
    }
    if unit.is_some() && params.get("maxRuns").is_some_and(Value::is_number) {
        return Err(invalid_carrier(
            "intervalUnit carrier 不能与有限 maxRuns 同时提交",
        ));
    }
    Ok(())
}

fn as_rule(value: Option<&Value>) -> Option<&Rule> {
    value.and_then(Value::as_object)
}

fn optional_i64(value: Option<&Value>) -> Option<i64> {
    value.and_then(Value::as_i64)
}

fn optional_string(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}

/// `{...rule, anchorAt}`：已有键原位覆盖，没有则追加。
fn with_anchor(rule: &Value, anchor_at: Value) -> Value {
    let mut rule = rule.as_object().cloned().unwrap_or_default();
    rule.insert("anchorAt".into(), anchor_at);
    Value::Object(rule)
}

pub struct AutomationService {
    repo: AutomationRepo,
    zone: Arc<dyn Zone>,
    clock: Clock,
}

impl AutomationService {
    pub fn new(repo: AutomationRepo, zone: Arc<dyn Zone>, clock: Clock) -> Self {
        Self { repo, zone, clock }
    }

    pub fn repo(&self) -> &AutomationRepo {
        &self.repo
    }

    pub fn zone(&self) -> &dyn Zone {
        self.zone.as_ref()
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    fn is_valid_cron(&self, cron_expr: Option<&Value>) -> bool {
        cron_expr
            .and_then(Value::as_str)
            .is_some_and(|expr| cron::is_valid(expr, self.zone.as_ref()))
    }

    /// `create`：`params` 是 `ZCodeAutomationCreateParams`（含 workspace 与 carrier 字段）。
    pub async fn create(&self, params: &Map<String, Value>) -> Result<Value> {
        let created_at = self.now();
        let zone = self.zone.as_ref();
        let relative = params.get("relativeDelayMinutes");
        if let Some(relative) = relative
            && !integer(Some(relative)).is_some_and(|value| (1.0..=525_600.0).contains(&value))
        {
            return Err(invalid_relative("delayMinutes 必须是 1-525600 的整数"));
        }
        if relative.is_some() && js_truthy(params.get("recurring")) {
            return Err(invalid_relative("相对延迟任务必须设置 recurring=false"));
        }
        if relative.is_some() && params.contains_key("maxRuns") {
            return Err(invalid_relative(
                "相对延迟任务固定只运行一次，不能设置 maxRuns",
            ));
        }
        if relative.is_some() && js_truthy(params.get("scheduleRule")) {
            return Err(invalid_relative(
                "不能同时提交 delayMinutes 和 scheduleRule",
            ));
        }
        assert_valid_carrier(params, params.get("scheduleRule"))?;
        // carrier 只用于本次归一化，不进入持久化字段。
        let mut normalized = params.clone();
        for key in ["relativeDelayMinutes", "intervalUnit", "interval"] {
            normalized.remove(key);
        }
        if let Some(delay) = optional_i64(relative) {
            let (cron_expr, rule) = build_relative_delay_schedule(zone, delay, created_at);
            normalized.insert("cronExpr".into(), json!(cron_expr));
            normalized.insert("scheduleRule".into(), Value::Object(rule));
            normalized.insert("recurring".into(), json!(false));
        }
        if let (Some(unit), Some(interval)) = (params.get("intervalUnit"), params.get("interval")) {
            let cron_expr = normalized
                .get("cronExpr")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let rule = build_interval_schedule_rule(
                zone,
                unit.as_str().unwrap_or_default(),
                interval.as_i64().unwrap_or_default(),
                &cron_expr,
                created_at,
            )
            .map_err(|message| anyhow!(message))?;
            normalized.insert("recurring".into(), json!(true));
            normalized.remove("maxRuns");
            normalized.insert("scheduleRule".into(), Value::Object(rule));
        }
        if !self.is_valid_cron(normalized.get("cronExpr")) {
            return Err(invalid_cron(normalized.get("cronExpr")));
        }
        let cron_expr = normalized["cronExpr"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let direct_rule = normalized
            .get("scheduleRule")
            .filter(|rule| js_truthy(Some(rule)))
            .cloned();
        if let Some(rule) = &direct_rule {
            assert_valid_schedule_rule(rule)?;
        }
        // `*/N` cron 缺少显式规则时补上以创建时刻为锚点的分钟规则。
        let schedule_rule = match direct_rule {
            Some(rule) => Some(with_anchor(&rule, json!(created_at))),
            None => {
                infer_minute_interval_schedule_rule(zone, &cron_expr, created_at).map(Value::Object)
            }
        };
        if let Some(rule) = &schedule_rule {
            normalized.insert("scheduleRule".into(), rule.clone());
        }
        let recurring = js_truthy(normalized.get("recurring"));
        let next_run_at = compute_initial_next_run_at(
            zone,
            &cron_expr,
            recurring,
            as_rule(schedule_rule.as_ref()),
            created_at,
        )
        .map_err(|message| anyhow!(message))?;
        let end_at = optional_i64(normalized.get("endAt"));
        // `endAt !== undefined && (nextRunAt ?? Infinity) > endAt`：null 参与比较时按 0。
        let ended_before_first_run = normalized.get("endAt").is_some_and(|end| {
            let end = end.as_f64().unwrap_or(0.0);
            next_run_at.is_none_or(|next| next as f64 > end)
        });
        let record = NewAutomation {
            title: optional_string(normalized.get("title")).unwrap_or_default(),
            cron_expr,
            prompt: optional_string(normalized.get("prompt")).unwrap_or_default(),
            model_selection: normalized.get("modelSelection").cloned(),
            mode: normalized.get("mode").cloned(),
            workspace_path: optional_string(normalized.get("workspacePath")).unwrap_or_default(),
            workspace_identity: optional_string(normalized.get("workspaceIdentity")),
            target_task_id: optional_string(normalized.get("targetTaskId")),
            bot_delivery_target: normalized
                .get("botDeliveryTarget")
                .filter(|target| js_truthy(Some(target)))
                .cloned(),
            recurring,
            max_runs: optional_i64(normalized.get("maxRuns")),
            end_at,
            schedule_rule: normalized
                .get("scheduleRule")
                .filter(|rule| js_truthy(Some(rule)))
                .cloned(),
        };
        if ended_before_first_run {
            self.repo
                .create(record, None, Some("completed".into()))
                .await
        } else {
            self.repo.create(record, next_run_at, None).await
        }
    }

    pub async fn list(&self, scope: Option<&Scope>) -> Result<Vec<Value>> {
        self.repo.list(scope_key(scope)).await
    }

    pub async fn has_task_binding(&self, scope: &Scope, target_task_id: &str) -> Result<bool> {
        self.repo
            .has_task_binding(scope.key(), target_task_id.to_owned())
            .await
    }

    pub async fn get(&self, automation_id: &str, scope: Option<&Scope>) -> Result<Option<Value>> {
        self.repo.get(automation_id, scope_key(scope)).await
    }
}

impl AutomationService {
    /// `update`：`patch` 是 `ZCodeAutomationUpdateParams`（含可选的 carrier 字段）。
    pub async fn update(
        &self,
        automation_id: &str,
        patch: &Map<String, Value>,
        scope: Option<&Scope>,
    ) -> Result<Option<Value>> {
        let zone = self.zone.as_ref();
        let key = scope_key(scope);
        let Some(existing) = self.repo.get(automation_id, key.clone()).await? else {
            return Ok(None);
        };
        if let Some(rule) = patch
            .get("scheduleRule")
            .filter(|rule| js_truthy(Some(rule)))
        {
            assert_valid_schedule_rule(rule)?;
        }
        let direct_rule = patch.get("scheduleRule").cloned();
        let mut rest = patch.clone();
        for key in ["intervalUnit", "interval", "scheduleRule"] {
            rest.remove(key);
        }
        assert_valid_carrier(patch, direct_rule.as_ref())?;
        let carrier = match (patch.get("intervalUnit"), patch.get("interval")) {
            (Some(unit), Some(interval)) => Some((
                unit.as_str().unwrap_or_default().to_owned(),
                interval.as_i64().unwrap_or_default(),
            )),
            _ => None,
        };
        if carrier.is_none()
            && rest.get("maxRuns") == Some(&Value::Null)
            && rest.get("recurring") != Some(&Value::Bool(true))
        {
            bail!("清空 maxRuns 时必须在同一次更新中设置 recurring=true");
        }
        let existing_recurring = existing["recurring"].as_bool().unwrap_or(false);
        let next_recurring = match (&carrier, rest.get("recurring")) {
            (Some(_), _) => true,
            (None, Some(Value::Null) | None) => existing_recurring,
            (None, Some(value)) => js_truthy(Some(value)),
        };
        if next_recurring && rest.get("maxRuns").is_some_and(Value::is_number) {
            bail!("无限循环任务不能设置有限次数 maxRuns");
        }
        let mut normalized = rest;
        if carrier.is_some() {
            normalized.insert("recurring".into(), json!(true));
            normalized.insert("maxRuns".into(), Value::Null);
        } else if next_recurring
            && !normalized.contains_key("maxRuns")
            && (normalized.get("recurring") == Some(&Value::Bool(true))
                || existing.get("maxRuns").is_some())
        {
            normalized.insert("maxRuns".into(), Value::Null);
        }

        let mut options = UpdateOptions::default();
        let existing_cron = existing["cronExpr"].as_str().unwrap_or_default().to_owned();
        let cron_changed = normalized
            .get("cronExpr")
            .is_some_and(|cron| cron.as_str() != Some(existing_cron.as_str()));
        if normalized.contains_key("cronExpr") && !self.is_valid_cron(normalized.get("cronExpr")) {
            return Err(invalid_cron(normalized.get("cronExpr")));
        }
        let updated_at = self.now();
        let effective_cron = match normalized.get("cronExpr") {
            Some(Value::String(cron)) => cron.clone(),
            _ => existing_cron,
        };
        let carrier_rule = match &carrier {
            Some((unit, interval)) => Some(Value::Object(
                build_interval_schedule_rule(zone, unit, *interval, &effective_cron, updated_at)
                    .map_err(|message| anyhow!(message))?,
            )),
            None => None,
        };
        // undefined（不改）与 null（清除）必须分开：direct 为 Some(Null) 表示清除。
        let effective_direct = direct_rule.or(carrier_rule);
        if let Some(rule) = effective_direct
            .as_ref()
            .filter(|rule| js_truthy(Some(rule)))
        {
            assert_valid_schedule_rule(rule)?;
        }
        let existing_rule = existing
            .get("scheduleRule")
            .filter(|rule| js_truthy(Some(rule)));
        let explicit_rule = match &effective_direct {
            Some(rule) if js_truthy(Some(rule)) => {
                let same = existing_rule.is_some_and(|existing_rule| {
                    as_rule(Some(existing_rule)).map(schedule_rule_definition)
                        == as_rule(Some(rule)).map(schedule_rule_definition)
                });
                let anchor = match existing_rule {
                    Some(existing_rule) if same => existing_rule
                        .get("anchorAt")
                        .cloned()
                        .unwrap_or(Value::Null),
                    _ => json!(updated_at),
                };
                Some(with_anchor(rule, anchor))
            }
            other => other.clone(),
        };
        let inferred = (cron_changed && effective_direct.is_none())
            .then(|| infer_minute_interval_schedule_rule(zone, &effective_cron, updated_at))
            .flatten()
            .map(Value::Object);
        let effective_schedule_rule: Option<Value> = match &effective_direct {
            None if cron_changed => inferred.clone(),
            None => existing_rule.cloned(),
            Some(_) => explicit_rule.clone().filter(|rule| !rule.is_null()),
        };
        let mut update_params = normalized.clone();
        if cron_changed && effective_direct.is_none() {
            update_params.insert(
                "scheduleRule".into(),
                inferred.clone().unwrap_or(Value::Null),
            );
        } else if effective_direct.is_some() {
            update_params.insert(
                "scheduleRule".into(),
                explicit_rule.clone().unwrap_or(Value::Null),
            );
        }
        let compute_next = |rule: Option<&Value>| -> Result<Option<i64>> {
            let rule = rule.filter(|rule| js_truthy(Some(rule)));
            compute_automation_next_run_at(zone, &effective_cron, as_rule(rule), updated_at)
                .map_err(|message| anyhow!(message))
        };
        if cron_changed || normalized.contains_key("endAt") || effective_direct.is_some() {
            options.next_run_at = Some(compute_next(effective_schedule_rule.as_ref())?);
            options.reset_retry = true;
        }
        let effective_end_at = match normalized.get("endAt") {
            None => existing.get("endAt").and_then(Value::as_f64),
            Some(end) => end.as_f64(),
        };
        let lifecycle = existing["lifecycleStatus"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        // `options.nextRunAt ?? existing.nextRunAt ?? Infinity`：计算结果为 null 时回落到已有值。
        let candidate = options
            .next_run_at
            .flatten()
            .or_else(|| existing.get("nextRunAt").and_then(Value::as_i64));
        if let Some(end) = effective_end_at
            && candidate.is_none_or(|next| next as f64 > end)
        {
            options.next_run_at = Some(None);
            options.lifecycle_status = Some("completed".into());
        } else if normalized.contains_key("endAt")
            && matches!(lifecycle.as_str(), "completed" | "failed")
        {
            options.lifecycle_status = Some("active".into());
        }
        if normalized.contains_key("recurring") || normalized.contains_key("maxRuns") {
            let next_max_runs = match normalized.get("maxRuns") {
                None => existing.get("maxRuns").and_then(Value::as_i64),
                Some(value) => value.as_i64(),
            };
            let Some(scheduled) = self
                .repo
                .get_scheduled_run_count(automation_id, key.clone())
                .await?
            else {
                return Ok(None);
            };
            let reached_max = !next_recurring && scheduled >= next_max_runs.unwrap_or(1);
            if reached_max {
                options.lifecycle_status = Some("completed".into());
            } else if matches!(lifecycle.as_str(), "completed" | "failed") {
                options.lifecycle_status = Some("active".into());
                if options.next_run_at.is_none() {
                    options.next_run_at = Some(compute_next(effective_schedule_rule.as_ref())?);
                }
            }
        }
        self.repo
            .update(automation_id, Value::Object(update_params), options, key)
            .await
    }

    pub async fn delete(&self, automation_id: &str, scope: Option<&Scope>) -> Result<bool> {
        self.repo.delete(automation_id, scope_key(scope)).await
    }

    pub async fn set_enabled(
        &self,
        automation_id: &str,
        enabled: bool,
        scope: Option<&Scope>,
    ) -> Result<()> {
        self.repo
            .set_enabled(automation_id, enabled, scope_key(scope))
            .await
    }

    /// 只有 `failed` 可以手动重排；`completed` 是有限计划自然耗尽后的终态，不能复活。
    pub async fn restart(&self, automation_id: &str, scope: Option<&Scope>) -> Result<()> {
        let key = scope_key(scope);
        let Some(existing) = self.repo.get(automation_id, key.clone()).await? else {
            return Ok(());
        };
        if existing["lifecycleStatus"] != "failed" {
            return Ok(());
        }
        let next_run_at = compute_automation_next_run_at(
            self.zone.as_ref(),
            existing["cronExpr"].as_str().unwrap_or_default(),
            as_rule(existing.get("scheduleRule")),
            self.now(),
        )
        .map_err(|message| anyhow!(message))?;
        self.repo.restart(automation_id, next_run_at, key).await
    }

    /// 立即运行：创建由当前 Host 直接派发的 manual run，不修改 cron 计划。
    pub async fn run_now(
        &self,
        automation_id: &str,
        scope: Option<&Scope>,
    ) -> Result<Option<ClaimedManualRun>> {
        let key = scope_key(scope);
        if self.repo.get(automation_id, key.clone()).await?.is_none() {
            return Ok(None);
        }
        self.repo.run_now(automation_id, self.now(), key).await
    }

    pub async fn list_runs(
        &self,
        automation_id: &str,
        scope: Option<&Scope>,
    ) -> Result<Vec<Value>> {
        self.repo.list_runs(automation_id, scope_key(scope)).await
    }

    pub async fn delete_run(&self, run_id: &str, scope: Option<&Scope>) -> Result<()> {
        self.repo.delete_run(run_id, scope_key(scope)).await
    }
}
