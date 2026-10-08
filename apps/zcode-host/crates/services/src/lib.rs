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
pub mod automation;
pub mod bots;
pub mod broadcast;
pub mod conversation_share;
pub mod cua_permission;
pub mod extensions;
pub mod file;
pub mod file_watcher;
pub mod git;
pub mod main_bridge;
pub mod media_preview;
pub mod off_peak;
pub mod prompt_attachment_transfer;
pub mod providers;
pub mod remote;
pub mod settings;
pub mod settings_sync;
pub mod store;
pub mod support;
pub mod system;
pub mod tasks;
pub mod tasks_index;
pub mod terminal;
#[cfg(test)]
mod testing;
pub mod workspace;

use main_bridge::MainBridge;
use support::env::ProcessEnv;

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
/// `InitLocal` 之后、安装服务之前准备好的存储（docs/specs/rust-host-r2.md §5.1）。
pub struct Storage {
    tasks_index: tasks_index::TasksIndex,
}

/// 迁移并打开 tasks-index.sqlite；`progress` 报告数据库阶段（`databasePhase`）。
pub async fn prepare_storage(
    environment: &HostEnvironment,
    progress: impl Fn(Option<&'static str>) + Send + Sync + 'static,
) -> Result<Storage, tasks_index::TasksIndexError> {
    let path = environment.app_config_dir().join("tasks-index.sqlite");
    let tasks_index = tasks_index::TasksIndex::open(path, move |event| {
        progress(database_phase(event.phase));
    })
    .await?;
    Ok(Storage { tasks_index })
}

/// StoragePhase → `DatabaseStartupState.databasePhase`（failed 不是数据库阶段）。
fn database_phase(phase: tasks_index::StoragePhase) -> Option<&'static str> {
    use tasks_index::StoragePhase;
    match phase {
        StoragePhase::Checking => Some("checking"),
        StoragePhase::WaitingForLock => Some("waiting_for_lock"),
        StoragePhase::Migrating => Some("migrating"),
        StoragePhase::Committing => Some("committing"),
        StoragePhase::Maintaining => Some("maintaining"),
        StoragePhase::Ready => Some("ready"),
        StoragePhase::Failed => None,
    }
}

pub struct Installed {
    agent: agent::AgentRuntime,
    workspace: workspace::Workspace,
    /// 观察者只被弱引用的生命周期任务持有，必须留在这里才能存活。
    _observer: Arc<agent::sessions_index::SessionsIndexObserver>,
    _controller: Arc<tasks::controller::Controller>,
    _syncer: tokio::task::JoinHandle<()>,
    _automation: automation::AreaHandle,
    remote: Arc<remote::RemoteWorkspaces>,
    /// 至少保留一个句柄，否则 tasks-index 线程与连接随之关闭。
    _storage: Storage,
}

impl Installed {
    /// Host 退出：回收终端与全部 CLI 进程树（有上限，不会无限等待）。
    pub async fn shutdown(&self) {
        self.workspace.shutdown();
        self.remote.shutdown().await;
        self.agent.dispose_all().await;
    }

    /// 远端 workspace：host-core 用它签发并接入 `scope.kind = "remote"` 的 attachment。
    pub fn remote(&self) -> &Arc<remote::RemoteWorkspaces> {
        &self.remote
    }
}

pub fn install(
    context: Arc<ServiceContext>,
    storage: Storage,
    registry: &mut ChannelRegistry,
    router: &mut ControlRouter,
) -> Installed {
    let bridge = MainBridge::new(context.control.clone());
    let settings = settings::SettingStore::new(&context.environment.home_dir);
    let (broadcast_service, broadcaster) = broadcast::service_with_sender(bridge.clone(), router);
    registry.implement(broadcast_service);
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

    // 子进程（git、终端 shell）的基础环境：Host 环境清理后叠加登录 shell 补丁，不改写自身环境。
    let child_env = ProcessEnv::for_children(
        &context.environment.vars,
        &context.init.runtime_process_env_patch,
    );

    let environment = &context.environment;
    let agent = agent::AgentRuntime::new(
        agent::AgentRuntimeConfig {
            base_env: child_env.vars().clone(),
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
    let observer = agent::sessions_index::SessionsIndexObserver::start(agent.clone());

    // ── tasks ──
    // 侧栏：syncer 与 controller 必须先挂上观察者，再预热拉起 CLI，否则会错过首份快照。
    let workspace_events = tasks::WorkspaceEvents::new();
    let task_service = tasks::service::TaskService::new(
        Arc::new(storage.tasks_index.clone()),
        workspace_events.clone(),
        agent.clone(),
    );
    registry.implement(tasks::service::service(task_service.clone()));
    let task_context = tasks::context::TaskContext::new(
        tasks::context::TaskPaths {
            user_home: context.environment.user_home.clone(),
            data_base_dir: context.environment.data_base_dir.clone(),
            log_dir: context
                .environment
                .vars
                .get("ZCODE_LOG_DIR")
                .map(|dir| dir.trim())
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from),
        },
        storage.tasks_index.clone(),
        task_service.clone(),
        agent.clone(),
        settings.clone(),
    );
    registry.implement(tasks::index_members::service(task_context.clone()));
    // 生命周期、查询与事件成员；automation / off-peak / bots 也可直接用 TaskLifecycle 的 API。
    let task_lifecycle = tasks::lifecycle::TaskLifecycle::start(task_context.clone());
    registry.implement(tasks::lifecycle_service::service(task_lifecycle));
    // task realtime bridge：与 Main taskRealtimeBus 的 lease / owner 命令；会话消息总是应答。
    tasks::realtime::register_session_messages(router, MainBridge::new(context.control.clone()));
    if let Some(realtime) = tasks::realtime::TaskRealtimeBridge::for_host_init(
        context.init.host_id.as_deref(),
        context.init.delivery_kind.as_deref(),
        MainBridge::new(context.control.clone()),
    ) {
        realtime.register(router);
        task_context.set_realtime(realtime);
    }
    let syncer = tasks::syncer::start(&observer, agent.clone(), task_service.clone());
    let controller = tasks::controller::Controller::start(
        Arc::new(task_context.clone()),
        observer.clone(),
        workspace_events,
    );
    registry.implement(tasks::controller::service(controller.clone()));

    // ── account ──
    // 账号栈（docs/specs/rust-host-account.md）：凭据、ApiClient、OAuth、账号 provider overlay、
    // CLI 集成，以及账号增值 channel。必须在预热拉起 CLI 之前登记 CLI 扩展点。
    let account = account::AccountStack::new(&context, &settings, &providers);
    account.register(
        registry,
        storage.tasks_index.clone(),
        context.environment.app_config_dir(),
    );
    account.register_coding_plan(registry, &providers);
    account.register_provider_writes(registry, &providers, &agent);
    account.register_feedback(registry, &context, bridge.clone(), router);
    registry.implement(settings_sync::service(settings_sync::SettingsSync::new(
        settings.clone(),
        &context.environment.vars,
        &context.environment.user_home,
    )));
    account.attach_agent(&agent, &context);
    account.start(broadcaster);
    account::telemetry::start_host_resource_samples(context.control.clone());

    // ── agent ──（docs/specs/rust-host-agent.md）
    // 三个 lane、CLI 通知分发、浏览器控制、进程生命周期上报，以及 zcode-agent / zcode-session /
    // plugin-management 三个 channel。AgentChannel 在最外层：连接作用域成员由它处理，其余回退到
    // members，再回退到先前登记的实现（例如 automation 成员）与契约 stub。
    let agent_main = MainBridge::new(context.control.clone());
    agent::process_events::forward(agent.extensions(), agent_main.clone());
    let agent_browser = agent::browser::BrowserBridge::new(Some(agent_main.clone()));
    agent_browser.install(router, agent.extensions());
    let agent_events =
        agent::notifications::AgentEvents::start(agent.extensions(), Some(agent_main));
    let agent_api = agent::api::AgentApi::new(
        agent.clone(),
        &context.environment.data_base_dir,
        agent_events,
        Some(agent_browser),
    )
    .with_tasks_index(storage.tasks_index.clone());
    registry.implement(agent::members::agent_service(&agent_api));
    registry.implement(agent::members::plugin_management_service(&agent_api));
    let fallback = registry.get(agent::service::CHANNEL);
    registry.register(
        agent::service::CHANNEL,
        agent::service::AgentChannel::new(agent_api.clone(), fallback),
    );
    registry.implement(agent::session::service(
        agent::session::SessionService::new(agent_api, Some(task_service.clone())),
    ));

    // ── automation ──（R6：automation、闲时任务、bots、conversation-share、cua-permission）
    let automation = automation::install(
        automation::Area::new(
            context.clone(),
            bridge.clone(),
            settings.clone(),
            providers.clone(),
            agent.clone(),
            observer.clone(),
            task_service.clone(),
            storage.tasks_index.clone(),
            account.credentials.clone(),
            account.repo.clone(),
        ),
        registry,
        router,
    );

    // ── workspace ──
    // git（提交信息生成回调 Agent）、git-checkpoint、media-preview（Main 授权路径）、
    // prompt-attachment-transfer；见 docs/specs/rust-host-workspace.md。
    let workspace = workspace::install(
        workspace::WorkspaceDeps {
            environment: &context.environment,
            child_env: &child_env,
            settings: settings.clone(),
            agent: agent.clone(),
            providers: providers.clone(),
            bridge: bridge.clone(),
        },
        registry,
        router,
    );

    // ── extensions ──
    extensions::install(
        registry,
        &agent,
        extensions::Roots::new(
            &context.environment.user_home,
            &context.environment.data_base_dir,
        ),
    );
    // ── remote ──
    let remote = remote::install(&context, settings.clone(), &controller, router);

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
        workspace,
        _observer: observer,
        _controller: controller,
        _syncer: syncer,
        _automation: automation,
        remote,
        _storage: storage,
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
