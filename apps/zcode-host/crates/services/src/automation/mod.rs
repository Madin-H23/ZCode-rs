//! R6 外部集成与自动化（docs/specs/rust-host-automation.md）：automation（定时任务）、
//! 闲时任务、bots、conversation-share、cua-permission。
//!
//! [`install`] 是本区域在 `crate::install` 里的唯一入口：先构建共享的 [`Area`]（Host 内部 task
//! 驱动、Bot 回推挂钩），再依次安装各子模块。子模块只依赖 [`Area`]，彼此之间经它交互。
pub mod cron;
pub mod schedule;
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

/// 本区域共享的依赖。
pub struct Area {
    pub context: Arc<ServiceContext>,
    pub bridge: MainBridge,
    pub settings: SettingStore,
    pub providers: ProviderRuntime,
    pub agent: AgentRuntime,
    pub observer: Arc<SessionsIndexObserver>,
    pub tasks: Arc<TaskService>,
    pub tasks_index: TasksIndex,
    /// bots 安装时登记；没有登记（或 Bot 未配置）时 automation 不做回推。
    pub bot_delivery: OnceLock<Arc<dyn BotDelivery>>,
}

pub struct AreaHandle {
    _parts: Vec<Keepalive>,
}

pub fn install(
    area: Area,
    registry: &mut ChannelRegistry,
    router: &mut ControlRouter,
) -> AreaHandle {
    let area = Arc::new(area);
    let mut parts: Vec<Keepalive> = vec![Box::new(area.clone())];
    parts.extend(crate::cua_permission::install(&area, registry, router));
    parts.extend(crate::conversation_share::install(&area, registry, router));
    parts.extend(crate::bots::install(&area, registry, router));
    parts.extend(crate::off_peak::install(&area, registry, router));
    AreaHandle { _parts: parts }
}
