//! conversation-share（docs/specs/rust-host-automation.md 第 8 节）。
//!
//! 按连接的 clientMode 门禁（conversationShareAttachmentService.ts）：desktop-continuous 全部可用；
//! web-remote-replayable 只读能力发现与预览，其余写操作以 `feature_disabled` 拒绝，导入副本读回 null。
//!
//! Rust Host 还没有账号（R4），token provider 恒为空：`getCapabilities`、`preflight`、`publish`
//! 在发请求前就以 `authentication_required` 失败（与 Node 未登录时相同）；预览、续接与导入走
//! 可选鉴权，照常可用。
// 分享错误携带脱敏后的 issue 列表与诊断字段，只在失败路径构造；不为减小 Err 体积而装箱。
#![allow(clippy::result_large_err)]

pub mod client;
pub mod errors;
pub mod import;
pub mod integrity;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use futures_util::StreamExt;
use serde_json::Value;
use zcode_host_control::{ClientMode, ControlRouter};
use zcode_host_kit::{
    Args, CallContext, ChannelRegistry, Reply, RpcError, ServiceBuilder, log, replies,
};

use crate::automation::{Area, Keepalive};
use client::{ShareClient, TokenProvider};
use errors::{ServiceError, ShareError};
use import::{ImportInput, Importer};

pub struct ShareService {
    client: Arc<ShareClient>,
    importer: Arc<Importer>,
    publish_progress: zcode_host_kit::EventHub<Value>,
}

fn desktop_only() -> RpcError {
    RpcError::new(
        "Error",
        "Conversation sharing is only available from Desktop",
    )
    .with_field("kind", "feature_disabled")
}

fn is_remote(context: &CallContext) -> bool {
    context.client_mode == ClientMode::WebRemoteReplayable
}

fn share_error(error: ShareError) -> RpcError {
    error.to_rpc()
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value[key].as_str().map(str::to_owned)
}

impl ShareService {
    /// 能力发现是 preflight / publish 的第一步（需要登录）。之后的选择、公开投影、结果物上传与
    /// confirm 轮询要等账号接入（R4）才可能执行，目前不可达。
    async fn authenticated_gate(&self) -> Result<Value, RpcError> {
        let capabilities = self
            .client
            .capabilities()
            .await
            .map_err(|error| error.to_rpc())?;
        log::warn("conversation share publishing pipeline is not available in the Rust Host yet");
        let _ = capabilities;
        Err(ServiceError::new(
            "feature_disabled",
            "Conversation share publishing is not available in this build",
        )
        .to_rpc())
    }
}

pub fn channel(service: Arc<ShareService>) -> ServiceBuilder {
    let s = move || service.clone();
    let (a, b, c, d, e, f, g, h, i) = (s(), s(), s(), s(), s(), s(), s(), s(), s());
    ServiceBuilder::new("conversation-share")
        .method("getCapabilities", move |_, _| {
            let service = a.clone();
            async move {
                service
                    .client
                    .capabilities()
                    .await
                    .map(Reply::Value)
                    .map_err(|error| error.to_rpc())
            }
        })
        .method("preflight", move |context, _| {
            let service = b.clone();
            async move {
                if is_remote(&context) {
                    return Err(desktop_only());
                }
                service.authenticated_gate().await.map(Reply::Value)
            }
        })
        .method("publish", move |context, _| {
            let service = c.clone();
            async move {
                if is_remote(&context) {
                    return Err(desktop_only());
                }
                service.authenticated_gate().await.map(Reply::Value)
            }
        })
        .method("importShare", move |context, args: Args| {
            let service = d.clone();
            async move {
                if is_remote(&context) {
                    return Err(desktop_only());
                }
                let input = args.raw(0).cloned().unwrap_or(Value::Null);
                let operation_id = args.opt::<String>(1, "operationId")?.unwrap_or_default();
                let input = ImportInput {
                    share_code: string_field(&input, "shareCode").unwrap_or_default(),
                    client_request_id: string_field(&input, "clientRequestId").unwrap_or_default(),
                    target_workspace_path: string_field(&input, "targetWorkspacePath"),
                    target_workspace_identity: string_field(&input, "targetWorkspaceIdentity"),
                    target_workspace_kind: string_field(&input, "targetWorkspaceKind"),
                    locale: string_field(&input, "locale"),
                };
                service
                    .importer
                    .import(input, operation_id)
                    .await
                    .map(Reply::Value)
                    .map_err(share_error)
            }
        })
        .method("getImportedConversation", move |context, args| {
            let service = e.clone();
            async move {
                if is_remote(&context) {
                    return Ok(Reply::null());
                }
                let input = args.raw(0).cloned().unwrap_or(Value::Null);
                let path = input["workspacePath"].as_str().unwrap_or_default();
                let context_id = input["contextId"].as_str().unwrap_or_default();
                Ok(Reply::Value(
                    service
                        .importer
                        .imported_conversation(path, context_id)
                        .await,
                ))
            }
        })
        .method("getPreview", move |_, args| {
            let service = f.clone();
            async move {
                let code = args.get::<String>(0, "shareCode")?;
                service
                    .client
                    .preview(&code)
                    .await
                    .map(Reply::Value)
                    .map_err(|error| error.to_rpc())
            }
        })
        .method("getContinuation", move |context, args| {
            let service = g.clone();
            async move {
                if is_remote(&context) {
                    return Err(desktop_only());
                }
                let input = args.raw(0).cloned().unwrap_or(Value::Null);
                service
                    .client
                    .continuation(
                        input["shareCode"].as_str().unwrap_or_default(),
                        input["clientRequestId"].as_str().unwrap_or_default(),
                    )
                    .await
                    .map(Reply::Value)
                    .map_err(|error| error.to_rpc())
            }
        })
        .event("onDynamicPublishProgress", move |context, arg| {
            if is_remote(context) {
                return Ok(Box::pin(futures_util::stream::pending()));
            }
            let operation = arg.as_str().unwrap_or_default().to_owned();
            let events = h.publish_progress.subscribe_filtered(move |event: &Value| {
                event["operationId"] == operation.as_str()
            });
            Ok(replies(events))
        })
        .event("onDynamicImportProgress", move |context, arg| {
            if is_remote(context) {
                return Ok(Box::pin(futures_util::stream::pending()));
            }
            let operation = arg.as_str().unwrap_or_default().to_owned();
            let events = i
                .importer
                .progress()
                .subscribe_filtered(move |event: &Value| event["operationId"] == operation.as_str())
                .boxed();
            Ok(replies(events))
        })
}

pub fn install(
    area: &Arc<Area>,
    registry: &mut ChannelRegistry,
    _router: &mut ControlRouter,
) -> Option<Keepalive> {
    let vars = &area.context.environment.vars;
    let origin = match crate::support::http::zcode_api_origin(vars) {
        Ok(origin) => origin,
        Err(error) => {
            log::error(format!("conversation share endpoint is invalid: {error}"));
            return None;
        }
    };
    // 账号（R4）接入前没有 zcode JWT。
    let token: TokenProvider = Arc::new(|| Box::pin(async { None }));
    let client = match ShareClient::new(format!("{origin}/api/v1"), token) {
        Ok(client) => Arc::new(client),
        Err(error) => {
            log::error(format!("conversation share client unavailable: {error:#}"));
            return None;
        }
    };
    let share_web_url = vars
        .get("ZCODE_CONVERSATION_SHARE_WEB_URL")
        .cloned()
        .unwrap_or_else(|| format!("{origin}/cn/share"));
    let conversation_root = area
        .context
        .environment
        .data_base_dir
        .join(".zcode")
        .join("workspace")
        .join("default");
    let importer = Importer::new(
        area.agent.clone(),
        Some(area.tasks.clone()),
        client.clone(),
        conversation_root,
        share_web_url,
    );
    let cleanup = importer.clone();
    tokio::spawn(async move { cleanup.cleanup_abandoned().await });
    let service = Arc::new(ShareService {
        client,
        importer,
        publish_progress: zcode_host_kit::EventHub::new(),
    });
    registry.implement(channel(service.clone()));
    Some(Box::new(service))
}
