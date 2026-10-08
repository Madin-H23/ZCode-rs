//! 账号栈（docs/specs/rust-host-account.md）：凭据、网络客户端、OAuth、账号 provider、
//! CLI 集成与账号相关 channel 的组装。
//!
//! 组装顺序即依赖方向：凭据 → 端点 / ApiClient → OAuth 仓储与服务 → 账号鉴权与权益 →
//! 账号 provider overlay（写入 ProviderRuntime）→ CLI 集成。ApiClient 与 OAuth 之间的环
//! （401 触发退出）用延迟登记的处理器解开。
pub mod account_providers;
pub mod auth;
pub mod availability;
pub mod cli;
pub mod client_config;
pub mod credential;
pub mod endpoint;
mod legacy_stub;
pub mod net;
pub mod oauth;
pub mod oauth_adapters;
pub mod oauth_repo;
pub mod official_mcp;
pub mod onboarding;
#[cfg(test)]
pub(crate) mod oauth_tests;

use std::sync::Arc;

use serde_json::json;
use zcode_host_kit::{ChannelRegistry, log};

use crate::ServiceContext;
use crate::agent::AgentRuntime;
use crate::broadcast::Broadcaster;
use crate::providers::ProviderRuntime;
use crate::settings::SettingStore;
use crate::tasks_index::{ListFilter, TasksIndex};

pub use legacy_stub::{coding_plan_service, usage_stats_service};

/// `ZCODE_JWT_INVALID_BROADCAST_CHANNEL`。
const JWT_INVALID_BROADCAST_CHANNEL: &str = "auth:zcode-jwt-invalid";

/// 账号栈的各个所有者；R4 的 channel 与 CLI 集成都从这里取依赖。
#[derive(Clone)]
pub struct AccountStack {
    pub credentials: credential::CredentialStore,
    pub endpoints: endpoint::Endpoints,
    pub api: net::ApiClient,
    pub repo: oauth_repo::OAuthCredentialRepo,
    pub oauth: oauth::OAuthService,
    pub account_api: auth::AccountApi,
    pub keys: auth::CodingPlanKeys,
    pub auth: auth::AccountRequestAuth,
    pub billing: availability::StartPlanBilling,
    pub availability: availability::AvailabilityResolver,
    pub providers: account_providers::AccountProviderSource,
    pub official_mcp: official_mcp::OfficialMcpCredentials,
}

impl AccountStack {
    /// 构建账号栈并把账号 overlay 接入 provider runtime（之后的读取等首份账号快照）。
    pub fn new(context: &ServiceContext, settings: &SettingStore, runtime: &ProviderRuntime) -> Self {
        let environment = &context.environment;
        let app_config_dir = environment.app_config_dir();
        let credentials = credential::CredentialStore::new(
            &app_config_dir,
            &credential::credential_secret(&environment.vars),
        );
        let endpoints = endpoint::Endpoints::new(environment.vars.clone(), Some(settings.clone()));
        let api = net::ApiClient::new(endpoints.clone(), app_config_dir.clone(), settings.clone());
        let repo = oauth_repo::OAuthCredentialRepo::new(credentials.clone());
        let oauth = oauth::OAuthService::new(repo.clone(), api.clone());
        let account_api = auth::AccountApi::new(api.clone());
        let keys = auth::CodingPlanKeys::new(credentials.clone(), repo.clone(), account_api.clone());
        let auth = auth::AccountRequestAuth::new(
            settings.clone(),
            repo.clone(),
            keys.clone(),
            account_api.clone(),
        );
        let billing = availability::StartPlanBilling::new(api.clone());
        let availability = availability::AvailabilityResolver::new(
            api.clone(),
            credentials.clone(),
            billing.clone(),
        );
        let providers = account_providers::AccountProviderSource::new(
            runtime.clone(),
            settings.clone(),
            auth.clone(),
            keys.clone(),
            availability.clone(),
        );
        let official_mcp = official_mcp::OfficialMcpCredentials::new(
            runtime.clone(),
            auth.clone(),
            credentials.clone(),
        );
        Self {
            credentials,
            endpoints,
            api,
            repo,
            oauth,
            account_api,
            keys,
            auth,
            billing,
            availability,
            providers,
            official_mcp,
        }
    }

    /// 退出、损坏登录态与 401 的处理器，以及账号 provider 的首轮刷新。
    pub fn start(&self, broadcaster: Broadcaster) {
        // 退出：删除该账号的个人 Coding Plan key，再刷新账号 provider（`oauthProviderLogout.ts`）。
        let logout = {
            let (keys, source) = (self.keys.clone(), self.providers.clone());
            Arc::new(move |provider: String, identity: Option<String>| {
                let (keys, source) = (keys.clone(), source.clone());
                Box::pin(async move {
                    if !matches!(provider.as_str(), oauth_repo::ZAI | oauth_repo::BIGMODEL) {
                        return Ok(());
                    }
                    if let Some(identity) = identity.filter(|id| !id.trim().is_empty()) {
                        keys.delete_for_logout(&provider, &identity).await;
                    }
                    source
                        .refresh(&format!("oauth-logout:{provider}"))
                        .await
                        .map(|_| ())
                }) as futures_util::future::BoxFuture<'static, Result<(), String>>
            })
        };
        self.oauth.set_logout_hook(logout.clone());
        // 本地 OAuth 凭据解密失败等价于强制退出：同一处理器清理派生凭据。
        self.repo.set_corrupt_cleared_handler(Arc::new(move |providers| {
            let logout = logout.clone();
            Box::pin(async move {
                for provider in providers {
                    if let Err(error) = logout(provider.clone(), None).await {
                        log::warn(format!(
                            "[oauthCredentialRepo] clear derived provider keys after corrupt OAuth session failed provider={provider}: {error}"
                        ));
                    }
                }
            })
        }));
        // 401：只有在会话队列里复核为当前登录凭据才退出，并广播 JWT 失效。
        let oauth = self.oauth.clone();
        self.api.set_unauthorized_handler(Arc::new(move |url, headers| {
            let (oauth, broadcaster) = (oauth.clone(), broadcaster.clone());
            Box::pin(async move {
                match oauth.logout_if_current_credential_request(url, headers).await {
                    Ok(true) => broadcaster.send(json!({
                        "channel": JWT_INVALID_BROADCAST_CHANNEL,
                        "payload": {},
                    })),
                    Ok(false) => {}
                    Err(error) => log::warn(format!(
                        "[zcodeJwtLogout] ZCode JWT logout failed: {}",
                        error.message
                    )),
                }
            })
        }));
        self.providers.start();
    }

    /// 账号相关 channel。
    pub fn register(&self, registry: &mut ChannelRegistry, tasks_index: TasksIndex, app_config_dir: std::path::PathBuf) {
        registry.implement(credential::service(self.credentials.clone()));
        registry.implement(oauth::service(self.oauth.clone()));
        registry.implement(client_config::client_config_service(
            client_config::ClientConfig::new(self.api.clone()),
        ));
        registry.implement(client_config::client_scenes_service(self.api.clone()));
        let repo = self.repo.clone();
        let records = onboarding::OnboardingRecords::new(
            app_config_dir,
            Arc::new(move || {
                let repo = repo.clone();
                Box::pin(async move {
                    repo.active_user_profile()
                        .await
                        .ok()
                        .flatten()
                        .map(|profile| profile.id)
                })
            }),
            Arc::new(move || {
                let tasks_index = tasks_index.clone();
                Box::pin(async move {
                    tasks_index
                        .list_task_metas(ListFilter::default())
                        .await
                        .map(|metas| !metas.is_empty())
                        .unwrap_or(false)
                })
            }),
        );
        registry.implement(onboarding::service(records));
    }

    /// CLI 集成：Account Config 同步、反向请求、取消通知与 spawn 环境。
    pub fn attach_agent(&self, runtime: &AgentRuntime, context: &ServiceContext) {
        cli::register(
            runtime,
            cli::AccountConfigSync::new(self.providers.clone()),
            self.auth.clone(),
            self.official_mcp.clone(),
            self.endpoints.clone(),
            self.repo.clone(),
            cli::captured_telemetry_env(&context.environment.vars),
            context.init.device_mid.clone(),
        );
    }
}
