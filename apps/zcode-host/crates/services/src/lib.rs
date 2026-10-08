//! Rust Host 的业务 service（docs/specs/rust-host.md §5）。
//!
//! [`install`] 在收到 `InitLocal` 时调用：按 Main 给的路径和环境构建已实现的 service，覆盖契约
//! stub，并登记它们关心的 Main → Host 控制消息。没实现的成员仍由 stub 立即应答。
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use zcode_host_control::{ControlRouter, ControlSink, InitLocal};
use zcode_host_kit::ChannelRegistry;

pub mod account;
pub mod agent;
pub mod broadcast;
pub mod file;
pub mod main_bridge;
pub mod providers;
pub mod runtime_env;
pub mod settings;
pub mod store;
pub mod system;
pub mod tasks;

use main_bridge::MainBridge;

/// 进程环境快照，由 composition root 读取后传入；service 不直接读 `std::env`。
#[derive(Debug, Clone, Default)]
pub struct HostEnvironment {
    /// 设置文件所在的 home：`ZCODE_DESKTOP_HOME_DIR`，否则 `HOME` / `USERPROFILE`。
    pub home_dir: PathBuf,
    /// 业务数据根：`ZCODE_DATA_BASE_DIR`，否则 `HOME`（`~/.zcode/v2` 在它下面）。
    pub data_base_dir: PathBuf,
    /// Node `os.homedir()`：`HOME`，Windows 上为 `USERPROFILE`；不受上面两个覆盖影响。
    pub user_home: PathBuf,
    /// Host 自身的环境变量（Main 启动 Host 时给的），子进程环境以它为底。
    pub vars: BTreeMap<String, String>,
    /// 向上查找开发态 CLI bundle 的起点：进程 cwd 与 Host 可执行文件所在目录。
    pub search_roots: Vec<PathBuf>,
}

impl HostEnvironment {
    pub fn capture(vars: impl IntoIterator<Item = (String, String)>) -> Self {
        let vars: BTreeMap<String, String> = vars.into_iter().collect();
        let non_empty = |name: &str| {
            vars.get(name)
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        let user_home = non_empty("HOME")
            .or_else(|| non_empty("USERPROFILE"))
            .unwrap_or_default();
        Self {
            home_dir: non_empty("ZCODE_DESKTOP_HOME_DIR").unwrap_or_else(|| user_home.clone()),
            data_base_dir: non_empty("ZCODE_DATA_BASE_DIR").unwrap_or_else(|| user_home.clone()),
            user_home,
            vars,
            search_roots: Vec::new(),
        }
    }

    /// `<data_base_dir>/.zcode/v2`（`getAppConfigDir`）。
    pub fn app_config_dir(&self) -> PathBuf {
        self.data_base_dir.join(".zcode").join("v2")
    }
}

pub struct ServiceContext {
    pub environment: HostEnvironment,
    pub init: InitLocal,
    pub control: Arc<dyn ControlSink>,
}

/// 需要在 Host 退出时清理的资源。
pub struct Installed {
    agent: agent::AgentRuntime,
    /// 观察者只被弱引用的生命周期任务持有，必须留在这里才能存活。
    _observer: Arc<agent::sessions_index::SessionsIndexObserver>,
}

impl Installed {
    /// Host 退出：回收全部 CLI 进程树（有上限，不会无限等待）。
    pub async fn shutdown(&self) {
        self.agent.dispose_all().await;
    }
}

pub fn install(
    context: Arc<ServiceContext>,
    registry: &mut ChannelRegistry,
    router: &mut ControlRouter,
) -> Installed {
    let bridge = MainBridge::new(context.control.clone());
    let settings = settings::SettingStore::new(&context.environment.home_dir);
    registry.implement(broadcast::service(bridge.clone(), router));
    registry.implement(settings::service(settings.clone()));

    let providers = providers::ProviderRuntime::new(
        context
            .init
            .zcode_builtin_provider_config_file_path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
            .map(PathBuf::from),
        context
            .environment
            .app_config_dir()
            .join("provider_config.json"),
    );
    providers.start_polling();
    registry.implement(providers::provider_settings_service(providers.clone()));
    registry.implement(providers::model_selection_service(providers.clone()));

    registry.implement(account::oauth_service(&context.environment.vars));
    registry.implement(account::onboarding_record_service());
    registry.implement(account::usage_stats_service());
    registry.implement(account::coding_plan_service(providers.clone()));

    registry.implement(file::service(file::FileServiceConfig {
        conversation_workspace: context
            .environment
            .data_base_dir
            .join(".zcode")
            .join("workspace")
            .join("default"),
        user_home: context.environment.user_home.clone(),
    }));
    registry.implement(system::service(
        context.environment.user_home.clone(),
        context.environment.vars.clone(),
    ));

    let environment = &context.environment;
    let agent = agent::AgentRuntime::new(
        agent::AgentRuntimeConfig {
            base_env: runtime_env::effective_base_env(
                &environment.vars,
                &context.init.runtime_process_env_patch,
            ),
            command_vars: environment.vars.clone(),
            search_roots: environment.search_roots.clone(),
            fallback_cwd: context
                .init
                .agent_spawn_fallback_cwd
                .as_deref()
                .filter(|path| !path.trim().is_empty())
                .map(PathBuf::from),
        },
        settings.clone(),
        providers.clone(),
    );
    let fallback = registry.get(agent::service::CHANNEL);
    registry.register(
        agent::service::CHANNEL,
        agent::service::AgentChannel::new(agent.clone(), fallback),
    );
    registry.implement(agent::session::service(agent.clone()));
    let observer = agent::sessions_index::SessionsIndexObserver::start(agent.clone());
    agent::session::warm_up(
        agent.clone(),
        context
            .init
            .agent_warmup_targets
            .iter()
            .map(|target| agent::WorkspaceTarget {
                workspace_path: target.workspace_path.clone(),
                workspace_identity: target.workspace_identity.clone(),
                remote_session_id: None,
            })
            .collect(),
    );
    Installed {
        agent,
        _observer: observer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_prefers_explicit_overrides() {
        let env = HostEnvironment::capture([
            ("HOME".to_owned(), "/home/u".to_owned()),
            ("ZCODE_DESKTOP_HOME_DIR".to_owned(), "/tmp/desk".to_owned()),
        ]);
        assert_eq!(env.home_dir, PathBuf::from("/tmp/desk"));
        assert_eq!(env.data_base_dir, PathBuf::from("/home/u"));
        assert_eq!(env.app_config_dir(), PathBuf::from("/home/u/.zcode/v2"));
    }
}
