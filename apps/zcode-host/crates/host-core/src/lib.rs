//! Rust Host 的组装层（docs/specs/rust-host.md §3.1、rust-host-r0.md）。
//!
//! 接入层只向这里交付两样东西：Main 的控制消息（[`HostCore::on_control`]），以及每个 attachment
//! 一条已鉴权的 `@zcode/rpc` 连接（[`HostCore::attach`]）。services 在收到 `InitLocal` 时按
//! Main 给的路径与环境构建；构建完成前建立的 attachment 先挂起，之后才发 `[200]`。
pub mod attachments;
mod server;

use std::collections::HashMap;
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

/// Main → Host：关闭某个 attachment 的连接。
const DETACH_SERVICE_PORT: &str = "detach-service-port";

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
    /// attachmentId → 该 attachment 上各条连接的关闭信号（`detach-service-port`、远端断线）。
    connections: Mutex<HashMap<String, Vec<watch::Sender<bool>>>>,
}

/// 把启动进度作为 `DatabaseStartupState` 发给 Main；同一次启动的 sequence 严格递增。
struct StartupReporter {
    control: Arc<dyn ControlSink>,
    startup_id: String,
    attempt_id: String,
    started_at: u64,
    sequence: AtomicU64,
}

impl StartupReporter {
    fn new(control: Arc<dyn ControlSink>, startup_id: String) -> Self {
        Self {
            control,
            startup_id,
            attempt_id: random_hex(16),
            started_at: now_ms(),
            sequence: AtomicU64::new(0),
        }
    }

    fn state(&self, phase: &'static str) -> DatabaseStartupState {
        DatabaseStartupState::new(
            self.startup_id.clone(),
            self.attempt_id.clone(),
            self.sequence.fetch_add(1, Ordering::SeqCst),
            self.started_at,
            now_ms(),
            phase,
        )
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
                connections: Mutex::new(HashMap::new()),
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
            MainToHost::Other { kind, message } if kind == DETACH_SERVICE_PORT => {
                if let Some(attachment_id) = message["attachmentId"].as_str() {
                    self.detach(attachment_id);
                }
            }
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
        let failed = |error: String| AttachmentEndpointResult::Failed { ok: False, error };
        match &meta.scope {
            // 本地 attachment 只接 Renderer；Bot 远程运行时后续接入。
            AttachmentScope::Local if meta.client_mode != ClientMode::DesktopContinuous => {
                return failed("only local desktop-continuous attachments are supported".into());
            }
            AttachmentScope::Local => {}
            // 远端 scope：logical session 必须存在且 path、identity 完全相同（rust-host-remote.md §2.4）。
            AttachmentScope::Remote { .. } => {
                let Some(scope) = remote_scope(&meta.scope) else {
                    return failed("invalid remote attachment scope".into());
                };
                let installed = self.installed();
                let Some(installed) = installed else {
                    return failed("Rust Host services are not initialized".into());
                };
                if let Err(error) = installed.remote().validate_scope(&scope) {
                    return failed(error);
                }
            }
        }
        let capability = self.inner.attachments.issue(meta);
        AttachmentEndpointResult::Issued {
            ok: True,
            url: format!("ws://127.0.0.1:{}/rpc", self.inner.config.ws_port),
            capability,
        }
    }

    fn installed(&self) -> Option<Arc<Installed>> {
        self.inner
            .installed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn lock_connections(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, Vec<watch::Sender<bool>>>> {
        self.inner
            .connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `detach-service-port`：关闭该 attachmentId 的全部连接（本地与远端 scope 同样适用）。
    pub fn detach(&self, attachment_id: &str) {
        let closers = self.lock_connections().remove(attachment_id);
        for closer in closers.into_iter().flatten() {
            closer.send_replace(true);
        }
        log::info(format!(
            "detached service port, attachmentId={attachment_id}"
        ));
    }

    /// 校验并作废 capability；返回签发时登记的 attachment 元数据。
    pub fn redeem_capability(&self, capability: &str) -> Option<AttachmentMeta> {
        self.inner.attachments.redeem(capability)
    }

    /// 为一条已鉴权的连接运行 `@zcode/rpc` 服务端。`inbound` / `outbound` 都是完整的 rpc 消息。
    ///
    /// 远端 scope 先等对应 logical session 的 workspace 就绪，再用它的 scoped channel 表；解析失败时
    /// 关闭连接、不发 `[200]`，绝不回落到本地服务。
    pub fn attach(
        &self,
        meta: AttachmentMeta,
        inbound: mpsc::UnboundedReceiver<Vec<u8>>,
        outbound: mpsc::UnboundedSender<Vec<u8>>,
    ) -> JoinHandle<()> {
        static NEXT_CONNECTION: AtomicU64 = AtomicU64::new(1);
        let context = CallContext {
            connection: NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed),
            attachment_id: meta.attachment_id.clone(),
            client_mode: meta.client_mode,
        };
        let (close_tx, close_rx) = watch::channel(false);
        {
            let mut connections = self.lock_connections();
            let closers = connections.entry(meta.attachment_id.clone()).or_default();
            closers.retain(|closer| !closer.is_closed());
            closers.push(close_tx);
        }
        let Some(scope) = remote_scope(&meta.scope) else {
            let inbound = closable(inbound, vec![close_rx]);
            return tokio::spawn(server::serve(
                context,
                self.inner.registry.subscribe(),
                inbound,
                outbound,
            ));
        };
        let core = self.clone();
        tokio::spawn(async move {
            let mut ready = core.inner.registry.subscribe();
            let local = match ready.wait_for(Option::is_some).await {
                Ok(registry) => registry.clone(),
                Err(_) => return,
            };
            let (Some(local), Some(installed)) = (local, core.installed()) else {
                return;
            };
            let attachment = match installed
                .remote()
                .attach(&meta.attachment_id, meta.client_mode, &scope, &local)
                .await
            {
                Ok(attachment) => attachment,
                Err(error) => {
                    // 跨 logical session 或旧 identity 的连接若继续暴露，会把远端请求路由到错误 source。
                    log::warn(format!(
                        "failed to attach scoped service port, attachmentId={}: {error}",
                        meta.attachment_id
                    ));
                    return;
                }
            };
            let (_, registry) = watch::channel(Some(attachment.registry));
            let inbound = closable(inbound, vec![close_rx, attachment.closed]);
            server::serve(context, registry, inbound, outbound).await;
        })
    }
}

/// `AttachmentScope::Remote` → 远端模块的 scope。
fn remote_scope(scope: &AttachmentScope) -> Option<zcode_host_services::remote::RemoteScope> {
    match scope {
        AttachmentScope::Local => None,
        AttachmentScope::Remote {
            remote_session_id,
            workspace_path,
            workspace_identity,
        } => Some(zcode_host_services::remote::RemoteScope {
            remote_session_id: remote_session_id.clone(),
            workspace_path: workspace_path.clone(),
            workspace_identity: workspace_identity.clone(),
        }),
    }
}

/// 转发入站消息，直到任一关闭信号置 true：此时丢弃转发端，服务端循环随之走正常的连接关闭清理。
fn closable(
    mut inbound: mpsc::UnboundedReceiver<Vec<u8>>,
    mut signals: Vec<watch::Receiver<bool>>,
) -> mpsc::UnboundedReceiver<Vec<u8>> {
    let (sender, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let closed = async {
            let waits = signals.iter_mut().map(|signal| {
                Box::pin(async move {
                    if signal.wait_for(|closed| *closed).await.is_err() {
                        std::future::pending::<()>().await;
                    }
                })
            });
            futures_util::future::select_all(waits).await;
        };
        tokio::pin!(closed);
        loop {
            tokio::select! {
                message = inbound.recv() => match message {
                    Some(message) => {
                        if sender.send(message).is_err() {
                            return;
                        }
                    }
                    None => return,
                },
                _ = &mut closed => return,
            }
        }
    });
    receiver
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
