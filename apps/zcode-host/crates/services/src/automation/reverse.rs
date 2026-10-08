//! CLI → Host 的 automation 反向请求（zcodeAgentService.ts 的 `automation/*` 分支）。
//!
//! workspace 一律取发请求的 CLI 进程所属 workspace，不进协议参数。参数按 zcode-protocol 的
//! strict schema 校验（`nonEmptyString` 会 trim），失败回 -32602；业务错误回 -32603。
use std::sync::Arc;

use serde_json::{Map, Value, json};
use zcode_host_kit::log;

use super::service::{AutomationService, Scope};
use crate::agent::extensions::{ReverseContext, ReverseHandler, ReverseReply};
use crate::agent::{AgentRuntime, WorkspaceTarget};
use crate::tasks_index::TasksIndex;
use crate::tasks_index::automation_repo::{parse_bot_delivery_target, parse_model_selection};

const TASK_MODES: [&str; 6] = ["yolo", "plan", "edit", "auto", "autoEdit", "build"];
const INTERVAL_UNITS: [&str; 6] = ["minute", "hourly", "daily", "weekly", "monthly", "yearly"];

type Parsed = Option<Map<String, Value>>;

/// zod 校验的单个字段：键缺失 = undefined；`null` 对 optional 字段非法（除非 nullable）。
struct Fields<'a> {
    input: &'a Map<String, Value>,
    out: Map<String, Value>,
    ok: bool,
}

fn positive_int(value: &Value, max: Option<f64>) -> bool {
    value.as_f64().is_some_and(|number| {
        number.fract() == 0.0 && number > 0.0 && max.is_none_or(|max| number <= max)
    })
}

impl<'a> Fields<'a> {
    fn new(input: &'a Map<String, Value>, allowed: &[&str]) -> Self {
        let ok = input.keys().all(|key| allowed.contains(&key.as_str()));
        Self {
            input,
            out: Map::new(),
            ok,
        }
    }

    fn field(&mut self, key: &str, required: bool, check: impl Fn(&Value) -> Option<Value>) {
        match self.input.get(key) {
            None if required => self.ok = false,
            None => {}
            Some(value) => match check(value) {
                Some(parsed) => {
                    self.out.insert(key.to_owned(), parsed);
                }
                None => self.ok = false,
            },
        }
    }

    fn finish(self) -> Parsed {
        self.ok.then_some(self.out)
    }
}

fn non_empty(value: &Value) -> Option<Value> {
    let text = value.as_str()?.trim();
    (!text.is_empty()).then(|| json!(text))
}

fn any_string(value: &Value) -> Option<Value> {
    value.as_str().map(|text| json!(text))
}

fn boolean(value: &Value) -> Option<Value> {
    value.is_boolean().then(|| value.clone())
}

fn one_of(values: &'static [&'static str]) -> impl Fn(&Value) -> Option<Value> {
    move |value| {
        value
            .as_str()
            .filter(|text| values.contains(text))
            .map(|text| json!(text))
    }
}

fn int_in(min: f64, max: f64) -> impl Fn(&Value) -> Option<Value> {
    move |value| {
        value
            .as_f64()
            .filter(|number| number.fract() == 0.0 && *number >= min && *number <= max)
            .map(|_| value.clone())
    }
}

/// `zcodeAutomationCreateParamsSchema`。
pub fn parse_create(params: &Value) -> Parsed {
    let input = params.as_object()?;
    let mut fields = Fields::new(
        input,
        &[
            "title",
            "cronExpr",
            "relativeDelayMinutes",
            "prompt",
            "modelSelection",
            "mode",
            "targetTaskId",
            "botDeliveryTarget",
            "recurring",
            "maxRuns",
            "intervalUnit",
            "interval",
        ],
    );
    fields.field("title", false, any_string);
    fields.field("cronExpr", true, non_empty);
    fields.field("relativeDelayMinutes", false, |value| {
        positive_int(value, Some(525_600.0)).then(|| value.clone())
    });
    fields.field("prompt", true, non_empty);
    fields.field("modelSelection", false, parse_model_selection);
    fields.field("mode", false, one_of(&TASK_MODES));
    fields.field("targetTaskId", false, non_empty);
    fields.field("botDeliveryTarget", false, parse_bot_delivery_target);
    fields.field("recurring", false, boolean);
    fields.field("maxRuns", false, |value| {
        positive_int(value, None).then(|| value.clone())
    });
    fields.field("intervalUnit", false, one_of(&INTERVAL_UNITS));
    fields.field("interval", false, int_in(1.0, 200.0));
    let out = fields.finish()?;
    let unit = out.contains_key("intervalUnit");
    let refined = unit == out.contains_key("interval")
        && (!unit || !out.contains_key("relativeDelayMinutes"))
        && (!unit || out.get("recurring") != Some(&json!(false)))
        && (!unit || !out.contains_key("maxRuns"));
    refined.then_some(out)
}

/// `zcodeAutomationUpdateParamsSchema`。
pub fn parse_update(params: &Value) -> Parsed {
    let input = params.as_object()?;
    let mut fields = Fields::new(
        input,
        &[
            "automationId",
            "title",
            "cronExpr",
            "prompt",
            "recurring",
            "maxRuns",
            "intervalUnit",
            "interval",
        ],
    );
    fields.field("automationId", true, non_empty);
    fields.field("title", false, non_empty);
    fields.field("cronExpr", false, non_empty);
    fields.field("prompt", false, non_empty);
    fields.field("recurring", false, boolean);
    fields.field("maxRuns", false, |value| {
        (value.is_null() || positive_int(value, None)).then(|| value.clone())
    });
    fields.field("intervalUnit", false, one_of(&INTERVAL_UNITS));
    fields.field("interval", false, int_in(1.0, 200.0));
    let out = fields.finish()?;
    let has = |key: &str| out.contains_key(key);
    let max_runs = out.get("maxRuns");
    let recurring_true = out.get("recurring") == Some(&json!(true));
    let refined = [
        "title",
        "cronExpr",
        "prompt",
        "recurring",
        "maxRuns",
        "intervalUnit",
    ]
    .iter()
    .any(|key| has(key))
        && (max_runs != Some(&Value::Null) || recurring_true)
        && (!recurring_true || !max_runs.is_some_and(Value::is_number))
        && has("intervalUnit") == has("interval")
        && (!has("intervalUnit") || out.get("recurring") != Some(&json!(false)))
        && (!has("intervalUnit")
            || max_runs.is_none()
            || (max_runs == Some(&Value::Null) && recurring_true));
    refined.then_some(out)
}

fn parse_id(params: &Value, key: &str) -> Parsed {
    let input = params.as_object()?;
    let mut fields = Fields::new(input, &[key]);
    fields.field(key, true, non_empty);
    fields.finish()
}

/// `toProtocolAutomation`：协议结果只透出这些字段（undefined 省略）。
pub fn to_protocol_automation(automation: &Value) -> Value {
    let keys = [
        "automationId",
        "title",
        "cronExpr",
        "prompt",
        "modelSelection",
        "mode",
        "targetTaskId",
        "enabled",
        "lifecycleStatus",
        "nextRunAt",
        "lastRunAt",
        "runCount",
        "recurring",
        "maxRuns",
        "scheduleRule",
    ];
    let object: Map<String, Value> = keys
        .iter()
        .filter_map(|key| {
            automation
                .get(*key)
                .map(|value| ((*key).to_owned(), value.clone()))
        })
        .collect();
    Value::Object(object)
}

fn invalid(message: &str) -> ReverseReply {
    ReverseReply::Error {
        code: -32602,
        message: message.to_owned(),
    }
}

fn internal(error: impl std::fmt::Display) -> ReverseReply {
    ReverseReply::Error {
        code: -32603,
        message: error.to_string(),
    }
}

fn scope_of(target: &WorkspaceTarget) -> Scope {
    Scope {
        workspace_path: target.workspace_path.clone(),
        workspace_identity: target.workspace_identity.clone(),
    }
}

async fn handle(
    context: ReverseContext,
    service: Arc<AutomationService>,
    index: TasksIndex,
) -> ReverseReply {
    let scope = scope_of(&context.target);
    // 只有 list 用 `request.params ?? {}`；其余 schema 对 null 直接判非法。
    let params = if context.params.is_null() && context.method == "automation/list" {
        json!({})
    } else {
        context.params.clone()
    };
    match context.method.as_str() {
        "automation/create" => {
            let Some(parsed) = parse_create(&params) else {
                return invalid("Invalid automation create params");
            };
            let mut input = parsed.clone();
            input.insert(
                "title".into(),
                parsed.get("title").cloned().unwrap_or_else(|| json!("")),
            );
            input.insert("workspacePath".into(), json!(scope.workspace_path));
            if let Some(identity) = &scope.workspace_identity {
                input.insert("workspaceIdentity".into(), json!(identity));
            }
            input.insert(
                "recurring".into(),
                parsed.get("recurring").cloned().unwrap_or(json!(true)),
            );
            let automation = match service.create(&input).await {
                Ok(automation) => automation,
                Err(error) => return internal(format!("{error:#}")),
            };
            if let Some(task_id) = automation["targetTaskId"].as_str() {
                stamp_task(&index, &scope, task_id, &automation).await;
            }
            ReverseReply::Result(json!({"automation": to_protocol_automation(&automation)}))
        }
        "automation/list" => {
            if params.as_object().is_none_or(|object| !object.is_empty()) {
                return invalid("Invalid automation list params");
            }
            match service.list(Some(&scope)).await {
                Ok(list) => ReverseReply::Result(json!({
                    "automations": list.iter().map(to_protocol_automation).collect::<Vec<_>>(),
                })),
                Err(error) => internal(format!("{error:#}")),
            }
        }
        "automation/checkTaskBinding" => {
            let Some(parsed) = parse_id(&params, "targetTaskId") else {
                return invalid("Invalid automation task binding params");
            };
            let task_id = parsed["targetTaskId"].as_str().unwrap_or_default();
            match service.has_task_binding(&scope, task_id).await {
                Ok(bound) => ReverseReply::Result(json!({"bound": bound})),
                Err(error) => internal(format!("{error:#}")),
            }
        }
        "automation/update" => {
            let Some(parsed) = parse_update(&params) else {
                return invalid("Invalid automation update params");
            };
            let automation_id = parsed["automationId"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let mut patch = parsed;
            patch.remove("automationId");
            match service.update(&automation_id, &patch, Some(&scope)).await {
                Ok(Some(automation)) => {
                    ReverseReply::Result(json!({"automation": to_protocol_automation(&automation)}))
                }
                Ok(None) => internal("Scheduled task not found in the current workspace."),
                Err(error) => internal(format!("{error:#}")),
            }
        }
        "automation/delete" => {
            let Some(parsed) = parse_id(&params, "automationId") else {
                return invalid("Invalid automation delete params");
            };
            let automation_id = parsed["automationId"].as_str().unwrap_or_default();
            match service.delete(automation_id, Some(&scope)).await {
                Ok(deleted) => ReverseReply::Result(json!({"deleted": deleted})),
                Err(error) => internal(format!("{error:#}")),
            }
        }
        other => ReverseReply::Error {
            code: -32601,
            message: format!("Unsupported ZCode Protocol request: {other}"),
        },
    }
}

/// 会话内创建的 automation 复用当前会话：给 tasks-index 行盖 `cronAutomationId`（不广播）。
async fn stamp_task(index: &TasksIndex, scope: &Scope, task_id: &str, automation: &Value) {
    let Ok(Some(mut meta)) = index.get_task_meta(&scope.key(), task_id).await else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default();
    if let Some(object) = meta.as_object_mut() {
        object.insert(
            "cronAutomationId".into(),
            automation["automationId"].clone(),
        );
        let updated = object.get("updatedAt").and_then(Value::as_i64).unwrap_or(0);
        object.insert("updatedAt".into(), json!(updated.max(now)));
    }
    if let Err(error) = index.sync_task_meta(meta, None).await {
        log::warn(format!(
            "stamp cronAutomationId on {task_id} failed: {error:#}"
        ));
    }
}

pub fn register(agent: &AgentRuntime, service: Arc<AutomationService>, index: TasksIndex) {
    for method in [
        "automation/create",
        "automation/update",
        "automation/list",
        "automation/delete",
        "automation/checkTaskBinding",
    ] {
        let (service, index) = (service.clone(), index.clone());
        let handler: ReverseHandler =
            Arc::new(move |context| Box::pin(handle(context, service.clone(), index.clone())));
        agent.extensions().register_reverse(method, handler);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_update_params_follow_the_protocol_schema() {
        let create = parse_create(&json!({"cronExpr": " 0 9 * * * ", "prompt": "hi"})).unwrap();
        assert_eq!(create["cronExpr"], "0 9 * * *");
        assert!(parse_create(&json!({"cronExpr": "0 9 * * *", "prompt": " "})).is_none());
        assert!(parse_create(&json!({"cronExpr": "x", "prompt": "p", "extra": 1})).is_none());
        assert!(
            parse_create(&json!({"cronExpr": "x", "prompt": "p", "intervalUnit": "daily"}))
                .is_none()
        );
        assert!(parse_create(&json!({"cronExpr": "x", "prompt": "p", "maxRuns": null})).is_none());
        assert!(parse_update(&json!({"automationId": "a"})).is_none());
        assert!(parse_update(&json!({"automationId": "a", "maxRuns": null})).is_none());
        assert!(
            parse_update(&json!({"automationId": "a", "maxRuns": null, "recurring": true}))
                .is_some()
        );
        assert!(
            parse_update(&json!({"automationId": "a", "maxRuns": 3, "recurring": true})).is_none()
        );
        assert!(parse_id(&json!({"automationId": ""}), "automationId").is_none());
    }
}
