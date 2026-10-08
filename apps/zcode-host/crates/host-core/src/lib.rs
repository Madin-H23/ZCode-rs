//! Rust Host 的组装层（docs/specs/rust-host.md §3.1、rust-host-r0.md）。
//!
//! 接入层只向这里交付两样东西：Main 的控制消息（[`HostCore::on_control`]），以及每个 attachment
//! 一条已鉴权的 `@zcode/rpc` 连接（[`HostCore::attach`]）。services 在收到 `InitLocal` 时按
//! Main 给的路径与环境构建；构建完成前建立的 attachment 先挂起，之后才发 `[200]`。
pub mod attachments;
mod server;

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use zcode_host_control::{
    AttachmentEndpointResult, AttachmentScope, ClientMode, ControlRouter, DatabaseStartupState,
    False, HostToMain, MainToHost, True,
};
use zcode_host_kit::{CallContext, ChannelRegistry, contract, log};
use zcode_host_services::{HostEnvironment, ServiceContext};

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
            }),
        }
    }

    pub fn on_control(&self, message: MainToHost) -> ControlOutcome {
        match message {
            MainToHost::InitLocal(init) => {
                if self.inner.registry.borrow().is_some() {
                    log::warn("duplicate init-local ignored");
                    return ControlOutcome::Continue;
                }
                let startup_id = init
                    .database_startup_id
                    .clone()
                    .filter(|id| !id.trim().is_empty())
                    .unwrap_or_else(|| "rust-host".to_owned());
                let context = Arc::new(ServiceContext {
                    environment: self.inner.config.environment.clone(),
                    init,
                    control: self.inner.control.clone(),
                });
                let mut registry = ChannelRegistry::from_contract(contract::embedded());
                let mut router = ControlRouter::default();
                zcode_host_services::install(context, &mut registry, &mut router);
                *self.lock_router() = router;
                // tasks-index 等数据库的准备随对应 repo 实现；目前没有阻塞启动的迁移。
                let state = DatabaseStartupState::ready(startup_id, random_hex(16), now_ms());
                self.inner
                    .control
                    .send(HostToMain::DatabaseStartupState { state });
                self.inner.registry.send_replace(Some(Arc::new(registry)));
                log::info("init-local received; services installed");
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
        let context = CallContext {
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
