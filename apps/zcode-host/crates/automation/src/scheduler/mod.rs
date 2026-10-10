//! 调度状态机（docs/specs/rust-u4-app-host.md §2.4）：`packages/desktop/src/scheduler` 的 Rust 移植，
//! 从 Main 起的 Node scheduler 进程迁入 Host。
//!
//! 一个 actor 任务独占全部调度状态（在途表、闲时任务退避表、上次上报的执行中计数），其他代码只经
//! [`SchedulerHandle`] 与它交互。派发在各自的任务里执行，结果回到 actor 结算；一轮不等待派发。
//!
//! ```text
//! Scheduler::start ─► recoverInterrupted ─► 第一轮 ─► loop {
//!     dispose()          ─► 等当前一轮 ─► 结算已完成的派发 ─► 取消在途派发 ─► 释放在途认领
//!     派发结果（JoinSet）─► 结算（tasks-index）─► automation 结算后 wake()
//!     wake()（Notify，合并）／20 s 单调定时器 ─► 一轮：claimDue → 派发 → claimManualRuns → 派发
//!                                              → 闲时 claimDue → 派发 → 上报执行中计数
//! }
//! ```
mod actor;
mod settle;

#[cfg(test)]
mod parity_tests;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use tokio::sync::{Notify, mpsc, oneshot};
use zcode_async::Spawner;

use crate::automation::dispatch::{CronRunRequest, Dispatcher};
use crate::automation::service::{AutomationService, Clock};
use crate::off_peak::dispatch::{OffPeakDispatcher, OffPeakRunRequest};
use crate::tasks_index::automation_repo::FailureKind;
use crate::tasks_index::off_peak_repo::OffPeakRepo;

/// 轮询间隔：cron 最小粒度是分钟，20 s 足以按时命中且开销低。
pub const TICK_INTERVAL: Duration = Duration::from_secs(20);

/// 派发失败（`cron-run-result` / `off-peak-run-result` 的 `error` 与 `failureKind`）。
#[derive(Debug, Clone, PartialEq)]
pub struct DispatchFailure {
    pub error: String,
    pub kind: FailureKind,
}

/// 把一次已认领的 automation 运行交给 Host 执行（`cron-run` 背后的派发器）。
///
/// 返回承载本次运行的会话 id。实现方完成整个派发（建会话或恢复绑定会话、发 prompt）后才返回；
/// 调用方在独立任务中等待它，不阻塞调度。
pub trait CronDispatch: Send + Sync {
    fn dispatch(
        &self,
        request: CronRunRequest,
    ) -> BoxFuture<'static, Result<String, DispatchFailure>>;
}

/// 把一次已认领的闲时任务交给 Host 执行（`off-peak-run` 背后的派发器），返回承载它的会话 id。
pub trait OffPeakDispatch: Send + Sync {
    fn dispatch(
        &self,
        request: OffPeakRunRequest,
    ) -> BoxFuture<'static, Result<String, DispatchFailure>>;
}

impl CronDispatch for Arc<Dispatcher> {
    fn dispatch(
        &self,
        request: CronRunRequest,
    ) -> BoxFuture<'static, Result<String, DispatchFailure>> {
        let dispatcher = self.clone();
        async move {
            // 与 `cron-run` 的处理相同：Host 派发器的失败一律是 transient，由退避重试。
            Dispatcher::dispatch(&dispatcher, &request)
                .await
                .map_err(|error| DispatchFailure {
                    error,
                    kind: FailureKind::Transient,
                })
        }
        .boxed()
    }
}

impl OffPeakDispatch for Arc<OffPeakDispatcher> {
    fn dispatch(
        &self,
        request: OffPeakRunRequest,
    ) -> BoxFuture<'static, Result<String, DispatchFailure>> {
        let dispatcher = self.clone();
        async move {
            OffPeakDispatcher::dispatch(&dispatcher, &request)
                .await
                .map_err(|error| DispatchFailure {
                    error: error.message,
                    kind: if error.permanent {
                        FailureKind::Permanent
                    } else {
                        FailureKind::Transient
                    },
                })
        }
        .boxed()
    }
}

/// 闲时任务模块没能安装（服务端客户端构建失败）时的派发器：按 transient 失败，任务留在队列退避。
pub struct OffPeakUnavailable;

impl OffPeakDispatch for OffPeakUnavailable {
    fn dispatch(
        &self,
        _request: OffPeakRunRequest,
    ) -> BoxFuture<'static, Result<String, DispatchFailure>> {
        async {
            Err(DispatchFailure {
                error: "off-peak dispatcher unavailable".into(),
                kind: FailureKind::Transient,
            })
        }
        .boxed()
    }
}

/// 执行中的闲时任务数（防休眠的判据）；只在数值变化时调用，启动后的第一次总会调用。
pub type ActiveCountPort = Arc<dyn Fn(u64) + Send + Sync>;

/// 唤醒信号：多次唤醒合并为一次；Scheduler 启动之前的唤醒并入第一轮。
///
/// 先于 Scheduler 创建，交给唤醒方（闲时任务同步）；Scheduler 启动时接过同一个信号。
#[derive(Clone, Default)]
pub struct WakeSignal(Arc<Notify>);

impl WakeSignal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn wake(&self) {
        self.0.notify_one();
    }

    async fn notified(&self) {
        self.0.notified().await;
    }

    /// 取走已到达的唤醒（Node 一轮开始时的 `tickRequested = false`）。
    fn take_pending(&self) {
        let _ = self.0.notified().now_or_never();
    }
}

/// Scheduler 依赖的仓库与端口。
pub struct SchedulerDeps {
    /// automation 仓库与下次触发时间的时区。
    pub automations: Arc<AutomationService>,
    pub off_peak: OffPeakRepo,
    pub cron: Arc<dyn CronDispatch>,
    pub off_peak_dispatch: Arc<dyn OffPeakDispatch>,
    pub active_count: ActiveCountPort,
    pub wake: WakeSignal,
}

/// 时钟与轮询间隔（测试注入，生产用 [`SchedulerOptions::default`]）。
pub struct SchedulerOptions {
    /// 墙上时钟（ms），用于到期判断与结算时间。
    pub clock: Clock,
    /// 单调时钟定时器的间隔。
    pub tick_interval: Duration,
}

impl Default for SchedulerOptions {
    fn default() -> Self {
        Self {
            clock: Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_millis() as i64)
                    .unwrap_or_default()
            }),
            tick_interval: TICK_INTERVAL,
        }
    }
}

enum Command {
    Dispose(oneshot::Sender<()>),
}

/// Scheduler 的句柄（可 clone）；所有 clone 都释放后 actor 按 dispose 收尾。
#[derive(Clone)]
pub struct SchedulerHandle {
    wake: WakeSignal,
    commands: mpsc::Sender<Command>,
    /// 已执行的轮数（测试观察轮次合并用）。
    #[cfg(test)]
    ticks: Arc<std::sync::atomic::AtomicUsize>,
}

impl SchedulerHandle {
    /// 请求尽快再执行一轮（执行中的一轮结束后立即补一轮）。
    pub fn wake(&self) {
        self.wake.wake();
    }

    #[cfg(test)]
    fn ticks(&self) -> usize {
        self.ticks.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 等当前一轮结束，释放仍在途的认领并停止；重复调用立即返回。
    pub async fn dispose(&self) {
        let (reply, done) = oneshot::channel();
        if self.commands.send(Command::Dispose(reply)).await.is_ok() {
            let _ = done.await;
        }
    }
}

pub struct Scheduler;

impl Scheduler {
    /// 启动 actor（`spawner` 作用域里的任务 `scheduler`）：回收中断的闲时任务 → 立即一轮 → 每
    /// `tick_interval` 一轮。actor 在 [`SchedulerHandle::dispose`] 之后结束；派发是它持有的结构化任务。
    pub fn start(
        spawner: &Spawner,
        deps: SchedulerDeps,
        options: SchedulerOptions,
    ) -> SchedulerHandle {
        let (commands, receiver) = mpsc::channel(4);
        let wake = deps.wake.clone();
        let actor = actor::Actor::new(deps, options.clock);
        #[cfg(test)]
        let ticks = actor.tick_counter();
        drop(spawner.spawn("scheduler", actor.run(receiver, options.tick_interval)));
        SchedulerHandle {
            wake,
            commands,
            #[cfg(test)]
            ticks,
        }
    }
}
