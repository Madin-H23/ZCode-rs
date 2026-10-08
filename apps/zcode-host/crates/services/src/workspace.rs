//! 工作区能力的装配（docs/specs/rust-host-workspace.md）：git（含提交信息生成）、
//! git-checkpoint、media-preview、prompt-attachment-transfer，以及 Agent 的本地媒体路径授权。
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use zcode_host_control::ControlRouter;
use zcode_host_kit::{ChannelRegistry, RpcError, log};

use crate::agent::extensions::UseReason;
use crate::agent::runtime::{AgentProcess, AgentRuntime, WorkspaceTarget};
use crate::git::commit_message::{TextGenerator, runtime_locale};
use crate::git::{self, CommitMessageSupport, checkpoint, repo::GitRepo};
use crate::main_bridge::MainBridge;
use crate::providers::ProviderRuntime;
use crate::support::env::ProcessEnv;
use crate::{HostEnvironment, media_preview, prompt_attachment_transfer};

const CANCEL_GENERATE_TEXT_TIMEOUT: Duration = Duration::from_secs(5);

/// 提交信息生成回到 Agent：模型取 `model-selection` 的 preferredSelection，文本经 CLI
/// `workspace/generateText`（Node `zcodeAgentService.generateWorkspaceText`）。
struct AgentTextGenerator {
    agent: AgentRuntime,
    providers: ProviderRuntime,
}

/// 调用 future 被丢弃（RPC 取消或连接关闭）时通知 CLI 取消这次生成；尽力而为。
struct CancelOnDrop {
    process: Option<Arc<AgentProcess>>,
    operation_id: String,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let Some(process) = self.process.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let operation_id = std::mem::take(&mut self.operation_id);
        runtime.spawn(async move {
            let params = json!({"operationId": operation_id});
            let cancel = process.observe("workspace/cancelGenerateText", &params);
            match tokio::time::timeout(CANCEL_GENERATE_TEXT_TIMEOUT, cancel).await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => log::debug(format!(
                    "workspace/cancelGenerateText failed: {}",
                    error.message
                )),
                Err(_) => log::debug("workspace/cancelGenerateText timed out"),
            }
        });
    }
}

/// `generateWorkspaceText`：写门禁 → 使用前钩子 → `workspace/generateText`（带 operationId）。
pub async fn generate_workspace_text(
    agent: &AgentRuntime,
    params: &Value,
) -> Result<Value, RpcError> {
    let target = WorkspaceTarget::from_value(params)?;
    let process = agent.get_ready_client(&target).await?;
    agent.before_use(&process, UseReason::GenerateText).await?;
    let operation_id = crate::agent::service::uuid_v4();
    let mut request = Map::new();
    request.insert("workspace".into(), target.workspace_ref());
    for key in ["selection", "prompt", "querySource"] {
        if let Some(value) = params.get(key).filter(|value| !value.is_null()) {
            request.insert(key.into(), value.clone());
        }
    }
    request.insert("operationId".into(), json!(operation_id));
    let mut guard = CancelOnDrop {
        process: Some(process.clone()),
        operation_id,
    };
    let result = process
        .request_value("workspace/generateText", &Value::Object(request))
        .await;
    guard.process = None;
    result
}

impl TextGenerator for AgentTextGenerator {
    fn current_model(&self) -> BoxFuture<'static, Option<Value>> {
        let providers = self.providers.clone();
        Box::pin(async move {
            let (revision, resolved) = providers.snapshot().await;
            resolved
                .model_selection_view(revision, None)
                .get("preferredSelection")
                .cloned()
        })
    }

    fn generate_text(&self, params: Value) -> BoxFuture<'static, Result<Value, RpcError>> {
        let agent = self.agent.clone();
        Box::pin(async move { generate_workspace_text(&agent, &params).await })
    }
}

pub struct WorkspaceDeps<'a> {
    pub environment: &'a HostEnvironment,
    pub child_env: &'a ProcessEnv,
    pub agent: AgentRuntime,
    pub providers: ProviderRuntime,
    pub bridge: MainBridge,
}

pub fn install(
    deps: WorkspaceDeps<'_>,
    registry: &mut ChannelRegistry,
    router: &mut ControlRouter,
) {
    let repo = GitRepo::new(deps.child_env);
    let generator: Arc<dyn TextGenerator> = Arc::new(AgentTextGenerator {
        agent: deps.agent.clone(),
        providers: deps.providers,
    });
    registry.implement(git::service_with(
        repo.clone(),
        CommitMessageSupport {
            generator: Some(generator),
            default_locale: runtime_locale(&deps.environment.vars),
        },
    ));
    let data_root = deps.environment.data_base_dir.join(".zcode");
    registry.implement(checkpoint::service(checkpoint::Checkpoints::new(
        repo,
        data_root,
        deps.environment.app_config_dir(),
    )));

    // 本地媒体路径只由 Main 授权（Main 登记后 zcode-media:// 协议才放行）。
    let authorizer = media_preview::main_authorizer(deps.bridge, router);
    deps.agent
        .extensions()
        .set_media_path_authorizer(authorizer.clone());
    registry.implement(media_preview::service(Some(authorizer)));
    registry.implement(prompt_attachment_transfer::service());
}
