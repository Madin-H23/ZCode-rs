//! R6 外部集成与自动化（docs/specs/rust-host-automation.md）：automation（定时任务）、
//! 闲时任务、bots、conversation-share、cua-permission。
//!
//! [`install`] 是本区域在 `crate::install` 里的唯一入口：先构建共享的 [`Area`]（Host 内部 task
//! 驱动、Bot 回推挂钩），再依次安装各子模块。子模块只依赖 [`Area`]，彼此之间经它交互。
pub mod cron;
pub mod dispatch;
pub mod driver;
pub mod members;
pub mod reverse;
pub mod schedule;
pub mod service;
#[cfg(test)]
mod tests;
pub mod time;

use std::any::Any;
use std::sync::{Arc, OnceLock};

use futures_util::future::BoxFuture;
use serde_json::Value;
use zcode_host_control::ControlRouter;
use zcode_host_kit::ChannelRegistry;

use crate::ServiceContext;
use crate::agent::sessions_index::SessionsIndexObserver;
use crate::agent::{AgentRuntime, WorkspaceTarget};
use crate::main_bridge::MainBridge;
use crate::providers::ProviderRuntime;
use crate::settings::SettingStore;
use crate::tasks::service::TaskService;
use crate::tasks_index::TasksIndex;

/// 子模块安装后需要随 Host 存活的对象（后台任务句柄、服务实例）。
pub type Keepalive = Box<dyn Any + Send + Sync>;

/// automation 派发前登记 Bot 终态回推（cronBotDelivery.ts `watchAutomationRun`）。
pub trait BotDelivery: Send + Sync {
    /// `target` 是 `ZCodeAutomationBotDeliveryTarget`；`task_id` 是本次运行的会话。
    fn watch_automation_run(
        &self,
        target: Value,
        task_id: String,
        workspace: WorkspaceTarget,
    ) -> BoxFuture<'static, Result<(), String>>;
}

/// account 区域提供给本区域的请求期鉴权与 ZCode 来源头（闲时任务的真实凭据链，
/// docs/specs/rust-host-integration.md §2.1）。
#[derive(Clone)]
pub struct AccountLink {
    pub auth: crate::account::auth::AccountRequestAuth,
    pub source_headers: crate::account::net::SourceHeaders,
}

/// 本区域共享的依赖。
pub struct Area {
    /// Host 内部 task 驱动（建会话、发输入、终态），automation、闲时任务、bots 共用。
    pub driver: Arc<driver::TaskDriver>,
    pub context: Arc<ServiceContext>,
    pub bridge: MainBridge,
    pub settings: SettingStore,
    pub providers: ProviderRuntime,
    pub agent: AgentRuntime,
    pub observer: Arc<SessionsIndexObserver>,
    pub tasks: Arc<TaskService>,
    pub tasks_index: TasksIndex,
    /// credentials.json 的唯一读写入口（account 区域所有；bots 读应用凭据也走它）。
    pub credentials: crate::account::credential::CredentialStore,
    /// 当前账号的 OAuth 凭据（account 区域所有），会话分享等需要 zcode JWT 的出站请求从这里取 token。
    pub oauth: crate::account::oauth_repo::OAuthCredentialRepo,
    /// bots 安装时登记；没有登记（或 Bot 未配置）时 automation 不做回推。
    pub bot_delivery: OnceLock<Arc<dyn BotDelivery>>,
    /// 账号栈（[`Area::with_account`]）；没有时闲时任务只有 mock 凭据。
    pub account: Option<AccountLink>,
    /// Bot 远端运行时（[`Area::with_remote_bots`]，远端区域提供）；没有时远端上下文视为未连接。
    pub remote_bots: Arc<dyn crate::bots::RemoteBots>,
    /// 调度器的唤醒信号：先于 Scheduler 创建，闲时任务翻为可派发时经它唤醒（rust-u4-app-host.md §2.4）。
    pub scheduler_wake: crate::scheduler::WakeSignal,
}

impl Area {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        context: Arc<ServiceContext>,
        bridge: MainBridge,
        settings: SettingStore,
        providers: ProviderRuntime,
        agent: AgentRuntime,
        observer: Arc<SessionsIndexObserver>,
        tasks: Arc<TaskService>,
        tasks_index: TasksIndex,
        credentials: crate::account::credential::CredentialStore,
        oauth: crate::account::oauth_repo::OAuthCredentialRepo,
    ) -> Self {
        let driver = driver::TaskDriver::start(agent.clone(), tasks.clone(), &observer);
        Self {
            driver,
            context,
            bridge,
            settings,
            providers,
            agent,
            observer,
            tasks,
            tasks_index,
            credentials,
            oauth,
            bot_delivery: OnceLock::new(),
            account: None,
            remote_bots: Arc::new(crate::bots::NoRemote),
            scheduler_wake: crate::scheduler::WakeSignal::new(),
        }
    }

    /// 接入账号栈的请求期鉴权与来源头（install 时由 account 区域提供）。
    pub fn with_account(mut self, account: AccountLink) -> Self {
        self.account = Some(account);
        self
    }

    /// 接入远端区域的 Bot 远端运行时（docs/specs/rust-host-remote.md §2.9）。
    pub fn with_remote_bots(mut self, remote: Arc<dyn crate::bots::RemoteBots>) -> Self {
        self.remote_bots = remote;
        self
    }
}

pub struct AreaHandle {
    _parts: Vec<Keepalive>,
    /// Host 退出时需要异步收尾的子模块（目前只有 bots）。
    bots: Option<Arc<crate::bots::service::Bots>>,
    area: Arc<Area>,
    service: Arc<service::AutomationService>,
    cron: Arc<dispatch::Dispatcher>,
    /// 闲时任务模块没能安装时为 None。
    off_peak: Option<Arc<crate::off_peak::dispatch::OffPeakDispatcher>>,
}

impl AreaHandle {
    /// 本地 Bots（远端区域据此派生远端 attachment 的 Bots 实例）。
    pub fn bots(&self) -> Option<&Arc<crate::bots::service::Bots>> {
        self.bots.as_ref()
    }

    /// Host 退出（`Installed::shutdown`，CLI 回收之前）：bots 停通道运行时并释放跨进程运行锁
    /// （Node `disposeServiceResourcesAndWait` 里的 `IBotsService.disposeAllAndWait`）；有上限。
    pub async fn shutdown(&self) {
        if let Some(bots) = &self.bots {
            bots.shutdown().await;
        }
    }

    /// 启动调度状态机（docs/specs/rust-u4-app-host.md §2.4）：派发直接调用本区域的派发器，不经 Main。
    /// 须在全部服务安装完成（数据库已就绪）之后调用；Host 退出时由调用方 `dispose`。
    pub fn start_scheduler(
        &self,
        active_count: crate::scheduler::ActiveCountPort,
    ) -> crate::scheduler::SchedulerHandle {
        let off_peak_dispatch: Arc<dyn crate::scheduler::OffPeakDispatch> = match &self.off_peak {
            Some(dispatcher) => Arc::new(dispatcher.clone()),
            None => Arc::new(crate::scheduler::OffPeakUnavailable),
        };
        crate::scheduler::Scheduler::start(
            crate::scheduler::SchedulerDeps {
                automations: self.service.clone(),
                off_peak: crate::tasks_index::off_peak_repo::OffPeakRepo::new(
                    self.area.tasks_index.clone(),
                ),
                cron: Arc::new(self.cron.clone()),
                off_peak_dispatch,
                active_count,
                wake: self.area.scheduler_wake.clone(),
            },
            crate::scheduler::SchedulerOptions::default(),
        )
    }
}

pub fn install(
    area: Area,
    registry: &mut ChannelRegistry,
    router: &mut ControlRouter,
) -> AreaHandle {
    let area = Arc::new(area);
    let mut parts: Vec<Keepalive> = vec![Box::new(area.clone())];
    let (service, cron) = install_automation(&area);
    parts.extend(crate::cua_permission::install(&area, registry, router));
    parts.extend(crate::conversation_share::install(&area, registry, router));
    let bots = crate::bots::install(&area, registry, router);
    let off_peak = crate::off_peak::install(&area, registry).map(|installed| {
        parts.push(installed.keepalive);
        installed.dispatcher
    });
    AreaHandle {
        _parts: parts,
        bots,
        area,
        service,
        cron,
        off_peak,
    }
}

/// automation 本体：服务、`zcode-agent` 成员、CLI 反向请求与派发器（由 Scheduler 调用）。
fn install_automation(
    area: &Arc<Area>,
) -> (Arc<service::AutomationService>, Arc<dispatch::Dispatcher>) {
    let zone: Arc<dyn time::Zone> = Arc::new(time::local_zone());
    let clock: service::Clock = Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as i64)
            .unwrap_or_default()
    });
    let repo = crate::tasks_index::automation_repo::AutomationRepo::new(area.tasks_index.clone());
    let service = Arc::new(service::AutomationService::new(repo, zone, clock));
    let dispatcher =
        dispatch::Dispatcher::start(area.clone(), service.clone(), area.driver.clone());
    members::register(&area.agent, service.clone(), dispatcher.clone());
    reverse::register(&area.agent, service.clone(), area.tasks_index.clone());
    (service, dispatcher)
}
