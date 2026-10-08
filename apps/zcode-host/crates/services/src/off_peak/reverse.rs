//! CLI → Host 的 `offPeak/create`、`offPeak/list`（zcodeAgentService.ts）。workspace 取发请求的 CLI
//! 进程所属 workspace；业务失败按分类回 `{ok:false,…}`，兜底异常只回固定的内部错误文案。
use std::sync::Arc;

use serde_json::{Map, Value, json};
use zcode_host_kit::{ServiceChannel, log};

use super::GrayOverlay;
use super::models::{complete_new_selection, provider_for_family};
use super::service::OffPeakService;
use crate::agent::extensions::{ReverseContext, ReverseHandler, ReverseReply};
use crate::automation::Area;

const PERMISSION_MODES: [&str; 4] = ["build", "edit", "plan", "yolo"];
const INTERNAL_ERROR_MESSAGE: &str = "Internal off-peak service error";

/// `zcodeOffPeakCreateParamsSchema`（strict，nonEmptyString 会 trim）。
fn parse_create(params: &Value) -> Option<Map<String, Value>> {
    let input = params.as_object()?;
    let allowed = [
        "title",
        "prompt",
        "permissionMode",
        "model",
        "thoughtLevel",
        "boundSessionId",
    ];
    if input.keys().any(|key| !allowed.contains(&key.as_str())) {
        return None;
    }
    let mut out = Map::new();
    for key in allowed {
        let Some(value) = input.get(key) else {
            if matches!(key, "title" | "prompt") {
                return None;
            }
            continue;
        };
        let text = value.as_str()?.trim();
        if text.is_empty() {
            return None;
        }
        if key == "permissionMode" && !PERMISSION_MODES.contains(&text) {
            return None;
        }
        out.insert(key.into(), json!(text));
    }
    Some(out)
}

/// `toProtocolOffPeakTaskSnapshot`：协议最小面。
fn snapshot(task: &Value) -> Value {
    let mut out = Map::new();
    out.insert("offPeakTaskId".into(), task["offPeakTaskId"].clone());
    out.insert("title".into(), task["title"].clone());
    out.insert("status".into(), task["status"].clone());
    if let Some(position) = task["queuePosition"]
        .as_i64()
        .filter(|position| *position > 0)
    {
        out.insert("queuePosition".into(), json!(position));
    }
    if let Some(session) = task["sessionId"].as_str().filter(|id| !id.is_empty()) {
        out.insert("sessionId".into(), json!(session));
    }
    out.insert("createdAt".into(), task["createdAt"].clone());
    Value::Object(out)
}

fn internal(method: &str, key: &str, error: impl std::fmt::Display) -> ReverseReply {
    log::warn(format!(
        "Off-peak 协议请求处理失败 method={method} workspaceKey={key}: {error}"
    ));
    ReverseReply::Error {
        code: -32603,
        message: INTERNAL_ERROR_MESSAGE.into(),
    }
}

fn rejected(code: &str) -> ReverseReply {
    ReverseReply::Result(json!({
        "ok": false,
        "failureStage": "client_validation",
        "errorCategory": "client_validation",
        "errorCode": code,
    }))
}

/// 灰度有效开启时的白名单（可按 Provider 过滤）。
fn allowed_models(gray: &Value, provider_id: Option<&str>) -> Vec<String> {
    if gray["enabled"] != true {
        return Vec::new();
    }
    gray["modelSelectionView"]["providers"]
        .as_array()
        .map(|providers| {
            providers
                .iter()
                .filter(|provider| provider_id.is_none_or(|id| provider["providerId"] == id))
                .flat_map(|provider| {
                    provider["models"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|model| model["modelId"].as_str().map(str::to_owned))
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn create(
    context: &ReverseContext,
    area: &Arc<Area>,
    service: &Arc<OffPeakService>,
    gray_source: &Arc<dyn ServiceChannel>,
) -> ReverseReply {
    let Some(parsed) = parse_create(&context.params) else {
        return ReverseReply::Error {
            code: -32602,
            message: "Invalid off-peak create params".into(),
        };
    };
    let gray = GrayOverlay::client_config(area, gray_source)
        .await
        .unwrap_or(Value::Null);
    let support = if allowed_models(&gray, None).is_empty() {
        Value::Null
    } else {
        service.coding_plan_support()
    };
    let provider_id = (support["supported"] == true)
        .then(|| provider_for_family(support["providerFamily"].as_str().unwrap_or_default()));
    let allowed = provider_id
        .map(|id| allowed_models(&gray, Some(id)))
        .unwrap_or_default();
    if allowed.is_empty() {
        return rejected("offpeak_disabled");
    }
    // 省略 → 白名单末位；显式 → trim + 大小写不敏感匹配，回写白名单原写法。
    let model = match parsed.get("model").and_then(Value::as_str) {
        None => allowed.last().cloned(),
        Some(wanted) => allowed
            .iter()
            .find(|model| model.trim().eq_ignore_ascii_case(wanted))
            .cloned(),
    };
    let Some(model) = model else {
        return rejected("model_not_allowed");
    };
    let provider_id = provider_id.unwrap_or_default();
    let selection = complete_new_selection(&gray["modelSelectionView"], provider_id, &model).map(
        |mut selection| {
            if let Some(level) = parsed.get("thoughtLevel") {
                selection["options"] = json!({"reasoningLevel": level});
            }
            selection
        },
    );
    let Some(selection) = selection else {
        return rejected("model_not_allowed");
    };
    let mut params = Map::new();
    params.insert("title".into(), parsed["title"].clone());
    params.insert("prompt".into(), parsed["prompt"].clone());
    params.insert(
        "permissionMode".into(),
        parsed
            .get("permissionMode")
            .cloned()
            .unwrap_or_else(|| json!("yolo")),
    );
    params.insert("modelSelection".into(), selection);
    if let Some(bound) = parsed.get("boundSessionId") {
        params.insert("boundSessionId".into(), bound.clone());
    }
    params.insert("workspacePath".into(), json!(context.target.workspace_path));
    if let Some(identity) = context
        .target
        .workspace_identity
        .as_deref()
        .filter(|identity| !identity.is_empty())
    {
        params.insert("workspaceIdentity".into(), json!(identity));
    }
    match service.create_task(&Value::Object(params)).await {
        Ok(result) if result["ok"] == true => {
            ReverseReply::Result(json!({"ok": true, "task": snapshot(&result["task"])}))
        }
        Ok(result) => ReverseReply::Result(json!({
            "ok": false,
            "failureStage": result["failureStage"],
            "errorCategory": result["errorCategory"],
            "errorCode": result["errorCode"],
        })),
        Err(error) => internal(&context.method, &context.workspace_key, error),
    }
}

async fn list(context: &ReverseContext, service: &Arc<OffPeakService>) -> ReverseReply {
    let params = if context.params.is_null() {
        json!({})
    } else {
        context.params.clone()
    };
    if params.as_object().is_none_or(|object| !object.is_empty()) {
        return ReverseReply::Error {
            code: -32602,
            message: "Invalid off-peak list params".into(),
        };
    }
    match service.list().await {
        Ok(tasks) => {
            let mut tasks: Vec<Value> = tasks
                .into_iter()
                .filter(|task| task["workspaceKey"] == context.workspace_key.as_str())
                .collect();
            tasks.sort_by_key(|task| std::cmp::Reverse(task["createdAt"].as_i64().unwrap_or(0)));
            tasks.truncate(20);
            ReverseReply::Result(json!({"tasks": tasks.iter().map(snapshot).collect::<Vec<_>>()}))
        }
        Err(error) => internal(
            &context.method,
            &context.workspace_key,
            format!("{error:#}"),
        ),
    }
}

pub fn register(
    area: &Arc<Area>,
    service: Arc<OffPeakService>,
    gray_source: Arc<dyn ServiceChannel>,
) {
    let (create_area, create_service, create_gray) = (area.clone(), service.clone(), gray_source);
    let create_handler: ReverseHandler = Arc::new(move |context| {
        let (area, service, gray) = (
            create_area.clone(),
            create_service.clone(),
            create_gray.clone(),
        );
        Box::pin(async move { create(&context, &area, &service, &gray).await })
    });
    area.agent
        .extensions()
        .register_reverse("offPeak/create", create_handler);
    let list_handler: ReverseHandler = Arc::new(move |context| {
        let service = service.clone();
        Box::pin(async move { list(&context, &service).await })
    });
    area.agent
        .extensions()
        .register_reverse("offPeak/list", list_handler);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_params_follow_the_protocol_schema() {
        let parsed = parse_create(&json!({"title": " t ", "prompt": "p"})).unwrap();
        assert_eq!(parsed["title"], "t");
        assert!(parse_create(&json!({"title": "t"})).is_none());
        assert!(
            parse_create(&json!({"title": "t", "prompt": "p", "permissionMode": "auto"})).is_none()
        );
        assert!(parse_create(&json!({"title": "t", "prompt": "p", "extra": 1})).is_none());
        let gray = json!({"enabled": true, "modelSelectionView": {"providers": [
            {"providerId": "a", "models": [{"modelId": "m1"}, {"modelId": "m2"}]},
            {"providerId": "b", "models": [{"modelId": "x"}]},
        ]}});
        assert_eq!(allowed_models(&gray, Some("a")), ["m1", "m2"]);
        assert_eq!(allowed_models(&gray, None).len(), 3);
        assert!(allowed_models(&json!({"enabled": false}), None).is_empty());
    }
}
