//! Rust Host 的组装层（docs/specs/rust-host.md §3.1、rust-host-r0.md）。
//!
//! 接入层只向这里交付两样东西：Main 的控制消息（[`HostCore::on_control`]），以及每个 attachment
//! 一条已鉴权的 `@zcode/rpc` 连接（[`HostCore::attach`]）。services 在收到 `InitLocal` 时按
//! Main 给的路径与环境构建；构建完成前建立的 attachment 先挂起，之后才发 `[200]`。
pub mod attachments;
mod server;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use zcode_host_control::{
    AttachmentEndpointResult, AttachmentScope, ClientMode, ControlRouter, DatabaseStartupState,
    False, HostToMain, InitLocal, MainToHost, True,
};
use zcode_host_kit::{CallContext, ChannelRegistry, contract, log};
use zcode_host_services::{HostEnvironment, Installed, ServiceContext};

pub use attachments::{AttachmentMeta, AttachmentRegistry};
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
}

/// 把启动进度作为 `DatabaseStartupState` 发给 Main；同一次启动的 sequence 严格递增。
struct StartupReporter {
    control: Arc<dyn ControlSink>,
    startup_id: String,
    attempt_id: String,
    started_at: u64,
    sequence: AtomicU64,
    /// 会话库准备上报的迁移事实（docs/specs/rust-host-agent.md §2.11），之后的状态一直带着。
    migrations: Mutex<zcode_host_services::agent::storage_prep::MigrationFacts>,
}

impl StartupReporter {
    fn new(control: Arc<dyn ControlSink>, startup_id: String) -> Self {
        Self {
            control,
            startup_id,
            attempt_id: random_hex(16),
            started_at: now_ms(),
            sequence: AtomicU64::new(0),
            migrations: Mutex::default(),
        }
    }

    fn state(&self, phase: &'static str) -> DatabaseStartupState {
        let mut state = DatabaseStartupState::new(
            self.startup_id.clone(),
            self.attempt_id.clone(),
            self.sequence.fetch_add(1, Ordering::SeqCst),
            self.started_at,
            now_ms(),
            phase,
        );
        let migrations = self
            .migrations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.migration = migrations.aggregate();
        let baselines = migrations.baselines();
        if !baselines.is_empty() {
            state.migration_baselines = Some(baselines);
        }
        state
    }

    /// `preparing_session_storage`：None 是进入新目录（checking），Some 是该目录的数据库阶段。
    fn send_session(&self, progress: Option<zcode_host_services::agent::storage_prep::Progress>) {
        let Some(progress) = progress else {
            self.send("preparing_session_storage", Some("checking"));
            return;
        };
        self.migrations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .record(&progress.database_id, progress.migration.as_ref());
        let mut state = self.state("preparing_session_storage");
        state.database_phase = Some(progress.database_phase);
        state.current_migration = progress.migration;
        state.final_database = Some(progress.final_database);
        self.control
            .send(HostToMain::DatabaseStartupState { state });
    }

    fn fail_session(&self, error: zcode_host_services::agent::storage_prep::PrepareError) {
        if let Some((database_id, facts)) = &error.migration {
            self.migrations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .record(database_id, Some(facts));
        }
        let mut state = self.state("failed");
        state.failed_phase = Some("preparing_session_storage");
        state.error_code = Some(error.error_code);
        state.sqlite_code = error.sqlite_code;
        state.system_code = error.system_code;
        state.migration_id = error.migration_id;
        state.current_migration = error.migration.map(|(_, facts)| facts);
        self.control
            .send(HostToMain::DatabaseStartupState { state });
    }

    fn send(&self, phase: &'static str, database_phase: Option<&'static str>) {
        let mut state = self.state(phase);
        state.database_phase = database_phase;
        self.control
            .send(HostToMain::DatabaseStartupState { state });
    }

    fn fail(&self, failed_phase: &'static str, error_code: &'static str) {
        let mut state = self.state("failed");
        state.failed_phase = Some(failed_phase);
        state.error_code = Some(error_code);
        self.control
            .send(HostToMain::DatabaseStartupState { state });
    }
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
    /// 迁移失败报 failed 并停在这里：已建立的 attachment 继续挂起，Renderer 显示启动失败页。
    async fn initialize(&self, init: InitLocal) {
        let startup_id = init
            .database_startup_id
            .clone()
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| "rust-host".to_owned());
        let reporter = Arc::new(StartupReporter::new(self.inner.control.clone(), startup_id));
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
        // 会话库准备（docs/specs/rust-host-agent.md §2.11）：失败停在这里，attachment 继续挂起。
        let session_reporter = reporter.clone();
        if let Err(error) = zcode_host_services::agent::storage_prep::prepare_for_init(
            &self.inner.config.environment,
            &init,
            &move |progress| session_reporter.send_session(progress),
        )
        .await
        {
            log::error(format!("session storage startup failed: {error}"));
            reporter.fail_session(error);
            return;
        }
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
