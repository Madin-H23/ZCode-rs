//! BigModel monitor 用量（packages/services/src/usage-stats/providers/bigmodelUsageMonitorMapper.ts、
//! bigmodelUsageMonitorRange.ts）：时间范围、坐标轴对齐，以及 `UsageStatsSnapshot` /
//! `CodingPlanUsageSnapshot` 的映射。输入是后端 JSON，输出键序与 TS 对象字面量一致。
use std::collections::HashMap;
use std::sync::OnceLock;

use chrono::{Datelike, Duration, Local, NaiveDate, TimeZone, Timelike, Utc};
use regex::Regex;
use serde_json::{Value, json};
use zcode_host_kit::RpcError;

const ESTIMATED_TOKEN_CHAR_DIVISOR: f64 = 3.0;
const MONITOR_MAX_RANGE_DAYS: i64 = 30;
const DAY_MS: i64 = 86_400_000;
const BREAKDOWN_BUCKET_CODES: [&str; 9] = [
    "cached_input",
    "cachedInput",
    "cache_input",
    "cacheInput",
    "uncached_input",
    "uncachedInput",
    "output",
    "output_tokens",
    "outputTokens",
];

/// JS number → JSON：整数值输出为整数。
pub fn num(value: f64) -> Value {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static regex"))
}

/// `toFiniteNumber`。
fn finite(value: &Value) -> Option<f64> {
    let parsed = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) if !text.trim().is_empty() => js_number_from_str(text),
        _ => None,
    };
    parsed.filter(|v| v.is_finite())
}

/// `Number(string)` 的常见子集：去空白后的十进制 / 十六进制 / 科学计数。
fn js_number_from_str(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    if let Some(hex) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
        return i64::from_str_radix(hex, 16).ok().map(|v| v as f64);
    }
    if trimmed.contains(['_', ',']) || trimmed.eq_ignore_ascii_case("inf") || trimmed.eq_ignore_ascii_case("nan") {
        return None;
    }
    trimmed.parse::<f64>().ok()
}

fn trimmed(value: &Value) -> Option<String> {
    value.as_str().map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned)
}

/// `value ?? fallback`（只在字段缺失或为 null 时回落）。
fn or(value: &Value, fallback: Value) -> Value {
    if value.is_null() { fallback } else { value.clone() }
}

fn array(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn strings(value: &Value) -> Vec<String> {
    array(value).iter().map(|v| v.as_str().unwrap_or("").to_owned()).collect()
}

fn invalid_time_zone(zone: &str) -> RpcError {
    RpcError::new("RangeError", format!("Invalid time zone specified: {zone}"))
}

/// `formatDateInTimeZone(now, timeZone)`：`YYYY-MM-DD`；空时区用本机时区。
pub fn today_in_time_zone(time_zone: Option<&str>) -> Result<String, RpcError> {
    match time_zone.filter(|zone| !zone.is_empty()) {
        None => Ok(Local::now().format("%Y-%m-%d").to_string()),
        Some(zone) => {
            let tz: chrono_tz::Tz = resolve_time_zone(zone).ok_or_else(|| invalid_time_zone(zone))?;
            Ok(Utc::now().with_timezone(&tz).format("%Y-%m-%d").to_string())
        }
    }
}

fn resolve_time_zone(zone: &str) -> Option<chrono_tz::Tz> {
    if let Ok(tz) = zone.parse::<chrono_tz::Tz>() {
        return Some(tz);
    }
    // Intl 对 IANA 名称大小写不敏感。
    chrono_tz::TZ_VARIANTS
        .iter()
        .find(|tz| tz.name().eq_ignore_ascii_case(zone))
        .copied()
}

fn parse_date_key(key: &str) -> NaiveDate {
    let mut parts = key.split('-');
    let mut next = |default: i64| parts.next().and_then(|p| p.parse::<i64>().ok()).unwrap_or(default);
    let (year, month, day) = (next(1970), next(1), next(1));
    // Date.UTC 会把越界的月 / 日顺延；用天数偏移复现。
    let base = NaiveDate::from_ymd_opt(year as i32, 1, 1).unwrap_or_default();
    let months = base
        .checked_add_months(chrono::Months::new((month - 1).max(0) as u32))
        .unwrap_or(base);
    months + Duration::days(day - 1)
}

/// `addDaysToDateKey`。
pub fn add_days(key: &str, days: i64) -> String {
    (parse_date_key(key) + Duration::days(days)).format("%Y-%m-%d").to_string()
}

/// `resolveUsageTimeRange`：本机时区，`YYYY-MM-DD HH:mm:ss`。
pub fn usage_time_range(range: &str) -> (String, String) {
    let days = match range {
        "7d" => 7,
        _ => 30,
    }
    .min(MONITOR_MAX_RANGE_DAYS);
    let now = Local::now();
    let end = now
        .with_hour(23)
        .and_then(|t| t.with_minute(59))
        .and_then(|t| t.with_second(59))
        .unwrap_or(now);
    let start_date = end.date_naive() - Duration::days(days - 1);
    let format = |date: NaiveDate, h: u32, m: u32, s: u32| {
        format!("{:04}-{:02}-{:02} {h:02}:{m:02}:{s:02}", date.year(), date.month(), date.day())
    };
    (format(start_date, 0, 0, 0), format(end.date_naive(), 23, 59, 59))
}

#[derive(Debug)]
pub struct CodingPlanRange {
    pub start_time: String,
    pub end_time: String,
    pub granularity: &'static str,
    pub x_time: Vec<String>,
    pub start_date_key: String,
    pub end_date_key: String,
}

fn daily_axis(start: &str, days: i64) -> Vec<String> {
    (0..days).map(|index| add_days(start, index)).collect()
}

/// `resolveCodingPlanUsageTimeRange`。
pub fn coding_plan_range(request: &Value) -> Result<CodingPlanRange, RpcError> {
    let end_key = today_in_time_zone(request["timeZone"].as_str())?;
    let day_range = |start: String, days: i64, end: String| CodingPlanRange {
        start_time: format!("{start} 00:00:00"),
        end_time: format!("{end} 23:59:59"),
        granularity: "day",
        x_time: daily_axis(&start, days),
        start_date_key: start,
        end_date_key: end,
    };
    match request["range"].as_str().unwrap_or("") {
        "today" => Ok(CodingPlanRange {
            start_time: format!("{end_key} 00:00:00"),
            end_time: format!("{end_key} 23:59:59"),
            granularity: "hour",
            x_time: (0..24).map(|hour| format!("{hour:02}:00:00")).collect(),
            start_date_key: end_key.clone(),
            end_date_key: end_key,
        }),
        "7d" => Ok(day_range(add_days(&end_key, -6), 7, end_key)),
        "30d" => Ok(day_range(add_days(&end_key, -29), 30, end_key)),
        "custom" => {
            static DATE_KEY: OnceLock<Regex> = OnceLock::new();
            let normalize = |value: &Value, label: &str| {
                let text = value.as_str().map(str::trim).unwrap_or("");
                if re(&DATE_KEY, r"^\d{4}-\d{2}-\d{2}$").is_match(text) {
                    Ok(text.to_owned())
                } else {
                    Err(RpcError::new("Error", format!("coding_plan_usage_custom_{label}_date_invalid")))
                }
            };
            let start = normalize(&request["customStartDate"], "start")?;
            let end = normalize(&request["customEndDate"], "end")?;
            let days = (parse_date_key(&end) - parse_date_key(&start)).num_days() + 1;
            if days <= 0 {
                return Err(RpcError::new("Error", "coding_plan_usage_custom_range_invalid_order"));
            }
            if days > MONITOR_MAX_RANGE_DAYS {
                return Err(RpcError::new("Error", "coding_plan_usage_custom_range_too_long"));
            }
            Ok(day_range(start, days, end))
        }
        other => Err(RpcError::new("Error", format!("unsupported coding plan usage range: {other}"))),
    }
}

/// `resolveCreditUsageActivityTimeRange` / `resolveModelPerformanceTimeRange`。
pub fn trailing_range(time_zone: Option<&str>, days_back: i64) -> Result<(String, String), RpcError> {
    let end = today_in_time_zone(time_zone)?;
    Ok((format!("{} 00:00:00", add_days(&end, -days_back)), format!("{end} 23:59:59")))
}

// ── 用量统计（设置页） ──

/// `buildUsageStatsSnapshotFromMonitor`（`sourceProvider` 由调用方追加）。
pub fn usage_stats_snapshot(request: &Value, model: &Value, tool: &Value, now_ms: i64) -> Value {
    let tokens = array(&model["tokensUsage"]);
    let calls = array(&model["modelCallCount"]);
    let daily: Vec<Value> = strings(&model["x_time"])
        .into_iter()
        .enumerate()
        .map(|(index, date)| {
            let total = tokens.get(index).and_then(Value::as_f64).unwrap_or(0.0);
            let messages = calls.get(index).and_then(Value::as_f64).unwrap_or(0.0);
            json!({
                "date": date, "label": date,
                "totalCharacters": num(total * ESTIMATED_TOKEN_CHAR_DIVISOR),
                "totalEstimatedTokens": num(total),
                "sessionCount": if messages > 0.0 { 1 } else { 0 },
                "messageCount": num(messages),
                "activityScore": num(total),
            })
        })
        .collect();
    let tokens_of = |day: &Value| day["totalEstimatedTokens"].as_f64().unwrap_or(0.0);
    let total_tokens = model["totalUsage"]["totalTokensUsage"].as_f64().unwrap_or(0.0);
    let total_calls = model["totalUsage"]["totalModelCallCount"].as_f64().unwrap_or(0.0);
    let active_days = daily.iter().filter(|day| tokens_of(day) > 0.0).count();
    let most_active = daily.iter().fold(None::<&Value>, |best, day| match best {
        Some(best) if tokens_of(day) <= tokens_of(best) => Some(best),
        _ => Some(day),
    });
    let first_active = daily.iter().find(|day| tokens_of(day) > 0.0);
    let last_active = daily.iter().rev().find(|day| tokens_of(day) > 0.0);
    let summary_list = if model["totalUsage"]["modelSummaryList"].is_null() {
        &model["modelSummaryList"]
    } else {
        &model["totalUsage"]["modelSummaryList"]
    };
    let mut models: Vec<Value> = array(summary_list)
        .iter()
        .filter(|entry| entry["modelName"].as_str().is_some_and(|name| !name.is_empty()))
        .map(|entry| {
            let total = entry["totalTokens"].as_f64().unwrap_or(0.0);
            json!({
                "modelId": entry["modelName"],
                "totalCharacters": num(total * ESTIMATED_TOKEN_CHAR_DIVISOR),
                "totalEstimatedTokens": num(total),
                "inputCharacters": 0, "inputEstimatedTokens": 0,
                "outputCharacters": 0, "outputEstimatedTokens": 0,
                "sessionCount": 0, "messageCount": 0,
                "share": num(if total_tokens > 0.0 { total / total_tokens } else { 0.0 }),
            })
        })
        .collect();
    models.sort_by(|a, b| tokens_of(b).partial_cmp(&tokens_of(a)).unwrap_or(std::cmp::Ordering::Equal));
    let favorite = models.first().map(|top| {
        json!({"modelId": top["modelId"], "totalCharacters": top["totalCharacters"],
            "totalEstimatedTokens": top["totalEstimatedTokens"], "share": top["share"]})
    });
    let max_score = daily.iter().map(tokens_of).fold(0.0, f64::max);
    let date_of = |day: Option<&Value>| day.map(|d| d["date"].clone()).unwrap_or(Value::Null);
    json!({
        "range": request["range"],
        "generatedAt": now_ms,
        "timeZone": or(&request["timeZone"], json!("UTC")),
        "estimatedTokenCharDivisor": 3,
        "summary": {
            "totalSessions": num(total_calls),
            "totalCharacters": num(total_tokens * ESTIMATED_TOKEN_CHAR_DIVISOR),
            "totalEstimatedTokens": num(total_tokens),
            "totalMessages": num(total_calls),
            "activeDays": active_days,
            "mostActiveDay": most_active.cloned().unwrap_or(Value::Null),
            "favoriteModel": favorite.unwrap_or(Value::Null),
            "longestSessionMs": 0,
            "longestStreakDays": 0,
            "currentStreakDays": 0,
            "firstActivityDate": date_of(first_active),
            "lastActivityDate": date_of(last_active),
            "peakHour": null,
        },
        "daily": daily,
        "heatmap": {
            "startDate": date_of(first_active),
            "endDate": date_of(last_active),
            "maxActivityScore": num(max_score),
            "weeks": [],
            "monthLabels": [],
        },
        "models": models,
        "source": "bigmodel-monitor",
        "tools": tool_usages(tool),
    })
}

fn tool_usages(tool: &Value) -> Vec<Value> {
    let mut tools: Vec<Value> = [
        ("network-search", "联网搜索 MCP", "networkSearchCount", "totalNetworkSearchCount"),
        ("web-reader", "网页读取 MCP", "webReadMcpCount", "totalWebReadMcpCount"),
        ("zread", "开源仓库 MCP", "zreadMcpCount", "totalZreadMcpCount"),
    ]
    .into_iter()
    .map(|(code, name, daily_key, total_key)| {
        let daily = tool[daily_key].as_array().cloned().unwrap_or_default();
        let total = match tool["totalUsage"][total_key].as_f64() {
            Some(total) => num(total),
            None => num(daily.iter().map(|v| v.as_f64().unwrap_or(0.0)).sum()),
        };
        json!({"toolCode": code, "displayName": name, "totalCalls": total, "dailyCalls": daily})
    })
    .collect();
    let search = tool["totalUsage"]["totalSearchMcpCount"].as_f64().unwrap_or(0.0);
    if search > 0.0 {
        tools.push(json!({"toolCode": "search-mcp", "displayName": "Search MCP", "totalCalls": num(search), "dailyCalls": []}));
    }
    tools
}

// ── Coding Plan 用量面板 ──

fn number_series(values: &Value) -> Vec<f64> {
    array(values).iter().map(|v| finite(v).unwrap_or(0.0)).collect()
}

fn sum(values: &[f64]) -> f64 {
    values.iter().sum()
}

fn sum_series(list: &[&Value]) -> Option<Vec<f64>> {
    let max = list.iter().map(|s| array(s).len()).max().unwrap_or(0);
    if max == 0 {
        return None;
    }
    Some(
        (0..max)
            .map(|index| list.iter().map(|s| array(s).get(index).and_then(finite).unwrap_or(0.0)).sum())
            .collect(),
    )
}

fn first_non_empty(list: Vec<Option<Vec<f64>>>) -> Vec<f64> {
    list.into_iter()
        .flatten()
        .find(|series| series.iter().any(|v| *v > 0.0))
        .unwrap_or_default()
}

fn axis_value(value: &str, granularity: &str) -> String {
    static TIME: OnceLock<Regex> = OnceLock::new();
    static DATE: OnceLock<Regex> = OnceLock::new();
    if granularity == "hour"
        && let Some(captures) = re(&TIME, r"(\d{2}):(\d{2})(?::(\d{2}))?").captures(value)
    {
        return format!(
            "{}:{}:{}",
            &captures[1],
            &captures[2],
            captures.get(3).map_or("00", |m| m.as_str())
        );
    }
    if let Some(captures) = re(&DATE, r"(\d{4})-(\d{2})-(\d{2})").captures(value) {
        return format!("{}-{}-{}", &captures[1], &captures[2], &captures[3]);
    }
    value.to_owned()
}

/// `alignUsageSeries`。
fn align(source_x: &[String], target_x: &[String], values: &[f64], granularity: &str) -> Vec<Value> {
    if source_x.is_empty() && values.len() == target_x.len() {
        return values.iter().map(|v| num(*v)).collect();
    }
    let mut by_time = HashMap::new();
    for (index, time) in source_x.iter().enumerate() {
        by_time.insert(axis_value(time, granularity), values.get(index).copied().unwrap_or(0.0));
    }
    target_x
        .iter()
        .enumerate()
        .map(|(index, time)| {
            let value = by_time.get(&axis_value(time, granularity)).copied().unwrap_or_else(|| {
                if source_x.is_empty() { values.get(index).copied().unwrap_or(0.0) } else { 0.0 }
            });
            num(value)
        })
        .collect()
}

fn sort_by_order(items: &mut [Value]) {
    items.sort_by(|a, b| {
        let (a, b) = (a["sortOrder"].as_f64().unwrap_or(0.0), b["sortOrder"].as_f64().unwrap_or(0.0));
        a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
    });
}

fn sort_order(item: &Value, index: usize) -> Value {
    or(&item["sortOrder"], json!(index))
}

fn is_breakdown_bucket(item: &Value) -> bool {
    if let Some(code) = trimmed(&item["modelCode"])
        && BREAKDOWN_BUCKET_CODES.contains(&code.as_str())
    {
        return true;
    }
    let name = item["modelName"].as_str().map(|n| n.trim().to_lowercase());
    matches!(name.as_deref(), Some("缓存" | "未缓存" | "输出"))
}

fn token_series(item: &Value) -> Vec<f64> {
    first_non_empty(vec![
        Some(number_series(&item["totalTokensUsage"])).filter(|_| item["totalTokensUsage"].is_array()),
        Some(number_series(&item["tokensUsage"])).filter(|_| item["tokensUsage"].is_array()),
        sum_series(&[&item["cachedInputTokensUsage"], &item["uncachedInputTokensUsage"], &item["outputTokensUsage"]]),
    ])
}

fn credit_model_data(usage: &Value, x_time: &[String], granularity: &str) -> Vec<Value> {
    let source_x = strings(&usage["xTime"]);
    let mut items: Vec<Value> = array(&usage["modelDataList"])
        .iter()
        .filter(|item| trimmed(&item["modelName"]).is_some() || trimmed(&item["modelCode"]).is_some())
        .filter(|item| !is_breakdown_bucket(item))
        .enumerate()
        .map(|(index, item)| {
            let aligned = |key: &str| align(&source_x, x_time, &number_series(&item[key]), granularity);
            let tokens = token_series(item);
            let credits = first_non_empty(vec![
                Some(number_series(&item["totalCreditsUsage"])).filter(|_| item["totalCreditsUsage"].is_array()),
                sum_series(&[&item["cachedInputCreditsUsage"], &item["uncachedInputCreditsUsage"], &item["outputCreditsUsage"]]),
            ]);
            let name = trimmed(&item["modelName"]).or_else(|| trimmed(&item["modelCode"])).unwrap_or_else(|| "unknown".into());
            json!({
                "modelName": name,
                "sortOrder": sort_order(item, index),
                "tokensUsage": align(&source_x, x_time, &tokens, granularity),
                "creditsUsage": align(&source_x, x_time, &credits, granularity),
                "cachedInputTokensUsage": aligned("cachedInputTokensUsage"),
                "uncachedInputTokensUsage": aligned("uncachedInputTokensUsage"),
                "outputTokensUsage": aligned("outputTokensUsage"),
                "cachedInputCreditsUsage": aligned("cachedInputCreditsUsage"),
                "uncachedInputCreditsUsage": aligned("uncachedInputCreditsUsage"),
                "outputCreditsUsage": aligned("outputCreditsUsage"),
                "totalTokens": num(finite(&item["totalTokens"]).unwrap_or_else(|| sum(&tokens))),
                "totalCredits": num(finite(&item["totalCredits"]).unwrap_or_else(|| sum(&credits))),
            })
        })
        .collect();
    sort_by_order(&mut items);
    items
}

fn credit_tool_data(usage: &Value, x_time: &[String], granularity: &str) -> Vec<Value> {
    let source_x = strings(&usage["xTime"]);
    let mut items: Vec<Value> = array(&usage["mcpDataList"])
        .iter()
        .filter(|item| ["mcpCode", "mcpName", "toolCode", "toolName"].iter().any(|key| trimmed(&item[*key]).is_some()))
        .enumerate()
        .map(|(index, item)| {
            let usage_series = first_non_empty(vec![
                Some(number_series(&item["mcpCallCount"])).filter(|_| item["mcpCallCount"].is_array()),
                Some(number_series(&item["usageCount"])).filter(|_| item["usageCount"].is_array()),
            ]);
            let credit_series = number_series(&item["creditsUsage"]);
            let code = trimmed(&item["mcpCode"])
                .or_else(|| trimmed(&item["toolCode"]))
                .or_else(|| trimmed(&item["mcpName"]))
                .unwrap_or_else(|| "unknown".into());
            let name = trimmed(&item["mcpName"])
                .or_else(|| trimmed(&item["toolName"]))
                .or_else(|| trimmed(&item["mcpCode"]))
                .unwrap_or_else(|| code.clone());
            json!({
                "toolCode": code,
                "toolName": name,
                "sortOrder": sort_order(item, index),
                "usageCount": align(&source_x, x_time, &usage_series, granularity),
                "creditsUsage": align(&source_x, x_time, &credit_series, granularity),
                "totalUsageCount": num(finite(&item["totalUsageCount"]).unwrap_or_else(|| sum(&usage_series))),
                "totalCredits": num(finite(&item["totalCredits"]).unwrap_or_else(|| sum(&credit_series))),
            })
        })
        .collect();
    sort_by_order(&mut items);
    items
}

fn guess_granularity(usage: &Value) -> &'static str {
    static HOUR: OnceLock<Regex> = OnceLock::new();
    if strings(&usage["xTime"]).iter().any(|v| re(&HOUR, r"\d{2}:\d{2}").is_match(v)) {
        "hour"
    } else {
        "day"
    }
}

fn credit_tokens_total(usage: &Value) -> f64 {
    let items = array(&usage["modelDataList"]);
    let has_real = items.iter().any(|item| !is_breakdown_bucket(item));
    items
        .iter()
        .filter(|item| !has_real || !is_breakdown_bucket(item))
        .map(|item| finite(&item["totalTokens"]).unwrap_or_else(|| sum(&token_series(item))))
        .sum()
}

fn level_for(value: f64, max: f64) -> i64 {
    if value <= 0.0 || max <= 0.0 {
        return 0;
    }
    ((value / max) * 4.0).ceil().clamp(1.0, 4.0) as i64
}

fn day_index(key: &str) -> Option<i64> {
    let date = NaiveDate::parse_from_str(key, "%Y-%m-%d").ok()?;
    Some(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?).timestamp_millis().div_euclid(DAY_MS))
}

fn day_key(index: i64) -> String {
    Utc.timestamp_millis_opt(index * DAY_MS)
        .single()
        .map(|t| t.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

fn activity(payload: &Value) -> Value {
    static DATE: OnceLock<Regex> = OnceLock::new();
    let mut cells: Vec<Value> = array(&payload["series"])
        .iter()
        .filter_map(|item| {
            let date = item["date"].as_str().map(str::trim).unwrap_or("").to_owned();
            if !re(&DATE, r"^\d{4}-\d{2}-\d{2}$").is_match(&date) {
                return None;
            }
            Some(json!({
                "date": date,
                "totalTokens": num(finite(&item["totalTokens"]).unwrap_or(0.0)),
                "turnCount": num(finite(&item["modelCallCount"]).unwrap_or(0.0)),
                "toolCallCount": num(finite(&item["mcpCalls"]).unwrap_or(0.0)),
            }))
        })
        .collect();
    cells.sort_by(|a, b| a["date"].as_str().cmp(&b["date"].as_str()));
    let tokens = |cell: &Value| cell["totalTokens"].as_f64().unwrap_or(0.0);
    let max = cells.iter().map(tokens).fold(0.0, f64::max);
    let by_date: HashMap<String, &Value> = cells.iter().map(|c| (c["date"].as_str().unwrap_or("").to_owned(), c)).collect();
    let mut weeks: Vec<(i64, Value)> = Vec::new();
    for cell in &cells {
        let Some(index) = day_index(cell["date"].as_str().unwrap_or("")) else { continue };
        let weekday = Utc
            .timestamp_millis_opt(index * DAY_MS)
            .single()
            .map(|t| t.weekday().num_days_from_sunday() as i64)
            .unwrap_or(0);
        let start = index - weekday;
        if weeks.iter().any(|(s, _)| *s == start) {
            continue;
        }
        let days: Vec<Value> = (0..7)
            .map(|offset| match by_date.get(&day_key(start + offset)) {
                Some(source) => {
                    let mut day = (*source).clone();
                    day["level"] = json!(level_for(tokens(source), max));
                    day
                }
                None => Value::Null,
            })
            .collect();
        weeks.push((start, json!({"weekIndex": weeks.len(), "days": days})));
    }
    weeks.sort_by_key(|(start, _)| *start);
    let weeks: Vec<Value> = weeks
        .into_iter()
        .enumerate()
        .map(|(index, (_, mut week))| {
            week["weekIndex"] = json!(index);
            week
        })
        .collect();
    let summary = &payload["summary"];
    json!({
        "summary": {
            "totalTokens": num(finite(&summary["totalTokens"]).unwrap_or(0.0)),
            "peakDailyTokens": num(finite(&summary["peakDailyTokens"]).unwrap_or(0.0)),
            "peakDailyTokensDate": trimmed(&summary["peakDailyTokensDate"]),
            "totalUsageDurationMs": num(finite(&summary["totalUsageDurationMs"]).unwrap_or(0.0)),
            "currentStreakDays": num(finite(&summary["currentStreakDays"]).unwrap_or(0.0)),
            "longestStreakDays": num(finite(&summary["longestStreakDays"]).unwrap_or(0.0)),
            "favoriteModelName": null,
        },
        "heatmap": {
            "startDate": cells.first().map(|c| c["date"].clone()).unwrap_or(Value::Null),
            "endDate": cells.last().map(|c| c["date"].clone()).unwrap_or(Value::Null),
            "maxTokens": num(max),
            "weeks": weeks,
        },
    })
}

fn detail_summary(summary: &Value) -> Value {
    let metric = |key: &str, field: &str| finite(&summary[key][field]).map(num).unwrap_or(Value::Null);
    json!({
        "cacheHitRate": metric("cacheHitRate", "value"),
        "cacheHitRateTrend": metric("cacheHitRate", "trend"),
        "totalCredits": num(finite(&summary["totalCredits"]["value"]).unwrap_or(0.0)),
        "totalCreditsTrend": metric("totalCredits", "trend"),
        "averageDailyCredits": num(finite(&summary["averageDailyCredits"]["value"]).unwrap_or(0.0)),
        "averageDailyCreditsTrend": metric("averageDailyCredits", "trend"),
    })
}

pub struct CodingPlanInputs<'a> {
    pub request: &'a Value,
    pub provider: &'a Value,
    pub quota: Value,
    pub range: &'a CodingPlanRange,
    pub activity: &'a Value,
    pub model_detail: &'a Value,
    pub tool_detail: &'a Value,
    pub health: &'a Value,
    pub now_ms: i64,
}

/// `buildCodingPlanUsageSnapshotFromMonitor`（只走新版 credit-usage 数据面）。
pub fn coding_plan_snapshot(inputs: CodingPlanInputs<'_>) -> Value {
    let (x_time, granularity) = (&inputs.range.x_time, inputs.range.granularity);
    let model_usage = &inputs.model_detail["modelUsage"];
    let tool_usage = &inputs.tool_detail["mcpUsage"];
    // getCodingPlanUsageSnapshot 不传旧版 model-usage / tool-usage：没有 credit-usage 时为空。
    let model_summary: Vec<Value> = if model_usage.is_null() {
        Vec::new()
    } else {
        let source_x = strings(&model_usage["xTime"]);
        credit_model_data(model_usage, &source_x, guess_granularity(model_usage))
            .into_iter()
            .map(|item| json!({"modelName": item["modelName"], "totalTokens": item["totalTokens"], "totalCredits": item["totalCredits"], "sortOrder": item["sortOrder"]}))
            .collect()
    };
    let tool_summary: Vec<Value> = if tool_usage.is_null() {
        Vec::new()
    } else {
        let source_x = strings(&tool_usage["xTime"]);
        credit_tool_data(tool_usage, &source_x, guess_granularity(tool_usage))
            .into_iter()
            .map(|item| json!({"toolCode": item["toolCode"], "toolName": item["toolName"], "totalUsageCount": item["totalUsageCount"], "totalCredits": item["totalCredits"], "sortOrder": item["sortOrder"]}))
            .collect()
    };
    let model_data = if model_usage.is_null() {
        Vec::new()
    } else {
        credit_model_data(model_usage, x_time, granularity)
    };
    let tool_data = if tool_usage.is_null() {
        Vec::new()
    } else {
        credit_tool_data(tool_usage, x_time, granularity)
    };
    let total_tokens = if model_usage.is_null() { 0.0 } else { credit_tokens_total(model_usage) };
    let mut activity = activity(inputs.activity);
    activity["summary"]["favoriteModelName"] =
        model_summary.first().map(|m| m["modelName"].clone()).unwrap_or(Value::Null);
    let health = inputs.health;
    json!({
        "range": inputs.request["range"],
        "rangeStartDate": inputs.range.start_date_key,
        "rangeEndDate": inputs.range.end_date_key,
        "generatedAt": inputs.now_ms,
        "sourceProvider": inputs.provider,
        "quota": inputs.quota,
        "activity": activity,
        "detail": {
            "model": detail_summary(&inputs.model_detail["summary"]),
            "tool": detail_summary(&inputs.tool_detail["summary"]),
        },
        "modelUsage": {
            "xTime": x_time,
            "granularity": granularity,
            "totalModelCallCount": array(&model_usage["modelDataList"]).len(),
            "totalTokensUsage": num(total_tokens),
            "modelDataList": model_data,
            "modelSummaryList": model_summary,
        },
        "toolUsage": {
            "xTime": x_time,
            "granularity": granularity,
            "toolDataList": tool_data,
            "toolSummaryList": tool_summary,
        },
        "health": {
            "xTime": if health["x_time"].is_null() { or(&health["xTime"], json!([])) } else { health["x_time"].clone() },
            "proMaxDecodeSpeed": number_series(&health["proMaxDecodeSpeed"]).into_iter().map(num).collect::<Vec<_>>(),
            "liteDecodeSpeed": number_series(&health["liteDecodeSpeed"]).into_iter().map(num).collect::<Vec<_>>(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_axes_and_alignment_match_node() {
        assert_eq!(add_days("2026-03-01", -1), "2026-02-28");
        assert_eq!(add_days("2024-12-31", 1), "2025-01-01");
        let range = coding_plan_range(&json!({"range": "custom", "customStartDate": "2026-01-30", "customEndDate": "2026-02-02"})).unwrap();
        assert_eq!(range.x_time, ["2026-01-30", "2026-01-31", "2026-02-01", "2026-02-02"]);
        assert_eq!(range.start_time, "2026-01-30 00:00:00");
        assert_eq!(
            coding_plan_range(&json!({"range": "custom", "customStartDate": "2026-02-02", "customEndDate": "2026-01-01"})).unwrap_err().message,
            "coding_plan_usage_custom_range_invalid_order"
        );
        assert_eq!(
            coding_plan_range(&json!({"range": "custom", "customStartDate": "2026-01-01", "customEndDate": "2026-03-01"})).unwrap_err().message,
            "coding_plan_usage_custom_range_too_long"
        );
        assert_eq!(coding_plan_range(&json!({"range": "today", "timeZone": "Asia/Shanghai"})).unwrap().x_time.len(), 24);
        assert_eq!(coding_plan_range(&json!({"range": "7d", "timeZone": "Nope/Zone"})).unwrap_err().message, "Invalid time zone specified: Nope/Zone");
        let x = vec!["2026-01-01".to_owned(), "2026-01-02".to_owned()];
        assert_eq!(align(&["2026-01-02 00:00".to_owned()], &x, &[5.0], "day"), vec![json!(0), json!(5)]);
        assert_eq!(align(&[], &x, &[1.0, 2.5], "day"), vec![json!(1), json!(2.5)]);
        assert_eq!(axis_value("2026-01-01 03:04", "hour"), "03:04:00");
    }

    #[test]
    fn coding_plan_snapshot_maps_credit_usage() {
        let range = CodingPlanRange {
            start_time: String::new(),
            end_time: String::new(),
            granularity: "day",
            x_time: vec!["2026-01-01".into(), "2026-01-02".into()],
            start_date_key: "2026-01-01".into(),
            end_date_key: "2026-01-02".into(),
        };
        let model_detail = json!({
            "summary": {"cacheHitRate": {"value": "0.5", "trend": null}, "totalCredits": {"value": 12}},
            "modelUsage": {"xTime": ["2026-01-01", "2026-01-02"], "modelDataList": [
                {"modelName": "GLM-5", "sortOrder": 2, "tokensUsage": [10, 20], "totalCreditsUsage": [1, 2]},
                {"modelName": "缓存", "tokensUsage": [99, 99]},
                {"modelCode": "glm-4", "sortOrder": 1, "cachedInputTokensUsage": [1, 1], "outputTokensUsage": [2, 2], "totalTokens": 7},
            ]},
        });
        let tool_detail = json!({"mcpUsage": {"xTime": ["2026-01-02"], "mcpDataList": [{"mcpName": "search", "usageCount": [4], "creditsUsage": [3]}]}});
        let activity = json!({"summary": {"totalTokens": 30}, "series": [{"date": "2026-01-02", "totalTokens": 30}, {"date": "bad"}]});
        let snapshot = coding_plan_snapshot(CodingPlanInputs {
            request: &json!({"range": "7d"}),
            provider: &json!({"id": "p", "name": "P"}),
            quota: Value::Null,
            range: &range,
            activity: &activity,
            model_detail: &model_detail,
            tool_detail: &tool_detail,
            health: &json!({"xTime": ["a"], "liteDecodeSpeed": ["1.5", null]}),
            now_ms: 1,
        });
        let models = &snapshot["modelUsage"]["modelDataList"];
        assert_eq!(models[0]["modelName"], "glm-4");
        assert_eq!(models[0]["tokensUsage"], json!([3, 3]));
        assert_eq!(models[1]["creditsUsage"], json!([1, 2]));
        assert_eq!(models[1]["totalTokens"], 30);
        assert_eq!(snapshot["modelUsage"]["totalTokensUsage"], 37);
        assert_eq!(snapshot["modelUsage"]["totalModelCallCount"], 3);
        assert_eq!(snapshot["toolUsage"]["toolDataList"][0]["usageCount"], json!([0, 4]));
        assert_eq!(snapshot["toolUsage"]["toolDataList"][0]["toolCode"], "search");
        assert_eq!(snapshot["activity"]["summary"]["favoriteModelName"], "glm-4");
        assert_eq!(snapshot["activity"]["heatmap"]["weeks"][0]["days"][5]["level"], 4);
        assert_eq!(snapshot["detail"]["model"]["cacheHitRate"], json!(0.5));
        assert_eq!(snapshot["detail"]["tool"]["totalCredits"], 0);
        assert_eq!(snapshot["health"]["liteDecodeSpeed"], json!([1.5, 0]));
    }

    #[test]
    fn usage_stats_snapshot_maps_legacy_monitor() {
        let model = json!({"x_time": ["d1", "d2"], "tokensUsage": [0, 9], "modelCallCount": [0, 2],
            "totalUsage": {"totalTokensUsage": 9, "totalModelCallCount": 2, "modelSummaryList": [{"modelName": "a", "totalTokens": 3}, {"modelName": "b", "totalTokens": 6}]}});
        let tool = json!({"networkSearchCount": [1, 2], "totalUsage": {"totalSearchMcpCount": 5}});
        let snapshot = usage_stats_snapshot(&json!({"range": "7d"}), &model, &tool, 5);
        assert_eq!(snapshot["summary"]["mostActiveDay"]["date"], "d2");
        assert_eq!(snapshot["summary"]["favoriteModel"]["modelId"], "b");
        assert_eq!(snapshot["models"][1]["share"], json!(3.0 / 9.0));
        assert_eq!(snapshot["timeZone"], "UTC");
        assert_eq!(snapshot["tools"][0]["totalCalls"], 3);
        assert_eq!(snapshot["tools"][3]["toolCode"], "search-mcp");
        assert_eq!(snapshot["daily"][1]["totalCharacters"], 27);
    }
}
