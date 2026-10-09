//! Rust Host 的组装层（docs/specs/rust-host.md §3.1、rust-host-r0.md）。
//!
//! 接入层只向这里交付两样东西：Main 的控制消息（[`HostCore::on_control`]），以及每个 attachment
//! 一条已鉴权的 `@zcode/rpc` 连接（[`HostCore::attach`]）。services 在收到 `InitLocal` 时按
//! Main 给的路径与环境构建；构建完成前建立的 attachment 先挂起，之后才发 `[200]`。
#![deny(clippy::unwrap_used, clippy::expect_used)]
pub mod attachments;
pub mod rpc_telemetry;
mod server;
mod startup;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use zcode_host_control::{
    AttachmentEndpointResult, AttachmentScope, ControlRouter, DatabaseStartupControl, False,
    HostToMain, InitLocal, MainToHost, True,
};
use zcode_host_kit::{CallContext, ChannelRegistry, FlowState, contract, log};
use zcode_host_services::{HostEnvironment, Installed, ServiceContext};

pub use attachments::{AttachmentMeta, AttachmentRegistry};
use startup::StartupCoordinator;
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
    /// `init-local` 与启动状态的所有者；失败后的 retry 用同一份 `init-local` 重新执行。
    startup: Mutex<Option<(InitLocal, Arc<StartupCoordinator>)>>,
    /// attachmentId → 该 attachment 上各条连接的关闭信号（`detach-service-port`、远端断线）。
    connections: Mutex<HashMap<String, Vec<watch::Sender<bool>>>>,
    /// RPC 网络遥测的观测缓冲（rust-host-integration.md §2.3）。
    telemetry: Arc<rpc_telemetry::RpcTelemetry>,
}

impl HostCore {
    pub fn new(config: HostConfig, control: Arc<dyn ControlSink>) -> Self {
        let (registry, _) = watch::channel(None);
        let telemetry = rpc_telemetry::RpcTelemetry::new(control.clone());
        Self {
            inner: Arc::new(Inner {
                config,
                control,
                telemetry,
                attachments: AttachmentRegistry::new(CAPABILITY_TTL),
                registry,
                router: Mutex::new(ControlRouter::default()),
                installed: Mutex::new(None),
                initializing: AtomicBool::new(false),
                startup: Mutex::new(None),
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
                    window_id: request.window_id,
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
            MainToHost::Other { kind, message } if kind == DETACH_SERVICE_PORT => {
                if let Some(attachment_id) = message["attachmentId"].as_str() {
                    self.detach(attachment_id);
                }
            }
            MainToHost::Other { kind, message } => {
                // 路由按值分发：先判断是否有处理者，没有时还要用原消息回失败。
                if !self.lock_router().dispatch(&kind, message.clone()) {
                    self.reject_unrouted(&kind, &message);
                }
            }
        }
        ControlOutcome::Continue
    }

    /// 没有处理者的控制消息。服务安装之前到达的 `setting-write` 直接回失败：Main 应在 Host 就绪后才写
    /// （rust-u4-app-host.md §2.6），万一早到也不能让它等满超时。
    fn reject_unrouted(&self, kind: &str, message: &serde_json::Value) {
        if kind == zcode_host_services::settings_control::SETTING_WRITE
            && let Some(request_id) = message["requestId"].as_str()
        {
            log::warn(format!(
                "setting-write before services are installed, requestId={request_id}"
            ));
            self.inner.control.send(HostToMain::Raw(serde_json::json!({
                "type": zcode_host_services::settings_control::SETTING_WRITE_RESULT,
                "requestId": request_id,
                "ok": false,
                "error": "Rust Host services are not initialized",
            })));
            return;
        }
        log::debug(format!(
            "control message not handled by the Rust Host yet: {kind}"
        ));
    }

    /// Host 退出前冲刷 RPC 遥测并回收子进程（CLI 进程树）；未初始化时只冲刷遥测。
    pub async fn shutdown(&self) {
        self.inner.telemetry.shutdown();
        let installed = self
            .inner
            .installed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(installed));
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
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_router(&self) -> std::sync::MutexGuard<'_, ControlRouter> {
        self.inner
            .router
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn issue_endpoint(&self, meta: AttachmentMeta) -> AttachmentEndpointResult {
        let failed = |error: String| AttachmentEndpointResult::Failed { ok: False, error };
        match &meta.scope {
            // 本地 scope 的 desktop-continuous 与 web-remote-replayable 都可签发（rust-host-tasks.md §2.4）；
            // 两者的差别（deliveryProfile、capabilities、ttft）由 connection scope 按 clientMode 处理。
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
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn lock_connections(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, Vec<watch::Sender<bool>>>> {
        self.inner
            .connections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
            attachment_id: meta.attachment_id.clone(),
            client_mode: meta.client_mode,
            window_id: meta.window_id,
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
                flow,
                self.inner.telemetry.clone(),
            ));
        };
        let telemetry = self.inner.telemetry.clone();
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
            server::serve(context, registry, inbound, outbound, flow, telemetry).await;
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
#[expect(clippy::expect_used, reason = "系统熵源不可用时不能签发凭据，只能终止")]
pub fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    // 取随机数失败说明系统熵源不可用，此时不能签发凭据，直接终止。
    getrandom::fill(&mut buffer).expect("operating system random source unavailable");
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod remote_scope_tests {
    use super::*;
    use std::time::Duration;
    use zcode_host_control::ClientMode;

    struct Sink(Mutex<Vec<HostToMain>>);

    impl ControlSink for Sink {
        fn send(&self, message: HostToMain) {
            self.0.lock().unwrap().push(message);
        }
    }

    fn core() -> (HostCore, Arc<Sink>) {
        let sink = Arc::new(Sink(Mutex::new(Vec::new())));
        let core = HostCore::new(
            HostConfig {
                ws_port: 4242,
                environment: zcode_host_services::HostEnvironment::default(),
            },
            sink.clone(),
        );
        (core, sink)
    }

    fn meta(scope: AttachmentScope, client_mode: ClientMode) -> AttachmentMeta {
        AttachmentMeta {
            attachment_id: "a1".into(),
            client_mode,
            scope,
            window_id: None,
        }
    }

    #[test]
    fn remote_scopes_need_an_initialized_host_and_a_known_session() {
        let (core, _) = core();
        let remote = AttachmentScope::Remote {
            remote_session_id: "s1".into(),
            workspace_path: "/w".into(),
            workspace_identity: "remote:docker:c:/w".into(),
        };
        assert!(matches!(
            core.issue_endpoint(meta(remote, ClientMode::WebRemoteReplayable)),
            AttachmentEndpointResult::Failed { error, .. } if error == "Rust Host services are not initialized"
        ));
        assert!(matches!(
            core.issue_endpoint(meta(AttachmentScope::Local, ClientMode::DesktopContinuous)),
            AttachmentEndpointResult::Issued { url, .. } if url == "ws://127.0.0.1:4242/rpc"
        ));
    }

    #[test]
    fn issued_capability_carries_the_window_id() {
        let (core, sink) = core();
        let window_id = zcode_host_control::WindowId::new(2);
        core.on_control(MainToHost::IssueAttachmentEndpoint(
            zcode_host_control::IssueAttachmentEndpoint {
                kind: "issue-attachment-endpoint".into(),
                request_id: "r1".into(),
                attachment_id: "a1".into(),
                client_mode: ClientMode::DesktopContinuous,
                scope: AttachmentScope::Local,
                window_id,
            },
        ));
        let capability = match sink.0.lock().unwrap().last() {
            Some(HostToMain::AttachmentEndpointIssued {
                result: AttachmentEndpointResult::Issued { capability, .. },
                ..
            }) => capability.clone(),
            other => panic!("expected an issued endpoint, got {other:?}"),
        };
        let meta = core.redeem_capability(&capability).unwrap();
        assert_eq!(meta.window_id, window_id);
    }

    #[test]
    fn setting_writes_before_install_fail_instead_of_hanging() {
        let (core, sink) = core();
        core.on_control(MainToHost::Other {
            kind: "setting-write".into(),
            message: serde_json::json!({"type": "setting-write", "requestId": "w1", "patch": {}}),
        });
        let sent = sink.0.lock().unwrap().clone();
        assert_eq!(
            sent,
            vec![HostToMain::Raw(serde_json::json!({
                "type": "setting-write-result",
                "requestId": "w1",
                "ok": false,
                "error": "Rust Host services are not initialized",
            }))]
        );
    }

    #[tokio::test]
    async fn closing_signals_end_the_forwarded_inbound_stream() {
        let (to_core, inbound) = mpsc::unbounded_channel();
        let (close, closed) = watch::channel(false);
        let (_other, other) = watch::channel(false);
        let mut forwarded = closable(inbound, vec![other, closed]);
        to_core.send(vec![1]).unwrap();
        assert_eq!(forwarded.recv().await, Some(vec![1]));
        close.send_replace(true);
        let ended = tokio::time::timeout(Duration::from_secs(1), forwarded.recv())
            .await
            .expect("forwarder stops");
        assert_eq!(ended, None);
    }

    #[test]
    fn detach_closes_every_connection_of_the_attachment() {
        let (core, _) = core();
        let (first, mut first_rx) = watch::channel(false);
        let (second, mut second_rx) = watch::channel(false);
        core.lock_connections()
            .insert("a1".into(), vec![first, second]);
        core.detach("a1");
        assert!(*first_rx.borrow_and_update());
        assert!(*second_rx.borrow_and_update());
        assert!(core.lock_connections().is_empty());
    }
}
