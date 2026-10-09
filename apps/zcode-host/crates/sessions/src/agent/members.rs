//! `zcode-agent` 非连接作用域成员与 `plugin-management` channel 的注册
//! （docs/specs/rust-host-agent.md §2.2、§2.13）。连接作用域成员（握手、订阅所有权）在
//! `service.rs` 的 `AgentChannel`，它把其余成员回退到这里。
use std::future::Future;

use futures_util::stream;
use serde_json::Value;
use zcode_host_control::ClientMode;
use zcode_host_kit::{Args, Reply, RpcError, ServiceBuilder};

use super::api::{AgentApi, fault};
use super::api_plugins::{PLUGIN_METHODS, WORKFLOW_METHODS, WORKFLOW_RUN_QUERIES};
use super::runtime::WorkspaceTarget;
use super::session;

pub const PLUGIN_MANAGEMENT_CHANNEL: &str = "plugin-management";

fn params(args: &Args) -> Value {
    args.raw(0).cloned().unwrap_or(Value::Null)
}

/// 注册一个“单对象参数 → JSON 结果”的方法。
fn method<F, Fut>(builder: ServiceBuilder, api: &AgentApi, name: &str, handler: F) -> ServiceBuilder
where
    F: Fn(AgentApi, Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value, RpcError>> + Send + 'static,
{
    let api = api.clone();
    let handler = std::sync::Arc::new(handler);
    builder.method(name, move |_, args| {
        let api = api.clone();
        let handler = handler.clone();
        async move { handler(api, params(&args)).await.map(Reply::Value) }
    })
}

fn pending_event() -> Result<zcode_host_kit::EventStream, RpcError> {
    Ok(Box::pin(stream::pending()))
}

/// 插件管理成员（两个 channel 共用）。
fn plugin_members(mut builder: ServiceBuilder, api: &AgentApi) -> ServiceBuilder {
    for spec in PLUGIN_METHODS {
        builder = method(builder, api, spec.member, move |api, params| async move {
            api.plugin_call(spec, &params).await
        });
    }
    builder = method(
        builder,
        api,
        "getPluginReferenceCatalog",
        |api, params| async move { api.plugin_reference_catalog(&params).await },
    );
    let events = api.events.clone();
    builder.event("onDynamicPluginOperationProgress", move |_, arg| {
        let operation_id = arg
            .as_str()
            .map(str::to_owned)
            .or_else(|| arg["operationId"].as_str().map(str::to_owned))
            .ok_or_else(|| fault("invalid argument operationId: is required"))?;
        Ok(events.plugin_progress(operation_id))
    })
}

pub fn plugin_management_service(api: &AgentApi) -> ServiceBuilder {
    plugin_members(ServiceBuilder::new(PLUGIN_MANAGEMENT_CHANNEL), api)
}

pub fn agent_service(api: &AgentApi) -> ServiceBuilder {
    let mut builder = plugin_members(ServiceBuilder::new(super::service::CHANNEL), api);

    // 存储启动
    builder = method(builder, api, "prepareStorage", |api, params| async move {
        let target = WorkspaceTarget::from_value(&params)?;
        api.prepare_storage(&target).await.map(|()| Value::Null)
    });
    builder = method(
        builder,
        api,
        "getStorageStartupState",
        |api, params| async move {
            let target = WorkspaceTarget::from_value(&params)?;
            Ok(api.chat.storage_state(&target).await.unwrap_or(Value::Null))
        },
    );
    let chat = api.chat.clone();
    builder = builder.event("onDynamicStorageStartupState", move |_, arg| {
        let key = WorkspaceTarget::from_value(&arg)?.key();
        let events = chat
            .storage_events()
            .subscribe_filtered(move |event| event["workspaceKey"] == key.as_str());
        Ok(Box::pin(futures_util::StreamExt::map(events, |event| {
            Reply::Value(event["snapshot"].clone())
        })))
    });
    builder = method(
        builder,
        api,
        "getWorkspaceRuntimeIdentity",
        |api, params| async move {
            let target = WorkspaceTarget::from_value(&params)?;
            api.runtime_identity(&target).await
        },
    );
    builder = method(
        builder,
        api,
        "readWorkspacePresentation",
        |api, params| async move {
            let target = WorkspaceTarget::from_value(&params)?;
            session::read_workspace_presentation(&api.chat, &target).await
        },
    );

    // legacy 会话
    builder = method(builder, api, "createSession", |api, p| async move {
        api.create_session(&p).await
    });
    builder = method(builder, api, "resumeSession", |api, p| async move {
        api.resume_session(&p).await
    });
    builder = method(builder, api, "listSessions", |api, p| async move {
        api.list_sessions(&p).await
    });
    builder = method(builder, api, "listSessionSubagents", |api, p| async move {
        api.list_session_subagents(&p).await
    });
    builder = method(builder, api, "readSession", |api, p| async move {
        api.read_session(&p).await
    });
    builder = method(builder, api, "readSessionMessages", |api, p| async move {
        api.read_session_messages(&p).await
    });
    builder = method(builder, api, "readSessionEvents", |api, p| async move {
        api.read_session_events(&p).await
    });
    builder = method(builder, api, "readSessionDebug", |api, p| async move {
        api.read_session_debug(&p).await
    });
    builder = method(
        builder,
        api,
        "grantWorkspaceHookTrust",
        |api, p| async move { api.grant_workspace_hook_trust(&p).await },
    );
    builder = method(builder, api, "sendPrompt", |api, p| async move {
        api.send_prompt(&p).await
    });
    builder = method(builder, api, "compactSession", |api, p| async move {
        api.compact_session(&p).await
    });
    builder = method(builder, api, "goalSession", |api, p| async move {
        api.goal_session(&p).await
    });
    builder = method(builder, api, "closeSession", |api, p| async move {
        api.close_session(&p).await.map(Value::Bool)
    });
    builder = method(builder, api, "setModel", |api, p| async move {
        api.set_model(&p).await
    });
    builder = method(builder, api, "setThoughtLevel", |api, p| async move {
        api.set_thought_level(&p).await
    });
    builder = method(builder, api, "setMode", |api, p| async move {
        api.set_mode(&p).await
    });
    // 本地 Host 的偏好权威是 local：反向请求直接作答，永远没有待转发的请求。
    builder = method(
        builder,
        api,
        "respondSessionRuntimePreferences",
        |_, p| async move {
            let request_id = p["requestId"].as_str().unwrap_or_default();
            Err(fault(format!(
                "ZCode session runtime preferences request not found: {request_id}"
            )))
        },
    );
    builder = builder.event("onDynamicSessionRuntimePreferencesRequest", |_, _| {
        pending_event()
    });
    let session_events = api.session_events.clone();
    builder = builder.event("onDynamicSessionEvent", move |_, arg| {
        session_events.listen(arg)
    });

    // MCP、技能、工作流
    builder = method(builder, api, "listMcpServerStatuses", |api, p| async move {
        api.list_mcp_server_statuses(&p).await
    });
    builder = method(
        builder,
        api,
        "getSkillReferenceCatalog",
        |api, p| async move { api.skill_reference_catalog(&p).await },
    );
    for (member, cli_method, fields) in WORKFLOW_METHODS {
        builder = method(builder, api, member, move |api, p| async move {
            api.workflow_call(cli_method, &p, fields).await
        });
    }
    builder = method(builder, api, "moveSavedWorkflow", |api, p| async move {
        api.move_saved_workflow(&p).await
    });
    for (member, cli_method, fields) in WORKFLOW_RUN_QUERIES {
        builder = method(builder, api, member, move |api, p| async move {
            let target = WorkspaceTarget::from_value(&p)?;
            let process = api.read_client(&target).await?;
            let mut request = serde_json::Map::new();
            super::api::copy_defined(&mut request, &p, fields);
            process
                .request_value(cli_method, &Value::Object(request))
                .await
        });
    }

    // 生成文本、连通性、用量、资源
    builder = method(builder, api, "generateWorkspaceText", |api, p| async move {
        api.generate_workspace_text(&p).await
    });
    builder = method(builder, api, "testModelConnectivity", |api, p| async move {
        api.test_model_connectivity(&p).await
    });
    builder = method(builder, api, "getAppUsageStats", |api, p| async move {
        api.app_usage_stats(&p).await
    });
    builder = method(builder, api, "getTaskTokenUsage", |api, p| async move {
        api.task_token_usage(&p).await
    });
    builder = method(
        builder,
        api,
        "collectLocalRuntimeChildProcesses",
        |api, _| async move { Ok(api.collect_child_processes().await) },
    );
    builder = method(
        builder,
        api,
        "hasActiveCuaOperationTurn",
        |_, _| async move { Ok(Value::Bool(false)) },
    );
    builder = method(builder, api, "disposeWorkspace", |api, p| async move {
        let target = WorkspaceTarget::from_value(&p)?;
        api.chat.dispose_workspace(&target).await;
        Ok(Value::Null)
    });
    builder = method(builder, api, "disposeAll", |api, _| async move {
        api.chat.dispose_all().await;
        Ok(Value::Null)
    });

    // 通知事件（connection scope 规则，docs §2.9）：Rust Host 的连接都是 terminal-client。
    let events = api.events.clone();
    builder = builder.event("onDynamicLocalTtftFacts", move |context, arg| {
        let target = WorkspaceTarget::from_value(&arg)?;
        let local = target.remote_session_id.is_none()
            && target
                .workspace_identity
                .as_deref()
                .is_none_or(|identity| identity.trim().is_empty());
        if context.client_mode != ClientMode::DesktopContinuous || !local {
            return pending_event();
        }
        Ok(events.local_ttft(target.key()))
    });
    let events = api.events.clone();
    builder = builder.event("onDynamicConversationTelemetryFact", move |context, arg| {
        // 不发协议请求，握手前也允许可信 desktop 提前监听。
        let target = WorkspaceTarget::from_value(&arg)?;
        if context.client_mode != ClientMode::DesktopContinuous {
            return pending_event();
        }
        Ok(events.telemetry(target.key()))
    });
    let events = api.events.clone();
    builder = builder.event("onDynamicCuaPermissionObservation", move |context, _| {
        if context.client_mode != ClientMode::DesktopContinuous {
            return pending_event();
        }
        Ok(events.cua_observations())
    });
    // 资源与 MCP 遥测只给 trusted host relay；Renderer 的订阅是永不触发的空流。
    for event in [
        "onDynamicProcessResourceSample",
        "onDynamicMcpTelemetry",
        "onDynamicMcpResourceSamples",
        "onDynamicToolExecResource",
    ] {
        builder = builder.event(event, |_, _| pending_event());
    }
    builder
}
