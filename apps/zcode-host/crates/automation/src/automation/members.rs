//! `zcode-agent` 的 automation 成员（zcodeAgentService.ts 的 listAutomations … deleteAutomationRun）。
//! 这些成员不依赖连接 scope，经 `agent/extensions.rs` 的 `register_member` 登记，
//! `AgentChannel` 的 dispatch 只做委托。
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value};
use zcode_host_kit::{Args, Reply, RpcError};

use super::dispatch::Dispatcher;
use super::service::{AutomationService, Scope};
use crate::agent::AgentRuntime;
use crate::agent::extensions::MemberHandler;

pub const MEMBERS: [&str; 10] = [
    "listAutomations",
    "listAllAutomations",
    "createAutomation",
    "updateAutomation",
    "deleteAutomation",
    "setAutomationEnabled",
    "restartAutomation",
    "runAutomationNow",
    "listAutomationRuns",
    "deleteAutomationRun",
];

fn error(message: impl Into<String>) -> RpcError {
    RpcError::new("Error", message)
}

fn anyhow_error(error: anyhow::Error) -> RpcError {
    RpcError::new("Error", error.to_string())
}

fn params(args: &Args) -> Map<String, Value> {
    args.raw(0)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn copy(from: &Map<String, Value>, keys: &[&str]) -> Map<String, Value> {
    keys.iter()
        .filter_map(|key| {
            from.get(*key)
                .map(|value| ((*key).to_owned(), value.clone()))
        })
        .collect()
}

fn id(params: &Map<String, Value>, key: &str) -> String {
    params
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn scope(params: &Map<String, Value>) -> Option<Scope> {
    Scope::from_params(&Value::Object(params.clone()))
}

async fn call(
    name: &'static str,
    service: Arc<AutomationService>,
    dispatcher: Arc<Dispatcher>,
    args: Args,
) -> Result<Reply, RpcError> {
    let params = params(&args);
    let scope = scope(&params);
    let scope = scope.as_ref();
    let value = |value: Value| Ok(Reply::Value(value));
    match name {
        "listAutomations" => value(Value::Array(
            service.list(scope).await.map_err(anyhow_error)?,
        )),
        "listAllAutomations" => value(Value::Array(
            service.list(None).await.map_err(anyhow_error)?,
        )),
        "createAutomation" => {
            let mut input = copy(
                &params,
                &[
                    "title",
                    "cronExpr",
                    "relativeDelayMinutes",
                    "prompt",
                    "modelSelection",
                    "mode",
                    "workspacePath",
                    "workspaceIdentity",
                    "maxRuns",
                    "endAt",
                    "scheduleRule",
                ],
            );
            let recurring = params
                .get("recurring")
                .filter(|recurring| !recurring.is_null())
                .cloned()
                .unwrap_or(Value::Bool(true));
            input.insert("recurring".into(), recurring);
            value(service.create(&input).await.map_err(anyhow_error)?)
        }
        "updateAutomation" => {
            let patch = copy(
                &params,
                &[
                    "title",
                    "cronExpr",
                    "prompt",
                    "modelSelection",
                    "mode",
                    "recurring",
                    "maxRuns",
                    "endAt",
                    "scheduleRule",
                    "scheduleEditedByUser",
                ],
            );
            let updated = service
                .update(&id(&params, "automationId"), &patch, scope)
                .await
                .map_err(anyhow_error)?;
            value(updated.unwrap_or(Value::Null))
        }
        "deleteAutomation" => {
            service
                .delete(&id(&params, "automationId"), scope)
                .await
                .map_err(anyhow_error)?;
            Ok(Reply::Undefined)
        }
        "setAutomationEnabled" => {
            let enabled = params
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            service
                .set_enabled(&id(&params, "automationId"), enabled, scope)
                .await
                .map_err(anyhow_error)?;
            Ok(Reply::Undefined)
        }
        "restartAutomation" => {
            service
                .restart(&id(&params, "automationId"), scope)
                .await
                .map_err(anyhow_error)?;
            Ok(Reply::Undefined)
        }
        "runAutomationNow" => {
            let scope = scope.cloned().unwrap_or(Scope {
                workspace_path: String::new(),
                workspace_identity: None,
            });
            let result = dispatcher
                .run_now(&id(&params, "automationId"), &scope)
                .await
                .map_err(error)?;
            value(result)
        }
        "listAutomationRuns" => value(Value::Array(
            service
                .list_runs(&id(&params, "automationId"), scope)
                .await
                .map_err(anyhow_error)?,
        )),
        "deleteAutomationRun" => {
            service
                .delete_run(&id(&params, "runId"), scope)
                .await
                .map_err(anyhow_error)?;
            Ok(Reply::Undefined)
        }
        other => Err(error(format!("Method not found: {other}"))),
    }
}

/// 登记 10 个成员。
pub fn register(
    agent: &AgentRuntime,
    service: Arc<AutomationService>,
    dispatcher: Arc<Dispatcher>,
) {
    for name in MEMBERS {
        let (service, dispatcher) = (service.clone(), dispatcher.clone());
        let handler: MemberHandler = Arc::new(
            move |args: Args| -> BoxFuture<'static, Result<Reply, RpcError>> {
                Box::pin(call(name, service.clone(), dispatcher.clone(), args))
            },
        );
        agent.extensions().register_member(name, handler);
    }
}
