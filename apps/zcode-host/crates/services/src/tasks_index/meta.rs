//! `ZCodeTaskMeta` 的 JS 语义：对象展开顺序、undefined、zod 校验（packages/shared/src/validation.ts
//! `zcodeTaskMetaSchema`）与 `rowToMeta`（packages/services/src/session/taskIndexRepo.ts）。
//!
//! meta_json 由 Node Host 与 Rust Host 共写，读出的 meta 必须与 TS 完全相同（含键序），写入的
//! meta_json 也按 TS 对象展开的键序序列化。
use serde_json::{Map, Number, Value};
use zcode_cli_domain::{js_json, js_string, workspace_identity};

/// JS 普通对象：保留插入顺序，并区分“键存在但值为 undefined”（`None`）与键缺失。
///
/// TS 的 `{ ...a, k: undefined }` 保留 `k` 原来的位置，之后再赋值仍在原位；`JSON.stringify`
/// 跳过 undefined。用它复现 TS 写入 meta_json 时的键序。
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct JsObject(Vec<(String, Option<Value>)>);

impl JsObject {
    pub fn from_map(map: Map<String, Value>) -> Self {
        Self(map.into_iter().map(|(k, v)| (k, Some(v))).collect())
    }

    /// 已定义的值（可能是 `null`）；缺失或 undefined 返回 `None`。
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.as_ref())
    }

    /// `Object.prototype.hasOwnProperty`：值为 undefined 也算存在。
    pub fn has_own(&self, key: &str) -> bool {
        self.0.iter().any(|(k, _)| k == key)
    }

    /// 赋值：已有键原位替换，新键追加到末尾（JS 属性顺序）。
    pub fn set(&mut self, key: &str, value: Option<Value>) {
        match self.0.iter_mut().find(|(k, _)| k == key) {
            Some(entry) => entry.1 = value,
            None => self.0.push((key.to_owned(), value)),
        }
    }

    /// `a ?? b` 的左值：缺失、undefined 与 null 都视为空。
    pub fn defined(&self, key: &str) -> Option<&Value> {
        self.get(key).filter(|v| !v.is_null())
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::as_str)
    }

    /// `JSON.parse(JSON.stringify(obj))` 得到的值：跳过 undefined。
    pub fn to_value(&self) -> Value {
        Value::Object(
            self.0
                .iter()
                .filter_map(|(k, v)| v.as_ref().map(|v| (k.clone(), v.clone())))
                .collect(),
        )
    }

    /// `JSON.stringify(obj)`。
    pub fn stringify(&self) -> String {
        js_json::stringify(&self.to_value())
    }

    /// 输入边界：JSON 里没有 undefined，调用方用 `null` 表示“没有值”。除 schema 允许 null 的
    /// `target` 外，把顶层 `null` 当作 undefined，避免写出 TS 读取时会整体判非法的 meta_json。
    pub fn null_as_undefined(mut self) -> Self {
        for (key, value) in &mut self.0 {
            if key != "target" && matches!(value, Some(Value::Null)) {
                *value = None;
            }
        }
        self
    }
}

/// JS 真值。
pub(crate) fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// 身份 key：`workspaceIdentity?.trim() || workspacePath`（packages/shared resolveWorkspaceKey）。
pub fn resolve_workspace_key(workspace_path: &str, workspace_identity: Option<&str>) -> String {
    workspace_identity
        .map(js_string::trim)
        .filter(|identity| !identity.is_empty())
        .unwrap_or(workspace_path)
        .to_owned()
}

/// meta 对象上的身份 key（调用前已校验 workspacePath / workspaceIdentity 的类型）。
pub(crate) fn meta_workspace_key(meta: &JsObject) -> String {
    resolve_workspace_key(
        meta.str("workspacePath").unwrap_or_default(),
        meta.str("workspaceIdentity"),
    )
}

pub(crate) fn is_remote_identity(identity: &str) -> bool {
    workspace_identity::parse_remote(identity).is_some()
}

// ---- zod 校验（只保留 zcodeTaskMetaSchema 用到的规则） ----

const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

type Check = fn(&Value) -> Option<Value>;

enum Presence {
    Required,
    Optional,
    /// `.default(null)`：缺失时在 schema 位置补 null。
    DefaultNull,
}

struct Field {
    key: &'static str,
    presence: Presence,
    nullable: bool,
    check: Check,
}

const fn field(key: &'static str, presence: Presence, check: Check) -> Field {
    Field {
        key,
        presence,
        nullable: false,
        check,
    }
}

const fn nullable(key: &'static str, presence: Presence, check: Check) -> Field {
    Field {
        key,
        presence,
        nullable: true,
        check,
    }
}

use Presence::{DefaultNull, Optional, Required};

/// `z.object`：输出按 schema 键序，未知键剥离（strict 时报错），缺失的可选键不出现。
fn object(value: &Value, fields: &[Field], strict: bool) -> Option<Map<String, Value>> {
    let input = value.as_object()?;
    if strict && input.keys().any(|k| !fields.iter().any(|f| f.key == k)) {
        return None;
    }
    let mut out = Map::new();
    for f in fields {
        match input.get(f.key) {
            None => match f.presence {
                Required => return None,
                Optional => {}
                DefaultNull => {
                    out.insert(f.key.to_owned(), Value::Null);
                }
            },
            Some(Value::Null) if f.nullable => {
                out.insert(f.key.to_owned(), Value::Null);
            }
            Some(v) => {
                out.insert(f.key.to_owned(), (f.check)(v)?);
            }
        }
    }
    Some(out)
}

/// `z.number().int()`：JS 数值必须是安全整数；输出统一成整数（`1e3`、`2.0` → 整数）。
fn integer(value: &Value, min: i64, max: Option<i64>) -> Option<Value> {
    let number = value.as_number()?;
    let int = if let Some(i) = number.as_i64() {
        i
    } else if number.is_u64() {
        return None; // 大于 i64::MAX，必然超出安全整数。
    } else {
        let f = number.as_f64()?;
        if !f.is_finite() || f.fract() != 0.0 {
            return None;
        }
        f as i64
    };
    if (int as f64).abs() > MAX_SAFE_INTEGER || int < min || max.is_some_and(|max| int > max) {
        return None;
    }
    Some(Value::Number(Number::from(int)))
}

fn int_nonneg(v: &Value) -> Option<Value> {
    integer(v, 0, None)
}

fn int_positive(v: &Value) -> Option<Value> {
    integer(v, 1, None)
}

fn status_code(v: &Value) -> Option<Value> {
    integer(v, 100, Some(599))
}

fn string(v: &Value) -> Option<Value> {
    v.is_string().then(|| v.clone())
}

fn boolean(v: &Value) -> Option<Value> {
    v.is_boolean().then(|| v.clone())
}

/// zod 4 的字符串长度按 Unicode code point 计（不是 UTF-16 单元）。
fn string_len(v: &Value, min: usize, max: usize) -> Option<Value> {
    let s = v.as_str()?;
    let len = s.chars().count();
    (min..=max).contains(&len).then(|| v.clone())
}

fn string_min1(v: &Value) -> Option<Value> {
    string_len(v, 1, usize::MAX)
}

fn string_1_160(v: &Value) -> Option<Value> {
    string_len(v, 1, 160)
}

/// `nonEmptyStringSchema = z.string().trim().min(1)`：输出是 trim 后的值。
fn non_empty(v: &Value) -> Option<Value> {
    let s = js_string::trim(v.as_str()?);
    (!s.is_empty()).then(|| Value::String(s.to_owned()))
}

fn one_of(v: &Value, options: &[&str]) -> Option<Value> {
    v.as_str()
        .filter(|s| options.contains(s))
        .map(|_| v.clone())
}

fn workspace_purpose(v: &Value) -> Option<Value> {
    one_of(v, &["project", "conversation"])
}

fn task_mode(v: &Value) -> Option<Value> {
    one_of(v, &["yolo", "plan", "edit", "auto", "autoEdit", "build"])
}

/// `ZCODE_AGENT_PROVIDER`。
pub(crate) const ZCODE_AGENT_PROVIDER: &str = "glm";

fn agent_provider(v: &Value) -> Option<Value> {
    one_of(v, &[ZCODE_AGENT_PROVIDER])
}

fn migration_source(v: &Value) -> Option<Value> {
    one_of(v, &["claudeCode"])
}

fn persist_status(v: &Value) -> Option<Value> {
    one_of(v, &["running", "completed", "error"])
}

fn attribution_source(v: &Value) -> Option<Value> {
    one_of(v, &["provider", "runtime", "tool", "network"])
}

fn error_phase(v: &Value) -> Option<Value> {
    one_of(
        v,
        &[
            "prepare",
            "configuration",
            "connect",
            "response",
            "stream",
            "parse",
            "validation",
            "unhandled",
        ],
    )
}

fn exception_kind(v: &Value) -> Option<Value> {
    one_of(
        v,
        &[
            "api_call",
            "generic",
            "protocol",
            "provider_business",
            "transport",
            "type_error",
            "validation",
        ],
    )
}

fn transport(v: &Value) -> Option<Value> {
    one_of(v, &["http", "sse", "websocket"])
}

fn goal_status(v: &Value) -> Option<Value> {
    one_of(v, &["active", "paused", "budget_limited", "complete"])
}

/// errorAttributionSchema（`.strict()`）。
fn attribution(v: &Value) -> Option<Value> {
    const FIELDS: &[Field] = &[
        field("source", Optional, attribution_source),
        field("reason", Optional, string_1_160),
        field("errorPhase", Optional, error_phase),
        field("exceptionKind", Optional, exception_kind),
        field("providerId", Optional, string_1_160),
        field("modelId", Optional, string_1_160),
        field("providerKind", Optional, string_1_160),
        field("transport", Optional, transport),
        field("statusCode", Optional, status_code),
        field("providerErrorCode", Optional, string_1_160),
        field("retryable", Optional, boolean),
    ];
    object(v, FIELDS, true).map(Value::Object)
}

fn last_error(v: &Value) -> Option<Value> {
    const FIELDS: &[Field] = &[
        field("code", Optional, string),
        field("detail", Optional, string),
        field("message", Required, string_min1),
        field("traceId", Optional, non_empty),
        field("taskId", Optional, non_empty),
        field("attribution", Optional, attribution),
    ];
    object(v, FIELDS, false).map(Value::Object)
}

fn change_file(v: &Value) -> Option<Value> {
    const FIELDS: &[Field] = &[
        field("path", Required, string),
        field("added", Required, int_nonneg),
        field("removed", Required, int_nonneg),
        field("writeCount", Required, int_positive),
        field("lastTurnIndex", Required, int_nonneg),
    ];
    object(v, FIELDS, false).map(Value::Object)
}

fn change_files(v: &Value) -> Option<Value> {
    v.as_array()?
        .iter()
        .map(change_file)
        .collect::<Option<Vec<_>>>()
        .map(Value::Array)
}

fn change_summary(v: &Value) -> Option<Value> {
    const FIELDS: &[Field] = &[
        field("fileCount", Required, int_nonneg),
        field("added", Required, int_nonneg),
        field("removed", Required, int_nonneg),
        field("files", Required, change_files),
    ];
    object(v, FIELDS, false).map(Value::Object)
}

fn goal_time(v: &Value) -> Option<Value> {
    const FIELDS: &[Field] = &[
        field("created", Required, int_nonneg),
        field("updated", Required, int_nonneg),
    ];
    object(v, FIELDS, false).map(Value::Object)
}

/// zcodeTaskGoalSchema。
fn goal(v: &Value) -> Option<Value> {
    const FIELDS: &[Field] = &[
        field("sessionID", Required, non_empty),
        field("targetID", Required, non_empty),
        field("objective", Required, non_empty),
        nullable("summaryTitle", DefaultNull, string_min1),
        field("status", Required, goal_status),
        nullable("tokenBudget", Required, int_positive),
        field("tokensUsed", Required, int_nonneg),
        field("timeUsedSeconds", Required, int_nonneg),
        nullable("activeInputId", Optional, non_empty),
        nullable("activeRunStartedAtMs", Optional, int_nonneg),
        nullable("activeRunLastSeenAtMs", Optional, int_nonneg),
        field("time", Required, goal_time),
    ];
    object(v, FIELDS, false).map(Value::Object)
}

const META_FIELDS: &[Field] = &[
    field("taskId", Required, non_empty),
    field("traceId", Required, non_empty),
    field("title", Required, string),
    field("titleOverridden", Optional, boolean),
    field("workspacePath", Required, non_empty),
    field("workspaceIdentity", Optional, non_empty),
    field("workspacePurpose", Optional, workspace_purpose),
    field("createdAt", Required, int_nonneg),
    field("updatedAt", Required, int_nonneg),
    field("mode", Required, task_mode),
    field("model", Optional, string),
    field("thoughtLevel", Optional, non_empty),
    field("runtimeEpoch", Optional, int_nonneg),
    field("provider", Optional, agent_provider),
    field("migrationSource", Optional, migration_source),
    field("forkedFromTaskId", Optional, non_empty),
    field("cronAutomationId", Optional, non_empty),
    field("offPeakTaskId", Optional, non_empty),
    field("unreadAt", Optional, int_nonneg),
    field("status", Optional, persist_status),
    field("lastError", Optional, last_error),
    field("changeSummary", Optional, change_summary),
    nullable("target", Optional, goal),
];

/// `zcodeTaskMetaSchema.safeParse(JSON.parse(text))` 成功时的输出。
pub(crate) fn parse_meta_json(text: &str) -> Option<JsObject> {
    let value: Value = serde_json::from_str(text).ok()?;
    object(&value, META_FIELDS, false).map(JsObject::from_map)
}

// ---- tasks 行 ----

/// tasks 表的一行（列语义同 TS `TaskIndexRow`）。
#[derive(Debug, Clone)]
pub(crate) struct TaskRow {
    pub workspace_key: String,
    pub workspace_path: String,
    pub workspace_identity: Option<String>,
    pub task_id: String,
    pub title: String,
    pub task_status: Option<String>,
    pub provider: Option<String>,
    pub mode: String,
    pub model: Option<String>,
    pub migration_source: Option<String>,
    pub forked_from_task_id: Option<String>,
    pub cron_automation_id: Option<String>,
    pub off_peak_task_id: Option<String>,
    pub created_at: Value,
    pub updated_at: Value,
    pub unread_at: Option<i64>,
    pub last_unread_at: i64,
    pub pinned: bool,
    pub archived: bool,
    pub deleted: bool,
    pub title_overridden: bool,
    pub searchable_text: String,
    pub meta_json: String,
}

/// 旧 Rust snapshot 曾把本地 fallback 路径填成 identity；只按主键 workspace_key 投影身份。
pub(crate) fn resolve_row_workspace_identity(
    workspace_key: &str,
    workspace_path: &str,
    workspace_identity: Option<&str>,
) -> Option<String> {
    let column = workspace_identity.map(js_string::trim);
    // key、path、identity 三者相等时没有独立身份，不能误走远端 UI/路由。
    if workspace_key == workspace_path && column == Some(workspace_path) {
        return None;
    }
    if column == Some(workspace_key) {
        return column.map(str::to_owned);
    }
    // workspace_key 是查询与主键隔离的依据：统一格式的远端 identity 以它为准，
    // 不能让残留的 identity 列把实体投影到另一个远端。
    if is_remote_identity(workspace_key) {
        return Some(workspace_key.to_owned());
    }
    None
}

fn string_value(value: Option<&String>) -> Option<Value> {
    value.map(|s| Value::String(s.clone()))
}

/// `rowToMeta`：meta_json 合法时以它为主、列覆盖身份与产品壳字段；否则由列兜底。
pub(crate) fn row_to_meta(row: &TaskRow) -> JsObject {
    let identity = resolve_row_workspace_identity(
        &row.workspace_key,
        &row.workspace_path,
        row.workspace_identity.as_deref(),
    )
    .map(Value::String);
    let unread_at = row.unread_at.map(Value::from);
    if let Some(mut meta) = parse_meta_json(&row.meta_json) {
        meta.set("taskId", Some(Value::String(row.task_id.clone())));
        meta.set(
            "workspacePath",
            Some(Value::String(row.workspace_path.clone())),
        );
        meta.set("workspaceIdentity", identity);
        // unread 是 tasks-index 的产品壳状态：标量列覆盖其他 Host 写入的旧 meta_json。
        meta.set("unreadAt", unread_at);
        // cron / off-peak 身份以 meta_json 为准，索引列只作兜底（历史行可能只写了列）。
        let cron = meta
            .get("cronAutomationId")
            .cloned()
            .or_else(|| string_value(row.cron_automation_id.as_ref()));
        meta.set("cronAutomationId", cron);
        let off_peak = meta
            .get("offPeakTaskId")
            .cloned()
            .or_else(|| string_value(row.off_peak_task_id.as_ref()));
        meta.set("offPeakTaskId", off_peak);
        meta.set("titleOverridden", Some(Value::Bool(row.title_overridden)));
        return meta;
    }
    zcode_host_kit::log::warn(format!(
        "tasks-index meta_json invalid, using columns taskId={}",
        row.task_id
    ));
    let mut meta = JsObject::default();
    let mut put = |key: &str, value: Option<Value>| meta.set(key, value);
    put("taskId", Some(Value::String(row.task_id.clone())));
    put(
        "traceId",
        Some(Value::String(format!("zcode-{}", row.task_id))),
    );
    put("title", Some(Value::String(row.title.clone())));
    put("titleOverridden", Some(Value::Bool(row.title_overridden)));
    put(
        "workspacePath",
        Some(Value::String(row.workspace_path.clone())),
    );
    put("workspaceIdentity", identity);
    put("createdAt", Some(row.created_at.clone()));
    put("updatedAt", Some(row.updated_at.clone()));
    put("mode", Some(Value::String(row.mode.clone())));
    put("model", string_value(row.model.as_ref()));
    put(
        "provider",
        (row.provider.as_deref() == Some(ZCODE_AGENT_PROVIDER))
            .then(|| Value::String(ZCODE_AGENT_PROVIDER.to_owned())),
    );
    put(
        "migrationSource",
        string_value(row.migration_source.as_ref()),
    );
    put(
        "forkedFromTaskId",
        string_value(row.forked_from_task_id.as_ref()),
    );
    put(
        "cronAutomationId",
        string_value(row.cron_automation_id.as_ref()),
    );
    put("offPeakTaskId", string_value(row.off_peak_task_id.as_ref()));
    put("unreadAt", unread_at);
    put("status", string_value(row.task_status.as_ref()));
    meta
}

/// 入参 meta 的最低要求：投影到列的字段类型正确，否则 TS 会写出无法读回的行。
pub(crate) fn validate_input_meta(meta: &JsObject) -> anyhow::Result<()> {
    let task_id = meta.str("taskId").unwrap_or_default();
    anyhow::ensure!(!task_id.is_empty(), "task meta requires a non-empty taskId");
    anyhow::ensure!(
        meta.str("workspacePath").is_some_and(|p| !p.is_empty()),
        "task meta {task_id} requires a non-empty workspacePath"
    );
    anyhow::ensure!(
        meta.str("title").is_some() && meta.str("mode").is_some(),
        "task meta {task_id} requires string title and mode"
    );
    for key in ["createdAt", "updatedAt"] {
        anyhow::ensure!(
            meta.get(key).and_then(as_integer).is_some(),
            "task meta {task_id} requires an integer {key}"
        );
    }
    for key in [
        "workspaceIdentity",
        "status",
        "provider",
        "model",
        "migrationSource",
        "forkedFromTaskId",
        "cronAutomationId",
        "offPeakTaskId",
    ] {
        anyhow::ensure!(
            meta.defined(key).is_none_or(Value::is_string),
            "task meta {task_id} field {key} must be a string"
        );
    }
    anyhow::ensure!(
        meta.defined("unreadAt")
            .is_none_or(|v| as_integer(v).is_some()),
        "task meta {task_id} field unreadAt must be an integer"
    );
    anyhow::ensure!(
        meta.defined("titleOverridden")
            .is_none_or(Value::is_boolean),
        "task meta {task_id} field titleOverridden must be a boolean"
    );
    Ok(())
}

/// JSON 数值转整数（`2.0` 也算）。
pub(crate) fn as_integer(value: &Value) -> Option<i64> {
    let number = value.as_number()?;
    number.as_i64().or_else(|| {
        number
            .as_f64()
            .filter(|f| f.is_finite() && f.fract() == 0.0 && f.abs() <= MAX_SAFE_INTEGER)
            .map(|f| f as i64)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_object_keeps_position_of_undefined_keys() {
        let mut object = JsObject::default();
        object.set("a", Some(Value::from(1)));
        object.set("b", None);
        object.set("c", Some(Value::from(3)));
        assert_eq!(object.stringify(), r#"{"a":1,"c":3}"#);
        object.set("b", Some(Value::from(2)));
        assert_eq!(object.stringify(), r#"{"a":1,"b":2,"c":3}"#);
        assert!(object.has_own("b"));
    }

    #[test]
    fn integers_follow_js_number_rules() {
        assert_eq!(int_nonneg(&serde_json::json!(1e3)), Some(Value::from(1000)));
        assert_eq!(int_nonneg(&serde_json::json!(2.0)), Some(Value::from(2)));
        assert_eq!(int_nonneg(&serde_json::json!(1.5)), None);
        assert_eq!(int_nonneg(&serde_json::json!(-1)), None);
        assert_eq!(
            int_nonneg(&serde_json::json!(9_007_199_254_740_993u64)),
            None
        );
        assert_eq!(status_code(&serde_json::json!(600)), None);
    }

    #[test]
    fn workspace_key_trims_identity() {
        assert_eq!(resolve_workspace_key("/p", Some("  ")), "/p");
        assert_eq!(resolve_workspace_key("/p", Some(" id ")), "id");
        assert_eq!(resolve_workspace_key("/p", None), "/p");
    }
}
