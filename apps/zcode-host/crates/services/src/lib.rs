//! Rust Host 的业务 service（docs/specs/rust-host.md §5）。
//!
//! [`install`] 在收到 `InitLocal` 时调用：按 Main 给的路径和环境构建已实现的 service，覆盖契约
//! stub，并登记它们关心的 Main → Host 控制消息。没实现的成员仍由 stub 立即应答。
use std::path::PathBuf;
use std::sync::Arc;

use zcode_host_control::ControlRouter;
use zcode_host_kit::ChannelRegistry;

pub use zcode_host_account::account;
pub use zcode_host_automation::automation;
pub use zcode_host_automation::bots;
pub use zcode_host_automation::conversation_share;
pub use zcode_host_automation::cua_permission;
pub use zcode_host_automation::integration;
pub use zcode_host_automation::off_peak;
pub use zcode_host_automation::scheduler;
pub use zcode_host_base::broadcast;
pub use zcode_host_base::environment;
pub use zcode_host_base::main_bridge;
pub use zcode_host_base::store;
pub use zcode_host_base::support;
pub use zcode_host_base::testing;
pub use zcode_host_extensions::extensions;
pub use zcode_host_extensions::settings_sync;
pub use zcode_host_providers::providers;
pub use zcode_host_remote::remote;
pub use zcode_host_sessions::agent;
pub use zcode_host_sessions::tasks;
pub use zcode_host_sessions::tasks_index;
pub use zcode_host_settings::settings;
pub use zcode_host_workspace::file;
pub use zcode_host_workspace::file_watcher;
pub use zcode_host_workspace::git;
pub use zcode_host_workspace::media_preview;
pub use zcode_host_workspace::prompt_attachment_transfer;
pub use zcode_host_workspace::system;
pub use zcode_host_workspace::terminal;
pub use zcode_host_workspace::workspace;

pub use environment::{HostEnvironment, ServiceContext};
use main_bridge::MainBridge;
use support::env::ProcessEnv;

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
    automation: automation::AreaHandle,
    /// 调度状态机（rust-u4-app-host.md §2.4）：install 结束时启动，Host 退出时最先收尾。
    scheduler: scheduler::SchedulerHandle,
    remote: Arc<remote::RemoteWorkspaces>,
    /// 至少保留一个句柄，否则 tasks-index 线程与连接随之关闭。
    _storage: Storage,
}

impl Installed {
    /// Host 退出：回收终端与全部 CLI 进程树（有上限，不会无限等待）。
    pub async fn shutdown(&self) {
        // 先停调度：等当前一轮结束、释放在途认领，此后不再向即将回收的 CLI 派发。
        self.scheduler.dispose().await;
        self.workspace.shutdown();
        self.remote.shutdown().await;
        // Bot 通道运行时在 CLI 之前收尾：停长轮询 / 长连接并立即释放跨进程运行锁。
        self.automation.shutdown().await;
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
    let coding_plan = account.register_coding_plan(registry, &providers);
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
        agent::session::SessionService::new(agent_api.clone(), Some(task_service.clone())),
    ));

    // ── remote ──（docs/specs/rust-host-remote.md）
    // 先于 automation：本地 Bots 安装时注入远端区域的 Bot 远端运行时（§2.9）。
    let remote = remote::install(
        &context,
        remote::provisioning::ProvisioningSources {
            settings: settings.clone(),
            credentials: account.credentials.clone(),
            providers: providers.clone(),
        },
        &controller,
        task_context.realtime().cloned(),
        router,
    );

    // ── automation ──（R6：automation、闲时任务、bots、conversation-share、cua-permission）
    let automation = automation::install(
        automation::Area::new(
            context.clone(),
            bridge.clone(),
            settings.clone(),
            providers.clone(),
            agent.clone(),
            observer.clone(),
            task_service,
            storage.tasks_index.clone(),
            account.credentials.clone(),
            account.repo.clone(),
        )
        .with_account(automation::AccountLink {
            auth: account.auth.clone(),
            source_headers: account.api.source_headers().clone(),
        })
        .with_remote_bots(remote.bots_runtime()),
        registry,
        router,
    );
    // 远端 attachment 的 bots channel 按连接从本地 Bots 派生（§2.9.8）。
    if let Some(bots) = automation.bots() {
        remote.bind_bots(bots.clone());
    }

    // ── workspace ──
    // git（提交信息生成回调 Agent）、git-checkpoint、media-preview（Main 授权路径）、
    // prompt-attachment-transfer；见 docs/specs/rust-host-workspace.md。
    let workspace = workspace::install(
        workspace::WorkspaceDeps {
            environment: &context.environment,
            child_env: &child_env,
            settings: settings.clone(),
            agent: agent.clone(),
            agent_api,
            providers: providers.clone(),
            bridge,
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
    // ── integration ──（docs/specs/rust-host-integration.md）
    // 动态工作流门禁的唯一写入方：coding-plan-subscription 的 client config（Node
    // `resolveDynamicWorkflowClientConfig`）。必须在预热拉起 CLI 之前登记，进程就绪前的
    // `workspace/updateDynamicWorkflowPolicy` 与会话请求的 flag 都读它。
    agent
        .extensions()
        .set_dynamic_workflow_gate(integration::dynamic_workflow_gate(coding_plan));

    // ── setting（数据目录迁移）──（docs/specs/rust-host-setting.md）
    // `updateDataBaseDir` 叠在 R1 的 setting channel 上：守卫 → 复制 `.zcode/v2` → 经 SettingStore
    // 写 `dataBaseDir`。Host 自己的数据根运行期不变，重启后由 Main 按 setting.json 下发。
    registry.implement(settings::data_dir::service(
        settings::data_dir::DataDirMove::new(settings, &context.environment),
    ));

    // ── scheduler ──（docs/specs/rust-u4-app-host.md §2.4）
    // 所有服务都已安装、tasks-index 已就绪：启动调度状态机（回收中断的闲时任务 → 第一轮）。
    // U4 接线点：执行中的闲时任务数 → Host → Main 的 `off-peak-active-count`（Main 据此维持防休眠）。
    // 控制消息由另一项改动定义并接在这里；在那之前上报为空操作。
    let scheduler = automation.start_scheduler(Arc::new(|_active_count: u64| {}));

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
        automation,
        scheduler,
        remote,
        _storage: storage,
    }
}
