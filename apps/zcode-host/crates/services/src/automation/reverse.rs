//! CLI → Host 的 automation 反向请求（zcodeAgentService.ts 的 `automation/*` 分支）。
//!
//! workspace 一律取发请求的 CLI 进程所属 workspace，不进协议参数。参数按 zcode-protocol 的
//! strict schema 校验（`support::zod`，`nonEmptyString` 会 trim），失败回 -32602 并带 zod
//! flatten 的 `data`；业务错误回 -32603。
use std::sync::Arc;

use serde_json::{Map, Value, json};
use zcode_host_kit::log;

use super::service::{AutomationService, Scope};
use crate::agent::extensions::{ReverseContext, ReverseHandler, ReverseReply};
use crate::agent::{AgentRuntime, WorkspaceTarget};
use crate::support::zod::{self, NumCheck, Schema, field, optional};
use crate::tasks_index::TasksIndex;

const ROOT: &[&str] = &[];
const INTERVAL: &[&str] = &["interval"];
const INTERVAL_UNIT: &[&str] = &["intervalUnit"];
const RECURRING: &[&str] = &["recurring"];
const MAX_RUNS: &[&str] = &["maxRuns"];

const TASK_MODES: [&str; 6] = ["yolo", "plan", "edit", "auto", "autoEdit", "build"];
const INTERVAL_UNITS: [&str; 6] = ["minute", "hourly", "daily", "weekly", "monthly", "yearly"];

/// 校验失败时为 zod `error.flatten()`（JSON-RPC 错误的 `data`）。
pub type Validated = Result<Map<String, Value>, Value>;
type Parsed = Option<Map<String, Value>>;

fn int(checks: Vec<NumCheck>) -> Schema {
    Schema::Num { int: true, checks }
}

/// `modelSelectionSchema`（model-selection.ts）。
pub fn model_selection_schema() -> Schema {
    Schema::Object(vec![
        field("providerId", zod::non_empty_string()),
        field("modelId", zod::non_empty_string()),
        optional(
            "options",
            Schema::Object(vec![optional("reasoningLevel", zod::non_empty_string())]),
        ),
    ])
}

/// `zcodeAutomationBotDeliveryTargetSchema`（bots.ts）。
fn bot_delivery_target_schema() -> Schema {
    Schema::Object(vec![
        field("provider", Schema::Enum(&["feishu", "lark", "weixin"])),
        field("botId", zod::non_empty_string()),
        field("providerUserId", zod::non_empty_string()),
        field("chatType", Schema::Enum(&["private", "group"])),
    ])
}

fn run(schema: &Schema, params: &Value, refines: &[zod::Refine]) -> Validated {
    // ReverseContext 把缺省 params 记为 null；zod 对缺省参数报 `received undefined`。
    let value = (!params.is_null()).then_some(params);
    zod::parse(schema, value, refines).map_err(|issues| zod::flatten(&issues))
}

fn has(input: &Map<String, Value>, key: &str) -> bool {
    input.contains_key(key)
}

/// `zcodeAutomationCreateParamsSchema`。
pub fn validate_create(params: &Value) -> Validated {
    let schema = Schema::Object(vec![
        optional("title", zod::string()),
        field("cronExpr", zod::non_empty_string()),
        optional(
            "relativeDelayMinutes",
            int(vec![NumCheck::Gt(0.0), NumCheck::Lte(525_600.0)]),
        ),
        field("prompt", zod::non_empty_string()),
        optional("modelSelection", model_selection_schema()),
        optional("mode", Schema::Enum(&TASK_MODES)),
        optional("targetTaskId", zod::non_empty_string()),
        optional("botDeliveryTarget", bot_delivery_target_schema()),
        optional("recurring", Schema::Bool),
        optional("maxRuns", int(vec![NumCheck::Gt(0.0)])),
        optional("intervalUnit", Schema::Enum(&INTERVAL_UNITS)),
        optional(
            "interval",
            int(vec![NumCheck::Gte(1.0), NumCheck::Lte(200.0)]),
        ),
    ]);
    run(
        &schema,
        params,
        &[
            |input| {
                (has(input, "intervalUnit") != has(input, "interval"))
                    .then_some(("intervalUnit and interval must be set together", INTERVAL))
            },
            |input| {
                (has(input, "intervalUnit") && has(input, "relativeDelayMinutes")).then_some((
                    "intervalUnit cannot combine with a relative delayMinutes",
                    INTERVAL_UNIT,
                ))
            },
            |input| {
                (has(input, "intervalUnit") && input.get("recurring") == Some(&json!(false)))
                    .then_some((
                        "intervalUnit is a recurring carrier and cannot combine with recurring=false",
                        RECURRING,
                    ))
            },
            |input| {
                (has(input, "intervalUnit") && has(input, "maxRuns")).then_some((
                    "intervalUnit is a recurring carrier and cannot combine with maxRuns",
                    MAX_RUNS,
                ))
            },
        ],
    )
}

/// `zcodeAutomationUpdateParamsSchema`。
pub fn validate_update(params: &Value) -> Validated {
    let schema = Schema::Object(vec![
        field("automationId", zod::non_empty_string()),
        optional("title", zod::non_empty_string()),
        optional("cronExpr", zod::non_empty_string()),
        optional("prompt", zod::non_empty_string()),
        optional("recurring", Schema::Bool),
        optional(
            "maxRuns",
            Schema::Nullable(Box::new(int(vec![NumCheck::Gt(0.0)]))),
        ),
        optional("intervalUnit", Schema::Enum(&INTERVAL_UNITS)),
        optional(
            "interval",
            int(vec![NumCheck::Gte(1.0), NumCheck::Lte(200.0)]),
        ),
    ]);
    run(
        &schema,
        params,
        &[
            |input| {
                (![
                    "title",
                    "cronExpr",
                    "prompt",
                    "recurring",
                    "maxRuns",
                    "intervalUnit",
                ]
                .iter()
                .any(|key| has(input, key)))
                .then_some(("automation update requires at least one field", ROOT))
            },
            |input| {
                (input.get("maxRuns") == Some(&Value::Null)
                    && input.get("recurring") != Some(&json!(true)))
                .then_some(("clearing maxRuns requires recurring=true", MAX_RUNS))
            },
            |input| {
                (input.get("recurring") == Some(&json!(true))
                    && input.get("maxRuns").is_some_and(Value::is_number))
                .then_some((
                    "recurring=true cannot be combined with a numeric maxRuns",
                    MAX_RUNS,
                ))
            },
            |input| {
                (has(input, "intervalUnit") != has(input, "interval"))
                    .then_some(("intervalUnit and interval must be set together", INTERVAL))
            },
            |input| {
                (has(input, "intervalUnit") && input.get("recurring") == Some(&json!(false)))
                    .then_some((
                        "intervalUnit is a recurring carrier and cannot combine with recurring=false",
                        RECURRING,
                    ))
            },
            |input| {
                let max_runs = input.get("maxRuns");
                (has(input, "intervalUnit")
                    && max_runs.is_some()
                    && !(max_runs == Some(&Value::Null)
                        && input.get("recurring") == Some(&json!(true))))
                .then_some((
                    "intervalUnit is a recurring carrier and only allows maxRuns=null with recurring=true",
                    MAX_RUNS,
                ))
            },
        ],
    )
}

/// `zcodeAutomationListParamsSchema.safeParse(request.params ?? {})`。
pub fn validate_list(params: &Value) -> Validated {
    let params = if params.is_null() { &json!({}) } else { params };
    run(&Schema::Object(Vec::new()), params, &[])
}

/// `zcodeAutomationCheckTaskBindingParamsSchema` / `zcodeAutomationDeleteParamsSchema`。
pub fn validate_id(params: &Value, key: &'static str) -> Validated {
    run(
        &Schema::Object(vec![field(key, zod::non_empty_string())]),
        params,
        &[],
    )
}

pub fn parse_create(params: &Value) -> Parsed {
    validate_create(params).ok()
}

pub fn parse_update(params: &Value) -> Parsed {
    validate_update(params).ok()
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

/// 参数校验失败：`-32602` + 固定文案 + zod flatten（`client.respondError({code, message, data})`）。
fn invalid(message: &str, data: Value) -> ReverseReply {
    ReverseReply::ErrorWithData {
        code: -32602,
        message: message.to_owned(),
        data,
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
            let parsed = match validate_create(&params) {
                Ok(parsed) => parsed,
                Err(data) => return invalid("Invalid automation create params", data),
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
                Err(error) => return internal(error.to_string()),
            };
            if let Some(task_id) = automation["targetTaskId"].as_str() {
                stamp_task(&index, &scope, task_id, &automation).await;
            }
            ReverseReply::Result(json!({"automation": to_protocol_automation(&automation)}))
        }
        "automation/list" => {
            if let Err(data) = validate_list(&params) {
                return invalid("Invalid automation list params", data);
            }
            match service.list(Some(&scope)).await {
                Ok(list) => ReverseReply::Result(json!({
                    "automations": list.iter().map(to_protocol_automation).collect::<Vec<_>>(),
                })),
                Err(error) => internal(error.to_string()),
            }
        }
        "automation/checkTaskBinding" => {
            let parsed = match validate_id(&params, "targetTaskId") {
                Ok(parsed) => parsed,
                Err(data) => return invalid("Invalid automation task binding params", data),
            };
            let task_id = parsed["targetTaskId"].as_str().unwrap_or_default();
            match service.has_task_binding(&scope, task_id).await {
                Ok(bound) => ReverseReply::Result(json!({"bound": bound})),
                Err(error) => internal(error.to_string()),
            }
        }
        "automation/update" => {
            let parsed = match validate_update(&params) {
                Ok(parsed) => parsed,
                Err(data) => return invalid("Invalid automation update params", data),
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
                Err(error) => internal(error.to_string()),
            }
        }
        "automation/delete" => {
            let parsed = match validate_id(&params, "automationId") {
                Ok(parsed) => parsed,
                Err(data) => return invalid("Invalid automation delete params", data),
            };
            let automation_id = parsed["automationId"].as_str().unwrap_or_default();
            match service.delete(automation_id, Some(&scope)).await {
                Ok(deleted) => ReverseReply::Result(json!({"deleted": deleted})),
                Err(error) => internal(error.to_string()),
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
        assert!(validate_id(&json!({"automationId": ""}), "automationId").is_err());
    }
}
