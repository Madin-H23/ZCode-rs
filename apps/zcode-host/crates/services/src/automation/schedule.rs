//! 下次触发时间与调度规则（packages/services/src/session/automationCron.ts 的逐行移植）。
//!
//! 调度规则（`ZCodeAutomationScheduleRule`）保持为 JSON 对象：它原样序列化进 `schedule_rule` 列，
//! 键序必须与 TS 的对象展开一致（`{...rule, anchorAt}` 覆盖已有键时保持原位置）。
use serde_json::{Map, Number, Value, json};

use super::cron::Cron;
use super::time::{Zone, locale_string};

pub type Rule = Map<String, Value>;

const ONE_SHOT_MISSED_RUN_GRACE_MS: i64 = 60 * 1_000;
const ONE_SHOT_STALE_TARGET_WINDOW_MS: i64 = 30 * 60 * 1_000;

fn number(rule: &Rule, key: &str) -> Option<f64> {
    rule.get(key).and_then(Value::as_f64)
}

fn int(rule: &Rule, key: &str) -> i64 {
    number(rule, key).map_or(0, |value| value.trunc() as i64)
}

fn list(rule: &Rule, key: &str) -> Option<Vec<f64>> {
    let values: Vec<f64> = rule
        .get(key)?
        .as_array()?
        .iter()
        .filter_map(Value::as_f64)
        .collect();
    Some(values)
}

/// JS 数值写回 JSON：整数保持整数形态。
pub fn js_number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15 {
        json!(value as i64)
    } else {
        Number::from_f64(value).map_or(Value::Null, Value::Number)
    }
}

/// `scheduleRuleDefinition`：比较时忽略 anchorAt（数组排序后比较）。
pub fn schedule_rule_definition(rule: &Rule) -> String {
    let sorted = |key: &str| -> Value {
        match list(rule, key) {
            Some(mut values) if rule.get(key).is_some() => {
                values.sort_by(f64::total_cmp);
                Value::Array(values.into_iter().map(js_number).collect())
            }
            _ => Value::Null,
        }
    };
    let get = |key: &str| rule.get(key).cloned().unwrap_or(Value::Null);
    Value::Array(vec![
        get("unit"),
        get("interval"),
        get("hour"),
        get("minute"),
        sorted("weekdays"),
        sorted("monthDays"),
        sorted("months"),
        rule.get("monthlyMode").cloned().unwrap_or(Value::Null),
    ])
    .to_string()
}

/// `isOneShotAutomation`：非循环且至多执行一次。
pub fn is_one_shot(recurring: bool, max_runs: Option<i64>) -> bool {
    !recurring && max_runs.unwrap_or(1) <= 1
}

/// `computeNextRunAt`：cron 的下一次（本地时区）；表达式非法时报错（与 `new Cron` 抛错相同）。
pub fn compute_next_run_at(
    zone: &dyn Zone,
    cron_expr: &str,
    from: i64,
) -> Result<Option<i64>, String> {
    Ok(Cron::parse(cron_expr, zone)?.next_run(zone, from))
}

/// `buildRelativeDelaySchedule`：返回兼容展示用的 cron 与锚定服务端时钟的一次性规则。
pub fn build_relative_delay_schedule(
    zone: &dyn Zone,
    delay_minutes: i64,
    from: i64,
) -> (String, Rule) {
    let target = zone.fields(from + delay_minutes * 60 * 1_000);
    let cron_expr = format!(
        "{} {} {} {} *",
        target.minute,
        target.hour,
        target.day,
        target.month + 1
    );
    let mut rule = Rule::new();
    rule.insert("unit".into(), json!("minute"));
    rule.insert("interval".into(), json!(delay_minutes));
    rule.insert("hour".into(), json!(target.hour));
    rule.insert("minute".into(), json!(target.minute));
    rule.insert("anchorAt".into(), json!(from));
    (cron_expr, rule)
}

fn is_fixed_calendar_cron(cron_expr: &str) -> bool {
    let parts: Vec<&str> = cron_expr.split_whitespace().collect();
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    parts.len() == 5 && parts[..4].iter().all(|part| digits(part)) && parts[4] == "*"
}

/// `computeInitialAutomationNextRunAt`：一次性固定月日 cron 刚错过目标分钟时补执行或拒绝。
pub fn compute_initial_next_run_at(
    zone: &dyn Zone,
    cron_expr: &str,
    recurring: bool,
    rule: Option<&Rule>,
    from: i64,
) -> Result<Option<i64>, String> {
    let next_run_at = compute_automation_next_run_at(zone, cron_expr, rule, from)?;
    if recurring || rule.is_some() || !is_fixed_calendar_cron(cron_expr.trim()) {
        return Ok(next_run_at);
    }
    let cron = Cron::parse(cron_expr, zone)?;
    let previous_run = cron.previous_run(zone, from);
    let target_age = previous_run
        .filter(|previous| from >= *previous && from - previous <= ONE_SHOT_STALE_TARGET_WINDOW_MS)
        .map(|previous| from - previous);
    let rolled = next_run_at.is_none_or(|next| next - from > ONE_SHOT_STALE_TARGET_WINDOW_MS);
    if let Some(age) = target_age
        && rolled
    {
        if age < ONE_SHOT_MISSED_RUN_GRACE_MS {
            return Ok(Some(from));
        }
        return Err(format!(
            "一次性定时任务的目标时间（{}）已过去；相对时间请使用 delayMinutes，绝对时间请确认未来时刻后重试",
            locale_string(zone, from - age)
        ));
    }
    Ok(next_run_at)
}

/// `atTime(date, hour, minute)`：取 date 的本地年月日，时分换成给定值。
fn at_time(zone: &dyn Zone, date: i64, hour: i64, minute: i64) -> i64 {
    let fields = zone.fields(date);
    zone.make([fields.year, fields.month, fields.day, hour, minute, 0, 0])
}

/// `new Date(year, month, day)`。
fn date(zone: &dyn Zone, year: i64, month: i64, day: i64) -> i64 {
    zone.make([year, month, day, 0, 0, 0, 0])
}

/// JS 默认 `sort()`：按字符串比较。
fn js_default_sort(values: &mut [f64]) {
    values.sort_by_key(|value| js_number(*value).to_string());
}

/// `computeScheduleRuleNextRunAt`：自定义重复规则的下一次（本地日历时间）。
pub fn compute_schedule_rule_next_run_at(zone: &dyn Zone, rule: &Rule, from: i64) -> Option<i64> {
    let interval = number(rule, "interval").map_or(1, |value| value.floor().max(1.0) as i64);
    let anchor_at = int(rule, "anchorAt");
    let anchor = zone.fields(anchor_at);
    let (hour, minute) = (int(rule, "hour"), int(rule, "minute"));
    match rule.get("unit").and_then(Value::as_str) {
        Some("minute") => {
            let step = interval * 60 * 1_000;
            let steps = ((from - anchor_at).div_euclid(step) + 1).max(1);
            Some(anchor_at + steps * step)
        }
        Some("hourly") => {
            let base = zone.make([
                anchor.year,
                anchor.month,
                anchor.day,
                anchor.hour,
                minute,
                0,
                0,
            ]);
            let step = interval * 60 * 60 * 1_000;
            let steps = ((from - base).div_euclid(step) + 1).max(0);
            Some(base + steps * step)
        }
        Some("daily") => (0..36_600).find_map(|index| {
            let day = date(
                zone,
                anchor.year,
                anchor.month,
                anchor.day + index * interval,
            );
            let candidate = at_time(zone, day, hour, minute);
            (candidate > from).then_some(candidate)
        }),
        Some("weekly") => {
            let start = date(zone, anchor.year, anchor.month, anchor.day);
            let start_fields = zone.fields(start);
            // setDate(getDate() - ((getDay() + 6) % 7))：保留当天时刻。
            let monday = zone.make([
                start_fields.year,
                start_fields.month,
                start_fields.day - (start_fields.weekday + 6) % 7,
                start_fields.hour,
                start_fields.minute,
                start_fields.second,
                start_fields.ms,
            ]);
            let week_start = zone.fields(monday);
            let mut weekdays = list(rule, "weekdays")
                .filter(|values| !values.is_empty())
                .unwrap_or_else(|| vec![1.0]);
            js_default_sort(&mut weekdays);
            let mut week = 0;
            while week < 5_220 {
                for weekday in &weekdays {
                    let offset = (*weekday as i64 + 6) % 7;
                    let day = date(
                        zone,
                        week_start.year,
                        week_start.month,
                        week_start.day + week * 7 + offset,
                    );
                    let candidate = at_time(zone, day, hour, minute);
                    if candidate > from {
                        return Some(candidate);
                    }
                }
                week += interval;
            }
            None
        }
        Some("monthly") => {
            let mut offset = 0;
            while offset <= 1_200 {
                let month = zone.fields(date(zone, anchor.year, anchor.month + offset, 1));
                let candidates: Vec<i64> =
                    if rule.get("monthlyMode").and_then(Value::as_str) == Some("weekday") {
                        let weekday = list(rule, "weekdays")
                            .and_then(|values| values.first().copied())
                            .map_or(1, |value| value as i64);
                        let first = zone.fields(date(zone, month.year, month.month, 1));
                        vec![date(
                            zone,
                            month.year,
                            month.month,
                            1 + (weekday - first.weekday + 7).rem_euclid(7),
                        )]
                    } else {
                        let mut days = list(rule, "monthDays")
                            .filter(|values| !values.is_empty())
                            .unwrap_or_else(|| vec![1.0]);
                        days.sort_by(f64::total_cmp);
                        days.into_iter()
                            .map(|day| date(zone, month.year, month.month, day as i64))
                            .filter(|candidate| zone.fields(*candidate).month == month.month)
                            .collect()
                    };
                for day in candidates {
                    let candidate = at_time(zone, day, hour, minute);
                    if candidate > from {
                        return Some(candidate);
                    }
                }
                offset += interval;
            }
            None
        }
        _ => {
            // yearly：月份取 months[0]（1-12），缺省回退锚点月份；日期取 monthDays[0] 或锚点日。
            let target_month = list(rule, "months")
                .and_then(|values| values.first().copied())
                .map_or(anchor.month, |month| ((month as i64 - 1) % 12 + 12) % 12);
            let target_day = list(rule, "monthDays")
                .and_then(|values| values.first().copied())
                .map_or(anchor.day, |day| day as i64);
            let mut offset = 0;
            while offset < 400 {
                let day = date(zone, anchor.year + offset, target_month, target_day);
                if zone.fields(day).month == target_month {
                    let candidate = at_time(zone, day, hour, minute);
                    if candidate > from {
                        return Some(candidate);
                    }
                }
                offset += interval;
            }
            None
        }
    }
}

/// `inferMinuteIntervalScheduleRule`：`*/N * * * *` → 以 anchorAt 为锚点的分钟规则。
pub fn infer_minute_interval_schedule_rule(
    zone: &dyn Zone,
    cron_expr: &str,
    anchor_at: i64,
) -> Option<Rule> {
    let parts: Vec<&str> = cron_expr.split_whitespace().collect();
    if parts.len() != 5 || parts[1..].iter().any(|part| *part != "*") {
        return None;
    }
    let step = parts[0].strip_prefix("*/")?;
    let valid = step.bytes().all(|byte| byte.is_ascii_digit())
        && !step.starts_with('0')
        && !step.is_empty();
    if !valid {
        return None;
    }
    let interval: f64 = step.parse().ok()?;
    let anchor = zone.fields(anchor_at);
    let mut rule = Rule::new();
    rule.insert("unit".into(), json!("minute"));
    rule.insert("interval".into(), js_number(interval));
    rule.insert("hour".into(), json!(anchor.hour));
    rule.insert("minute".into(), json!(anchor.minute));
    rule.insert("anchorAt".into(), json!(anchor_at));
    Some(rule)
}

/// JS `Number(text)`：十进制（可带符号、小数、指数）、0x/0o/0b、Infinity、空串为 0；其他为 NaN（None）。
fn js_to_number(text: &str) -> Option<f64> {
    let text = text.trim();
    if text.is_empty() {
        return Some(0.0);
    }
    let radix = |prefix: &str, radix: u32| {
        text.get(..2)
            .filter(|head| head.eq_ignore_ascii_case(prefix))
            .and_then(|_| u64::from_str_radix(&text[2..], radix).ok())
            .map(|value| value as f64)
    };
    if let Some(value) = radix("0x", 16)
        .or_else(|| radix("0o", 8))
        .or_else(|| radix("0b", 2))
    {
        return Some(value);
    }
    match text {
        "Infinity" | "+Infinity" => return Some(f64::INFINITY),
        "-Infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    let valid = text
        .bytes()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.' | b'e' | b'E'));
    if !valid {
        return None;
    }
    text.parse::<f64>().ok()
}

fn js_integer(text: &str) -> Option<f64> {
    js_to_number(text).filter(|value| value.is_finite() && value.fract() == 0.0)
}

struct CronFields {
    minute: Option<f64>,
    hour: Option<f64>,
    month_days: Option<Vec<f64>>,
    months: Option<Vec<f64>>,
    weekdays: Option<Vec<f64>>,
}

/// `parseCronFields`：5 段 cron 的纯数字字段；通配段为 None。
fn parse_cron_fields(cron_expr: &str) -> CronFields {
    let parts: Vec<&str> = cron_expr.split_whitespace().collect();
    let empty = CronFields {
        minute: None,
        hour: None,
        month_days: None,
        months: None,
        weekdays: None,
    };
    if parts.len() < 5 {
        return empty;
    }
    let single = |token: &str| match token {
        "*" | "?" => None,
        token => js_integer(token),
    };
    let many = |token: &str| match token {
        "*" | "?" => None,
        token => {
            let values: Vec<f64> = token.split(',').filter_map(js_integer).collect();
            (!values.is_empty()).then_some(values)
        }
    };
    CronFields {
        minute: single(parts[0]),
        hour: single(parts[1]),
        month_days: many(parts[2]),
        months: many(parts[3]),
        weekdays: many(parts[4]),
    }
}

fn numbers(values: Vec<f64>) -> Value {
    Value::Array(values.into_iter().map(js_number).collect())
}

/// `buildIntervalScheduleRule`：会话侧 carrier（intervalUnit + interval）→ 权威 scheduleRule。
pub fn build_interval_schedule_rule(
    zone: &dyn Zone,
    unit: &str,
    interval: i64,
    cron_expr: &str,
    anchor_at: i64,
) -> Result<Rule, String> {
    let fields = parse_cron_fields(cron_expr);
    let anchor = zone.fields(anchor_at);
    let minute = fields
        .minute
        .map_or_else(|| json!(anchor.minute), js_number);
    let hour = fields.hour.map_or_else(|| json!(anchor.hour), js_number);
    let month_days = fields.month_days.unwrap_or_else(|| vec![anchor.day as f64]);
    let months = fields
        .months
        .unwrap_or_else(|| vec![(anchor.month + 1) as f64]);
    let weekdays = fields.weekdays.unwrap_or_else(|| vec![1.0]);
    let mut rule = Rule::new();
    rule.insert("unit".into(), json!(unit));
    rule.insert("interval".into(), json!(interval));
    match unit {
        "minute" => {
            rule.insert("hour".into(), json!(anchor.hour));
            rule.insert("minute".into(), json!(anchor.minute));
        }
        "hourly" => {
            rule.insert("hour".into(), json!(0));
            rule.insert("minute".into(), minute);
        }
        "daily" | "weekly" | "monthly" | "yearly" => {
            rule.insert("hour".into(), hour);
            rule.insert("minute".into(), minute);
            match unit {
                "weekly" => {
                    rule.insert("weekdays".into(), numbers(weekdays));
                }
                "monthly" => {
                    rule.insert("monthDays".into(), numbers(month_days));
                    rule.insert("monthlyMode".into(), json!("date"));
                }
                "yearly" => {
                    rule.insert("months".into(), numbers(months));
                    rule.insert("monthDays".into(), numbers(month_days));
                }
                _ => {}
            }
        }
        other => return Err(format!("Unsupported intervalUnit: {other}")),
    }
    rule.insert("anchorAt".into(), json!(anchor_at));
    Ok(rule)
}

/// `computeAutomationNextRunAt`：有规则按规则，否则按 cron。
pub fn compute_automation_next_run_at(
    zone: &dyn Zone,
    cron_expr: &str,
    rule: Option<&Rule>,
    from: i64,
) -> Result<Option<i64>, String> {
    match rule {
        Some(rule) => Ok(compute_schedule_rule_next_run_at(zone, rule, from)),
        None => compute_next_run_at(zone, cron_expr, from),
    }
}
