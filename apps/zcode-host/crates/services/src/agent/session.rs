//! `zcode-session` channel（packages/services/src/zcode-session/zcodeSessionService.ts）。
//!
//! 核心链路只需要两个成员：
//! - `readWorkspacePresentation`：每次发送前的门槛（composer 在它成功前不允许提交），透传 CLI
//!   `workspace/readPresentation`；CLI 声明 `workspaceExecutionCapabilities` 时请求附带执行能力。
//! - `initializeWorkspace`：Host 启动预热与 Renderer 预热共用，等同 `zcode-agent.initialize`。
//!
//! 其余成员属于 legacy 会话接口，V4 界面不调用，继续由契约 stub 应答。
use serde_json::{Map, Value, json};
use zcode_host_kit::{Reply, RpcError, ServiceBuilder, log};

use super::extensions::UseReason;
use super::runtime::{AgentRuntime, RuntimePolicy, WorkspaceTarget};

pub const CHANNEL: &str = "zcode-session";

fn is_closed_transport(error: &RpcError) -> bool {
    error.message.starts_with("ZCode agent transport closed")
}

pub async fn read_workspace_presentation(
    runtime: &AgentRuntime,
    target: &WorkspaceTarget,
) -> Result<Value, RpcError> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let process = runtime
            .get_client(target, RuntimePolicy::StartIfNeeded)
            .await?;
        runtime
            .before_use(&process, UseReason::ReadPresentation)
            .await?;
        let mut params = Map::new();
        params.insert("workspace".into(), target.workspace_ref());
        if process
            .capabilities()
            .await
            .is_some_and(|capabilities| capabilities["workspaceExecutionCapabilities"] == true)
        {
            params.insert("includeExecutionCapabilities".into(), json!(true));
        }
        match process
            .request_value("workspace/readPresentation", &Value::Object(params))
            .await
        {
            // 进程恰好在两次请求之间退出：换一代重试一次，与 Node 的 stale client 重试相同。
            Err(error) if attempt == 1 && is_closed_transport(&error) => continue,
            Err(error) => {
                log::warn(format!(
                    "readWorkspacePresentation failed for {}: {}",
                    target.key(),
                    error.message
                ));
                return Err(error);
            }
            Ok(presentation) => return Ok(presentation),
        }
    }
}

pub fn service(runtime: AgentRuntime) -> ServiceBuilder {
    let presentation_runtime = runtime.clone();
    ServiceBuilder::new(CHANNEL)
        .method("readWorkspacePresentation", move |_, args| {
            let runtime = presentation_runtime.clone();
            async move {
                let target = WorkspaceTarget::from_value(args.raw(0).unwrap_or(&Value::Null))?;
                read_workspace_presentation(&runtime, &target)
                    .await
                    .map(Reply::Value)
            }
        })
        .method("initializeWorkspace", move |_, args| {
            let runtime = runtime.clone();
            async move {
                let target = WorkspaceTarget::from_value(args.raw(0).unwrap_or(&Value::Null))?;
                Ok(Reply::Value(runtime.initialize(&target).await))
            }
        })
}

/// Host 启动预热（packages/desktop/src/host/index.ts warmUpZCodeAgent）：让 `existing-only` 的
/// sessions-index 订阅一启动就有进程可连。失败只记日志。
pub fn warm_up(runtime: AgentRuntime, targets: Vec<WorkspaceTarget>) {
    for target in targets {
        let runtime = runtime.clone();
        tokio::spawn(async move {
            let result = runtime.initialize(&target).await;
            let workspace = &target.workspace_path;
            if result["available"] == true {
                log::info(format!("ZCode agent warmup ready workspace={workspace}"));
            } else if result["reasonCode"] == "provider_not_ready" {
                log::info(format!(
                    "ZCode agent warmup waiting for provider/model workspace={workspace}"
                ));
            } else {
                log::warn(format!(
                    "ZCode agent warmup unavailable workspace={workspace} reason={}",
                    result["reason"].as_str().unwrap_or("unknown")
                ));
            }
        });
    }
}
