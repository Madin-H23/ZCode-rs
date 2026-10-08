//! Rust Host 的组装层（docs/specs/rust-host.md §3.1、rust-host-r0.md）。
//!
//! 接入层只向这里交付两样东西：Main 的控制消息（[`HostCore::on_control`]），以及每个 attachment
//! 一条已鉴权的 `@zcode/rpc` 连接（[`HostCore::attach`]）。services 在收到 `InitLocal` 时按
//! Main 给的路径与环境构建；构建完成前建立的 attachment 先挂起，之后才发 `[200]`。
pub mod attachments;
mod server;
mod startup;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use zcode_host_control::{
    AttachmentEndpointResult, AttachmentScope, ClientMode, ControlRouter, DatabaseStartupControl,
    False, HostToMain, InitLocal, MainToHost, True,
};
use zcode_host_kit::{CallContext, ChannelRegistry, FlowState, contract, log};
use zcode_host_services::{HostEnvironment, Installed, ServiceContext};

pub use attachments::{AttachmentMeta, AttachmentRegistry};
use startup::StartupCoordinator;
pub use zcode_host_control::ControlSink;

/// capability 有效期（rust-host-r0.md §3）。
pub const CAPABILITY_TTL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub ws_port: u16,
    pub environment: HostEnvironment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlOutcome {
    Continue,
    Shutdown,
}

#[derive(Clone)]
pub struct HostCore {
    inner: Arc<Inner>,
}

struct Inner {
    config: HostConfig,
    control: Arc<dyn ControlSink>,
    attachments: AttachmentRegistry,
    /// InitLocal 之后构建好的 channel 注册表；None 表示尚未就绪。
    registry: watch::Sender<Option<Arc<ChannelRegistry>>>,
    router: Mutex<ControlRouter>,
    installed: Mutex<Option<Arc<Installed>>>,
    initializing: AtomicBool,
    /// `init-local` 与启动状态的所有者；失败后的 retry 用同一份 `init-local` 重新执行。
    startup: Mutex<Option<(InitLocal, Arc<StartupCoordinator>)>>,
}

impl HostCore {
    pub fn new(config: HostConfig, control: Arc<dyn ControlSink>) -> Self {
        let (registry, _) = watch::channel(None);
        Self {
            inner: Arc::new(Inner {
                config,
                control,
                attachments: AttachmentRegistry::new(CAPABILITY_TTL),
                registry,
                router: Mutex::new(ControlRouter::default()),
                installed: Mutex::new(None),
                initializing: AtomicBool::new(false),
                startup: Mutex::new(None),
            }),
        }
    }

    pub fn on_control(&self, message: MainToHost) -> ControlOutcome {
        match message {
            MainToHost::InitLocal(init) => {
                if self.inner.initializing.swap(true, Ordering::SeqCst) {
                    log::warn("duplicate init-local ignored");
                    return ControlOutcome::Continue;
                }
                let core = self.clone();
                tokio::spawn(async move { core.initialize(init).await });
            }
            MainToHost::IssueAttachmentEndpoint(request) => {
                let result = self.issue_endpoint(AttachmentMeta {
                    attachment_id: request.attachment_id,
                    client_mode: request.client_mode,
                    scope: request.scope,
                });
                self.inner
                    .control
                    .send(HostToMain::AttachmentEndpointIssued {
                        request_id: request.request_id,
                        result,
                    });
            }
            MainToHost::InvalidIssueAttachmentEndpoint { request_id, error } => {
                log::warn(format!("invalid issue-attachment-endpoint: {error}"));
                self.inner
                    .control
                    .send(HostToMain::AttachmentEndpointIssued {
                        request_id,
                        result: AttachmentEndpointResult::Failed {
                            ok: False,
                            error: "invalid issue-attachment-endpoint request".to_owned(),
                        },
                    });
            }
            MainToHost::Dispose => return ControlOutcome::Shutdown,
            MainToHost::DatabaseStartupControl(control) => self.on_startup_control(control),
            MainToHost::Other { kind, message } => {
                if !self.lock_router().dispatch(&kind, message) {
                    log::debug(format!(
                        "control message not handled by the Rust Host yet: {kind}"
                    ));
                }
            }
        }
        ControlOutcome::Continue
    }

    /// Host 退出前回收子进程（CLI 进程树）；未初始化时为空操作。
    pub async fn shutdown(&self) {
        let installed = self
            .inner
            .installed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(installed) = installed {
            installed.shutdown().await;
        }
    }

    /// InitLocal 之后的启动顺序（docs/specs/rust-host-r2.md §5.1）：
    /// starting → preparing_host_storage（tasks-index 迁移）→ starting_services → ready。
    /// 迁移失败报 failed 并停在这里：已建立的 attachment 继续挂起，Renderer 显示启动失败页，
    /// 用户点 Retry 后由 [`Self::on_startup_control`] 重新执行（rust-host-resilience.md §2.2）。
    async fn initialize(&self, init: InitLocal) {
        let startup_id = init
            .database_startup_id
            .clone()
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| "rust-host".to_owned());
        let coordinator = Arc::new(StartupCoordinator::begin(
            self.inner.control.clone(),
            startup_id,
        ));
        *self.lock_startup() = Some((init, coordinator));
        self.run_startup().await;
    }

    fn on_startup_control(&self, control: DatabaseStartupControl) {
        let Some((_, coordinator)) = self.lock_startup().clone() else {
            // init-local 之前没有启动状态；Main 自己持有最新镜像。
            return;
        };
        match control {
            DatabaseStartupControl::Snapshot {} => coordinator.republish(),
            DatabaseStartupControl::Retry { attempt_id } => {
                if coordinator.try_retry(&attempt_id) {
                    log::info("database startup retry accepted");
                    let core = self.clone();
                    tokio::spawn(async move { core.run_startup().await });
                } else {
                    log::debug("database startup retry ignored");
                }
            }
            // 退出 App 由 Main 处理。
            DatabaseStartupControl::Exit {} => {}
        }
    }

    /// 执行一次启动尝试；调用方保证同一时间只有一个尝试在运行。
    async fn run_startup(&self) {
        let Some((init, reporter)) = self.lock_startup().clone() else {
            return;
        };
        reporter.send("starting", None);
        let progress = reporter.clone();
        let storage = match zcode_host_services::prepare_storage(
            &self.inner.config.environment,
            move |database_phase| progress.send("preparing_host_storage", database_phase),
        )
        .await
        {
            Ok(storage) => storage,
            Err(error) => {
                reporter.fail("preparing_host_storage", error.code().as_str());
                log::error(format!("tasks-index startup failed: {error:#}"));
                return;
            }
        };
        reporter.send("starting_services", None);
        let context = Arc::new(ServiceContext {
            environment: self.inner.config.environment.clone(),
            init,
            control: self.inner.control.clone(),
        });
        let mut registry = ChannelRegistry::from_contract(contract::embedded());
        let mut router = ControlRouter::default();
        let installed = zcode_host_services::install(context, storage, &mut registry, &mut router);
        *self.lock_router() = router;
        *self
            .inner
            .installed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::new(installed));
        reporter.send("ready", None);
        self.inner.registry.send_replace(Some(Arc::new(registry)));
        log::info("init-local received; services installed");
    }

    fn lock_startup(
        &self,
    ) -> std::sync::MutexGuard<'_, Option<(InitLocal, Arc<StartupCoordinator>)>> {
        self.inner
            .startup
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_router(&self) -> std::sync::MutexGuard<'_, ControlRouter> {
        self.inner
            .router
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn issue_endpoint(&self, meta: AttachmentMeta) -> AttachmentEndpointResult {
        // 只接 Renderer 的本地 attachment；Bot 远程运行时与远端 workspace 后续接入。
        if meta.client_mode != ClientMode::DesktopContinuous || meta.scope != AttachmentScope::Local
        {
            return AttachmentEndpointResult::Failed {
                ok: False,
                error: "only local desktop-continuous attachments are supported".to_owned(),
            };
        }
        let capability = self.inner.attachments.issue(meta);
        AttachmentEndpointResult::Issued {
            ok: True,
            url: format!("ws://127.0.0.1:{}/rpc", self.inner.config.ws_port),
            capability,
        }
    }

    /// 校验并作废 capability；返回签发时登记的 attachment 元数据。
    pub fn redeem_capability(&self, capability: &str) -> Option<AttachmentMeta> {
        self.inner.attachments.redeem(capability)
    }

    /// 为一条已鉴权的连接运行 `@zcode/rpc` 服务端。`inbound` / `outbound` 都是完整的 rpc 消息。
    pub fn attach(
        &self,
        meta: AttachmentMeta,
        inbound: mpsc::UnboundedReceiver<Vec<u8>>,
        outbound: mpsc::UnboundedSender<Vec<u8>>,
    ) -> JoinHandle<()> {
        // 不观测背压的接入方：流控状态恒为 drained。
        let (_flow, flow_rx) = watch::channel(FlowState::Drained);
        self.attach_with_flow(meta, inbound, outbound, flow_rx)
    }

    /// 同 [`Self::attach`]，并把接入层观测到的发送积压边沿（`saturated` / `drained`）交给
    /// 按连接持有状态的 channel（rust-host-resilience.md §2.3）。
    pub fn attach_with_flow(
        &self,
        meta: AttachmentMeta,
        inbound: mpsc::UnboundedReceiver<Vec<u8>>,
        outbound: mpsc::UnboundedSender<Vec<u8>>,
        flow: watch::Receiver<FlowState>,
    ) -> JoinHandle<()> {
        static NEXT_CONNECTION: AtomicU64 = AtomicU64::new(1);
        let context = CallContext {
            connection: NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed),
            attachment_id: meta.attachment_id,
            client_mode: meta.client_mode,
        };
        tokio::spawn(server::serve(
            context,
            self.inner.registry.subscribe(),
            inbound,
            outbound,
            flow,
        ))
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

/// 操作系统随机源生成的十六进制串；十六进制字符都是合法的 WebSocket 子协议 token。
pub fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    // 取随机数失败说明系统熵源不可用，此时不能签发凭据，直接终止。
    getrandom::fill(&mut buffer).expect("operating system random source unavailable");
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}
