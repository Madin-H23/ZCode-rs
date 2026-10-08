//! 工作区能力的装配（docs/specs/rust-host-workspace.md）：git（含提交信息生成）、
//! git-checkpoint、media-preview、prompt-attachment-transfer，以及 Agent 的本地媒体路径授权。
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::Value;
use zcode_host_control::ControlRouter;
use zcode_host_kit::{ChannelRegistry, RpcError};

use crate::agent::api::AgentApi;
use crate::agent::runtime::AgentRuntime;
use crate::git::commit_message::{TextGenerator, runtime_locale};
use crate::git::{self, CommitMessageSupport, checkpoint, repo::GitRepo};
use crate::main_bridge::MainBridge;
use crate::providers::ProviderRuntime;
use crate::settings::SettingStore;
use crate::support::env::ProcessEnv;
use crate::terminal::Terminals;
use crate::{HostEnvironment, media_preview, prompt_attachment_transfer};
use crate::{file_watcher, terminal};

/// 提交信息生成回到 Agent：模型取 `model-selection` 的 preferredSelection，文本经 agent 区域唯一的
/// `AgentApi::generate_workspace_text`（Node `zcodeAgentService.generateWorkspaceText`；写门禁、使用前
/// 钩子、取消守卫都在那里，docs/specs/rust-host-integration.md §2.6）。
struct AgentTextGenerator {
    agent: AgentApi,
    providers: ProviderRuntime,
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
        Box::pin(async move { agent.generate_workspace_text(&params).await })
    }
}

pub struct WorkspaceDeps<'a> {
    pub environment: &'a HostEnvironment,
    pub child_env: &'a ProcessEnv,
    pub settings: SettingStore,
    pub agent: AgentRuntime,
    /// 生成文本的唯一实现（agent 区域）。
    pub agent_api: AgentApi,
    pub providers: ProviderRuntime,
    pub bridge: MainBridge,
}

/// Host 退出时需要回收的工作区资源。
pub struct Workspace {
    terminals: Terminals,
}

impl Workspace {
    pub fn shutdown(&self) {
        self.terminals.dispose_all();
    }
}

pub fn install(
    deps: WorkspaceDeps<'_>,
    registry: &mut ChannelRegistry,
    router: &mut ControlRouter,
) -> Workspace {
    registry.implement(file_watcher::service(file_watcher::FileWatchers::new()));
    let terminals = Terminals::new(deps.child_env.clone(), deps.settings);
    registry.implement(terminal::service(terminals.clone()));

    let repo = GitRepo::new(deps.child_env);
    let generator: Arc<dyn TextGenerator> = Arc::new(AgentTextGenerator {
        agent: deps.agent_api,
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
    Workspace { terminals }
}
