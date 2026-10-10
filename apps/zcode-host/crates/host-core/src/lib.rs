//! Rust Host 的组装层（docs/specs/rust-host.md §3.1、rust-host-r0.md）。
//!
//! 接入层只向这里交付两样东西：Main 的控制消息（[`HostCore::on_control`]），以及每个 attachment
//! 一条已鉴权的 `@zcode/rpc` 连接（[`HostCore::connect`]，连接本身由 Gateway 承载，见
//! docs/specs/rust-u2-gateway.md）。services 在收到 `InitLocal` 时按 Main 给的路径与环境构建；
//! 构建完成前建立的 attachment 先挂起，之后才发 `[200]`。
//!
//! `HostCore` 持有根作用域 `host`（docs/specs/rust-u2-task-scope.md §4）：启动、遥测、连接循环与
//! transport 的任务都在它下面；Host 退出时关闭它。
#![deny(clippy::unwrap_used, clippy::expect_used)]
pub mod attachments;
pub mod rpc_telemetry;
mod startup;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::watch;
use zcode_async::{Spawner, TaskScope};
use zcode_host_control::{
    AttachmentEndpointResult, AttachmentScope, ControlRouter, DatabaseStartupControl, False,
    HostToMain, InitLocal, MainToHost, True,
};
use zcode_host_gateway::{ChannelSource, Channels, Connection, Gateway, OutboundLimits};
use zcode_host_kit::{ChannelRegistry, contract, log};
use zcode_host_services::{HostEnvironment, Installed, ServiceContext};

pub use attachments::{AttachmentMeta, AttachmentRegistry};
use startup::StartupCoordinator;
pub use zcode_host_control::ControlSink;

/// Main → Host：关闭某个 attachment 的连接。
const DETACH_SERVICE_PORT: &str = "detach-service-port";

/// capability 有效期（rust-host-r0.md §3）。
pub const CAPABILITY_TTL: Duration = Duration::from_secs(30);

/// Host 退出时等各连接收口、transport 发完 Close 的时限（docs/specs/rust-u2-gateway.md §2.4）。
/// 与 WebSocket transport 发 Close 的时限相同：对端不读时不让 dispose 一直等。
const CONNECTION_CLOSE_GRACE: Duration = Duration::from_secs(1);

/// service 收尾之后，关闭根作用域时等仍在运行的任务自行结束的时限（rust-u2-task-scope.md §6.4）。
/// 常驻任务随取消信号立即结束；其余任务正常应在 service 的有序收尾里已经结束，过了这个时限还在的
/// 被中止并以 warn 列出。
const TASK_SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub ws_port: u16,
    pub environment: HostEnvironment,
    /// 每条连接的出站队列阈值；产品构建总是 `OutboundLimits::default()`，调试构建的测试可以调小上限
    /// （docs/specs/rust-u2-gateway.md §2.6）。
    pub outbound_limits: OutboundLimits,
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
    /// 连接注册表、连接循环、出站队列与串行键通道（docs/specs/rust-u2-gateway.md）。
    gateway: Gateway,
    /// RPC 网络遥测的观测缓冲（rust-host-integration.md §2.3）。
    telemetry: Arc<rpc_telemetry::RpcTelemetry>,
    /// 根作用域 `host`。它的任务（连接循环、transport、启动）持有 `HostCore` 的克隆，所以作用域不会
    /// 随最后一个外部克隆释放；[`HostCore::shutdown`] 显式关闭它。
    scope: TaskScope,
    /// `host/services`：交给组装根，各 service 的作用域在它下面（rust-u2-task-scope.md §6.1）。
    services: TaskScope,
}

impl HostCore {
    pub fn new(config: HostConfig, control: Arc<dyn ControlSink>) -> Self {
        let (registry, _) = watch::channel(None);
        // 任务 panic 只结束它自己；上报进 Host 日志，带作用域路径与任务名。
        let scope = TaskScope::root("host", |panic| {
            log::error(format!(
                "task panicked: scope={} task={}: {}",
                panic.scope, panic.task, panic.message
            ));
        });
        let telemetry = rpc_telemetry::RpcTelemetry::new(control.clone(), scope.spawner());
        let gateway = Gateway::new(scope.spawner(), telemetry.clone(), config.outbound_limits);
        let services = scope.spawner().child("services");
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
                gateway,
                scope,
                services,
            }),
        }
    }

    /// 根作用域的启动权限：transport 等 Host 级任务在这里启动，随 Host 退出被中止。
    pub fn spawner(&self) -> Spawner {
        self.inner.scope.spawner()
    }

    pub fn on_control(&self, message: MainToHost) -> ControlOutcome {
        match message {
            MainToHost::InitLocal(init) => {
                if self.inner.initializing.swap(true, Ordering::SeqCst) {
                    log::warn("duplicate init-local ignored");
                    return ControlOutcome::Continue;
                }
                let core = self.clone();
                drop(
                    self.spawner()
                        .spawn("startup", async move { core.initialize(init).await }),
                );
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

    /// Host 退出：先关闭全部连接（docs/specs/rust-u2-gateway.md §2.4），再冲刷 RPC 遥测并回收子进程
    /// （CLI 进程树）；未初始化时只冲刷遥测。最后关闭根作用域（rust-u2-task-scope.md §5、§6.4）：常驻
    /// 任务随取消信号结束，其余任务至多再等 [`TASK_SHUTDOWN_GRACE`]，仍在运行的被中止并以 warn 列出；
    /// 正常退出时列表为空，记一条 info。返回被中止的任务（`<作用域路径>: <任务名>`）。
    pub async fn shutdown(&self) -> Vec<String> {
        // 先以 HostShutdown 关闭全部连接并等 transport 发出 Close，再停止 service：Renderer 收到
        // 4005 而不是 transport 任务被中止后的 1006；连接按正常路径收口时 service 还在。
        self.inner.gateway.shutdown(CONNECTION_CLOSE_GRACE).await;
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
        let aborted = self.inner.scope.close(TASK_SHUTDOWN_GRACE).await;
        if aborted.is_empty() {
            log::info("host tasks at shutdown: none left running");
        } else {
            log::warn(format!(
                "host tasks still running {TASK_SHUTDOWN_GRACE:?} after the services shut down; aborting: {aborted:?}"
            ));
        }
        aborted
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
                    drop(
                        self.spawner()
                            .spawn("startup", async move { core.run_startup().await }),
                    );
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
        let installed = zcode_host_services::install(
            &self.inner.services.spawner(),
            context,
            storage,
            &mut registry,
            &mut router,
        );
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

    /// `detach-service-port`：关闭该 attachmentId 的全部连接（本地与远端 scope 同样适用）。
    pub fn detach(&self, attachment_id: &str) {
        let closed = self.inner.gateway.close_attachment(attachment_id);
        log::info(format!(
            "detached service port, attachmentId={attachment_id}, connections={closed}"
        ));
    }

    /// 校验并作废 capability；返回签发时登记的 attachment 元数据。
    pub fn redeem_capability(&self, capability: &str) -> Option<AttachmentMeta> {
        self.inner.attachments.redeem(capability)
    }

    /// 接入一条已鉴权的连接（docs/specs/rust-u2-gateway.md §2.1）。
    ///
    /// 本地 scope 等 InitLocal 之后的 channel 表；远端 scope 先等对应 logical session 的 workspace
    /// 就绪，再用它的 scoped channel 表。解析失败时关闭连接、不发 `[200]`，绝不回落到本地服务。
    pub fn connect(&self, meta: AttachmentMeta) -> Connection {
        let source = self.channel_source(&meta);
        self.inner.gateway.connect(meta, source)
    }

    fn channel_source(&self, meta: &AttachmentMeta) -> ChannelSource {
        let mut ready = self.inner.registry.subscribe();
        let Some(scope) = remote_scope(&meta.scope) else {
            return Box::pin(async move {
                let registry = ready.wait_for(Option::is_some).await.ok()?.clone()?;
                Some(Channels {
                    registry,
                    closed: None,
                })
            });
        };
        let (core, attachment_id, client_mode) =
            (self.clone(), meta.attachment_id.clone(), meta.client_mode);
        Box::pin(async move {
            let local = ready.wait_for(Option::is_some).await.ok()?.clone()?;
            let installed = core.installed()?;
            match installed
                .remote()
                .attach(&attachment_id, client_mode, &scope, &local)
                .await
            {
                Ok(attachment) => Some(Channels {
                    registry: attachment.registry,
                    closed: Some(attachment.closed),
                }),
                Err(error) => {
                    // 跨 logical session 或旧 identity 的连接若继续暴露，会把远端请求路由到错误 source。
                    log::warn(format!(
                        "failed to attach scoped service port, attachmentId={attachment_id}: {error}"
                    ));
                    None
                }
            }
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
                outbound_limits: OutboundLimits::default(),
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

    /// 本地 scope 的连接在 Host 就绪前挂起；detach 照样能关闭它们，同一 attachment 的连接全部关闭，
    /// 其他 attachment 不受影响。
    #[tokio::test]
    async fn detach_closes_every_connection_of_the_attachment() {
        let (core, _) = core();
        let local = |attachment_id: &str| AttachmentMeta {
            attachment_id: attachment_id.into(),
            ..meta(AttachmentScope::Local, ClientMode::DesktopContinuous)
        };
        let first = core.connect(local("a1"));
        let second = core.connect(local("a1"));
        let other = core.connect(local("a2"));
        core.detach("a1");
        for connection in [&first, &second] {
            let ended = tokio::time::timeout(Duration::from_secs(5), connection.outbound.next())
                .await
                .expect("detached connection closes");
            assert_eq!(ended, None);
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(50), other.outbound.next())
                .await
                .is_err(),
            "the other attachment still waits for the Host to be ready"
        );
    }
}
