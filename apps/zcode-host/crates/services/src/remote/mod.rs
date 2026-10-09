//! 远端 workspace（docs/specs/rust-host-remote.md）：窗口级连接注册表、远端 `@zcode/rpc` 连接、
//! scoped channel 表、window-controller 远端 source 与远端媒体预览代理。
//!
//! [`RemoteWorkspaces`] 是本模块对 Host 其余部分的唯一入口：
//! - Main 的远端控制消息（connect / cancel / bind / dispose / provider-provisioning-execute）；
//! - host-core 为 `scope.kind = "remote"` 的 attachment 解析 scoped channel 表并登记断开租约；
//! - Bot 远端运行时（[`bots::RemoteBotRuntime`]，§2.9）；
//! - Host 退出时释放全部远端连接。
pub mod agent_scope;
pub mod bots;
pub mod media;
pub mod provisioning;
pub mod registry;
pub mod scope;
pub mod sources;
pub mod target;
pub mod tasks;
pub mod transport;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio::sync::watch;
use tokio::task::AbortHandle;
use zcode_host_control::{ClientMode, ControlRouter, ControlSink, HostToMain, LogLevel};
use zcode_host_kit::{ChannelRegistry, RpcValue, log};

use crate::settings::SettingStore;
use crate::tasks::realtime::TaskRealtimeBridge;
use agent_scope::RemoteAgentChannel;
use provisioning::ProvisioningSource;
use registry::{
    ConnectParams as RegistryConnectParams, ConnectRequest, RegistryHandle, RegistryOptions,
    RemoteRegistry, SessionClosed, WorkspaceContext,
};
pub use registry::{RemoteScope, SessionSnapshot};
use scope::ForwardChannel;
use target::{RemoteTarget, TargetKind};
use transport::{CloseEvent, ConnectParams, Connection, Transport};

/// 一条已建立的远端连接及其上的 Host 侧常驻对象。
pub struct RemoteHandle {
    pub connection: Arc<Connection>,
    pub target: RemoteTarget,
    /// 每条 attachment 连接一个 scope，按 `CallContext.connection` 区分。
    pub agent: Arc<RemoteAgentChannel>,
    pub tracker: Arc<tasks::TaskTracker>,
    /// zcode-task wrapper（附件物化、运行中任务、realtime 镜像），attachment 与 Bot 共用。
    pub tasks: Arc<tasks::RemoteTaskOps>,
    /// 远端 attachment 的 `bots` channel 背后的 Bots 实例（第一次 attach 时建，§2.9.8）。
    bots: OnceLock<Arc<crate::bots::service::Bots>>,
    background: Vec<AbortHandle>,
}

impl RegistryHandle for RemoteHandle {
    fn closed(&self) -> BoxFuture<'static, CloseEvent> {
        let connection = self.connection.clone();
        Box::pin(async move { connection.closed().await })
    }

    fn dispose(&self) -> BoxFuture<'static, ()> {
        for task in &self.background {
            task.abort();
        }
        self.tracker.dispose();
        let connection = self.connection.clone();
        let bots = self.bots.get().cloned();
        Box::pin(async move {
            if let Some(bots) = bots {
                bots.shutdown().await;
            }
            connection.dispose().await
        })
    }
}

pub struct RemoteConfig {
    pub transport: Arc<dyn Transport>,
    pub settings: SettingStore,
    pub control: Arc<dyn ControlSink>,
    pub provisioning: ProvisioningSource,
    /// `ZCODE_REMOTE_MEDIA_RANGE_PREVIEW_ENABLED !== "0"`。
    pub media_range_enabled: bool,
    pub wsl_idle_ttl: Duration,
    /// tasks 区域的 task realtime bridge（InitLocal 带 hostId 时才有），远端 sendPrompt 镜像用。
    pub realtime: Option<Arc<TaskRealtimeBridge>>,
}

/// host-core 拿到的远端 attachment：channel 表与断开信号（session 断线、释放或换代时置 true）。
pub struct RemoteAttachment {
    pub registry: Arc<ChannelRegistry>,
    pub closed: watch::Receiver<bool>,
}

struct Lease {
    attachment_id: String,
    remote_session_id: String,
    generation: u64,
    close: watch::Sender<bool>,
}

pub struct RemoteWorkspaces {
    registry: RemoteRegistry<RemoteHandle>,
    sources: Arc<sources::RemoteSources>,
    control: Arc<dyn ControlSink>,
    settings: SettingStore,
    provisioning: ProvisioningSource,
    media_range_enabled: bool,
    leases: Mutex<Vec<Lease>>,
    /// Host 生命周期内的后台任务（Provisioning 触发监听），退出时结束。
    background: Mutex<Vec<AbortHandle>>,
    /// requestId → 连接日志是否仍在上报（连接 settle 后停止，同 `remoteConnectionProgressContext`）。
    progress: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    /// Bot 远端运行时（注入本地 Bots 的 `RemoteBots`）。
    bots_runtime: Arc<bots::RemoteBotRuntime>,
    /// 本地 Bots（automation 区域安装后绑定）；远端 attachment 的 Bots 实例从它派生。
    local_bots: OnceLock<Arc<crate::bots::service::Bots>>,
}

fn raw(message: Value) -> HostToMain {
    HostToMain::Raw(message)
}

fn level_str(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
    }
}

fn text(message: &Value, key: &str) -> Option<String> {
    message[key]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
}

impl RemoteWorkspaces {
    pub fn new(config: RemoteConfig) -> Arc<Self> {
        Arc::new_cyclic(|weak: &std::sync::Weak<Self>| {
            let progress: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> = Arc::default();
            let transport = config.transport.clone();
            let settings = config.settings.clone();
            let control = config.control.clone();
            let progress_for_connect = progress.clone();
            let realtime = config.realtime.clone();
            let owner = weak.clone();
            let mut options = RegistryOptions::new(Arc::new(move |request: ConnectRequest| {
                let (transport, settings, control, progress, owner) = (
                    transport.clone(),
                    settings.clone(),
                    control.clone(),
                    progress_for_connect.clone(),
                    owner.clone(),
                );
                let realtime = realtime.clone();
                Box::pin(async move {
                    connect_handle(
                        transport, settings, control, progress, owner, realtime, request,
                    )
                    .await
                }) as BoxFuture<'static, _>
            }));
            options.release_workspace = Some(Arc::new(|handle: Arc<RemoteHandle>, context| {
                Box::pin(async move {
                    let mut params = Map::new();
                    params.insert("workspacePath".into(), json!(context.workspace_path));
                    if let Some(identity) = &context.workspace_identity {
                        params.insert("workspaceIdentity".into(), json!(identity));
                    }
                    params.insert("provider".into(), json!("glm"));
                    handle
                        .connection
                        .client
                        .call(
                            "zcode-task",
                            "releaseWorkspacePreparation",
                            vec![RpcValue::Json(Value::Object(params))],
                        )
                        .await
                        .map(|_| {
                            log::info(format!(
                                "released WSL workspace runtime, workspaceKey={}",
                                context.key()
                            ))
                        })
                        .map_err(|error| error.message)
                }) as BoxFuture<'static, _>
            }));
            options.on_release_error = Arc::new(|context: &WorkspaceContext, error: &str| {
                log::warn(format!(
                    "failed to release WSL workspace runtime, workspaceKey={}: {error}",
                    context.key()
                ))
            });
            let closed = weak.clone();
            options.on_session_closed = Arc::new(move |event: SessionClosed| {
                if let Some(remote) = closed.upgrade() {
                    remote.on_session_closed(event);
                }
            });
            options.wsl_idle_ttl = config.wsl_idle_ttl;
            let registry = RemoteRegistry::new(options);
            let bots_runtime = bots::RemoteBotRuntime::new(
                weak.clone(),
                config.settings.clone(),
                config.provisioning.credentials.clone(),
                crate::main_bridge::MainBridge::new(config.control.clone()),
            );
            Self {
                sources: sources::RemoteSources::new(registry.clone()),
                registry,
                control: config.control,
                settings: config.settings,
                provisioning: config.provisioning,
                media_range_enabled: config.media_range_enabled,
                leases: Mutex::new(Vec::new()),
                background: Mutex::new(Vec::new()),
                progress,
                bots_runtime,
                local_bots: OnceLock::new(),
            }
        })
    }

    pub fn registry(&self) -> &RemoteRegistry<RemoteHandle> {
        &self.registry
    }

    /// 注入本地 Bots 的远端运行时（`bots::install` 经 `automation::Area::with_remote_bots`）。
    pub fn bots_runtime(&self) -> Arc<bots::RemoteBotRuntime> {
        self.bots_runtime.clone()
    }

    /// automation 区域装好本地 Bots 后绑定；之后的远端 attachment 才有按连接的 Bots 实例。
    pub fn bind_bots(&self, local: Arc<crate::bots::service::Bots>) {
        let _ = self.local_bots.set(local);
    }

    /// 远端 attachment 的 Bots 实例：每条连接一个，第一次需要时从本地 Bots 派生。
    fn connection_bots(&self, handle: &RemoteHandle) -> Option<Arc<crate::bots::service::Bots>> {
        let local = self.local_bots.get()?;
        Some(
            handle
                .bots
                .get_or_init(|| bots::connection_bots(local, handle))
                .clone(),
        )
    }

    /// window-controller 的远端 source（测试与 install 用）。
    pub fn sources(&self) -> &Arc<sources::RemoteSources> {
        &self.sources
    }

    fn send(&self, message: Value) {
        self.control.send(raw(message));
    }

    fn log_topology(&self, reason: &str) {
        let (connections, sessions) = self.registry.stats();
        let attachments = self.lock_leases().len();
        log::info(format!(
            "window Host topology, reason={reason}, pid={}, connections={connections}, logicalSessions={sessions}, attachments={attachments}",
            std::process::id()
        ));
    }

    fn lock_leases(&self) -> std::sync::MutexGuard<'_, Vec<Lease>> {
        self.leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 关闭满足条件的远端 attachment（`detachRemoteSessionAttachments` 等）。
    fn close_leases(&self, matches: impl Fn(&Lease) -> bool) {
        let mut leases = self.lock_leases();
        leases.retain(|lease| {
            if matches(lease) {
                lease.close.send_replace(true);
                return false;
            }
            !lease.close.is_closed()
        });
    }

    fn on_session_closed(&self, event: SessionClosed) {
        // logical session 离线时 attachment 仍持有旧 channel 与订阅：在失效源头统一关闭所有 clientMode。
        let id = event.remote_session_id.clone();
        self.close_leases(|lease| lease.remote_session_id == id);
        if let Some(scope) = self
            .registry
            .get_session(&id)
            .and_then(|session| session.scope())
        {
            self.sources.disconnect_source(&scope);
        }
        let mut message = json!({
            "type": "remote-workspace-closed",
            "remoteSessionId": id,
            "reason": "connection-closed",
            "exitCode": event.event.exit_code,
            "signal": Value::Null,
        });
        if let Some(error) = event.event.error.filter(|error| !error.is_empty()) {
            message["error"] = json!(error);
        }
        self.send(message);
        self.log_topology("remote-connection-closed");
    }

    /// 登记 Main → Host 的远端控制消息。
    pub fn register_control(self: &Arc<Self>, router: &mut ControlRouter) {
        self.bots_runtime.register(router);
        let remote = self.clone();
        router.on("connect-remote-workspace", move |message| {
            remote.clone().on_connect(message)
        });
        let remote = self.clone();
        router.on("cancel-remote-workspace-connect", move |message| {
            if let Some(request_id) = text(&message, "requestId") {
                remote.registry.cancel_connect(&request_id);
            }
        });
        let remote = self.clone();
        router.on("bind-remote-workspace-context", move |message| {
            remote.on_bind(message)
        });
        let remote = self.clone();
        router.on("dispose-remote-workspace-session", move |message| {
            let remote = remote.clone();
            tokio::spawn(async move { remote.on_dispose(message).await });
        });
        let remote = self.clone();
        router.on("provider-provisioning-execute", move |message| {
            let remote = remote.clone();
            tokio::spawn(async move { remote.on_provisioning(message).await });
        });
    }

    fn on_connect(self: Arc<Self>, message: Value) {
        let Some(request_id) = text(&message, "requestId") else {
            log::warn("connect-remote-workspace without requestId");
            return;
        };
        let target = match RemoteTarget::parse(&message["target"]) {
            Ok(target) => target,
            Err(error) => {
                self.send(json!({"type": "remote-workspace-connect-failed", "requestId": request_id, "error": error}));
                return;
            }
        };
        let workspace_path = text(&message, "workspacePath").unwrap_or_else(|| "/".into());
        let workspace_identity = text(&message, "workspaceIdentity")
            .unwrap_or_else(|| target.workspace_identity(&workspace_path));
        log::info(format!(
            "connecting window-scoped remote source, requestId={request_id}, target={}",
            target.log_label()
        ));
        let active = Arc::new(AtomicBool::new(true));
        self.progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(request_id.clone(), active.clone());
        tokio::spawn(async move {
            let result = self
                .registry
                .connect(RegistryConnectParams {
                    request_id: request_id.clone(),
                    target,
                    remote_assets: message["remoteAssets"].clone(),
                    workspace_path: Some(workspace_path),
                    workspace_identity: Some(workspace_identity),
                })
                .await;
            active.store(false, Ordering::SeqCst);
            self.progress
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&request_id);
            match result {
                Ok(descriptor) => {
                    self.replace_offline_sessions(&descriptor).await;
                    self.send(json!({
                        "type": "remote-workspace-connected",
                        "requestId": request_id,
                        "descriptor": descriptor.descriptor(),
                    }));
                    self.log_topology("remote-connected");
                }
                Err(error) => {
                    let error = if error.trim().is_empty() {
                        "远程连接失败".to_owned()
                    } else {
                        error
                    };
                    self.send(json!({"type": "remote-workspace-connect-failed", "requestId": request_id, "error": error}));
                }
            }
        });
    }

    /// 重连成功后替换同 path + identity 的离线旧 session（controller 原子替换由 sources 负责）。
    async fn replace_offline_sessions(&self, descriptor: &SessionSnapshot) {
        let replaced: Vec<SessionSnapshot> = self
            .registry
            .list_sessions()
            .into_iter()
            .filter(|session| {
                session.remote_session_id != descriptor.remote_session_id
                    && session.state == registry::ConnState::Disconnected
                    && session.workspace_path == descriptor.workspace_path
                    && session.workspace_identity == descriptor.workspace_identity
            })
            .collect();
        for session in replaced {
            let id = session.remote_session_id.clone();
            if let (Some(previous), Some(next)) = (session.scope(), descriptor.scope()) {
                self.sources
                    .replace_disconnected_source(&previous, &next)
                    .await;
            }
            self.close_leases(|lease| lease.remote_session_id == id);
            self.registry.dispose_session(&id).await;
            // 旧 remoteSessionId 已不可 attachment：同步清理 Main 的端口关联，但不伪报一次 transport 失败。
            self.send(json!({"type": "remote-workspace-closed", "remoteSessionId": id, "reason": "disposed"}));
        }
    }

    fn on_bind(&self, message: Value) {
        let previous = text(&message, "remoteSessionId")
            .and_then(|id| self.registry.get_session(&id))
            .and_then(|session| session.scope());
        let (Some(id), Some(path), Some(identity)) = (
            text(&message, "remoteSessionId"),
            text(&message, "workspacePath"),
            text(&message, "workspaceIdentity"),
        ) else {
            log::warn("invalid bind-remote-workspace-context");
            return;
        };
        let ready = match self
            .registry
            .bind_workspace_context(&id, path.clone(), identity)
        {
            Ok(ready) => ready,
            Err(error) => {
                log::warn(format!(
                    "failed to bind remote workspace context, remoteSessionId={id}: {error}"
                ));
                return;
            }
        };
        if let Some(current) = self.registry.get_session(&id) {
            // scope 换代后，旧 Renderer / 手机 attachment 不得继续持有远端 channel。
            let generation = current.generation;
            self.close_leases(|lease| {
                lease.remote_session_id == id && lease.generation < generation
            });
        }
        let id_for_log = id.clone();
        tokio::spawn(async move {
            let mut ready = ready;
            if let Ok(result) = ready.wait_for(Option::is_some).await
                && let Some(Err(error)) = result.clone()
            {
                log::warn(format!(
                    "failed to prepare bound remote workspace, remoteSessionId={id_for_log}: {error}"
                ));
            }
        });
        if let Some(previous) = previous {
            self.sources.remove_source(&previous);
        }
        log::info(format!(
            "bound remote workspace context, remoteSessionId={id}, workspacePath={path}"
        ));
    }

    async fn on_dispose(&self, message: Value) {
        let Some(id) = text(&message, "remoteSessionId") else {
            return;
        };
        let disposed = self
            .registry
            .get_session(&id)
            .and_then(|session| session.scope());
        self.close_leases(|lease| lease.remote_session_id == id);
        self.registry.dispose_session(&id).await;
        if let Some(scope) = disposed {
            self.sources.remove_source(&scope);
        }
        self.send(
            json!({"type": "remote-workspace-closed", "remoteSessionId": id, "reason": "disposed"}),
        );
        self.log_topology("remote-session-disposed");
    }

    async fn on_provisioning(&self, message: Value) {
        let (Some(request_id), Some(environment_key), Some(id)) = (
            text(&message, "requestId"),
            text(&message, "environmentKey"),
            text(&message, "remoteSessionId"),
        ) else {
            log::warn("invalid provider-provisioning-execute");
            return;
        };
        let respond = |status: &str, error: Option<String>| {
            let mut response = json!({
                "type": "provider-provisioning-execution-result",
                "requestId": request_id,
                "environmentKey": environment_key,
                "status": status,
            });
            if let Some(error) = error {
                response["error"] = json!(error);
            }
            self.send(response);
        };
        let session = self.registry.get_session(&id).filter(|session| {
            session.workspace_identity.is_some() && session.environment_key == environment_key
        });
        let Some(session) = session else {
            respond(
                "failed",
                Some("Remote Environment registration 已失效".into()),
            );
            return;
        };
        let scope = RemoteScope {
            remote_session_id: session.remote_session_id.clone(),
            workspace_path: session.workspace_path.clone().unwrap_or_else(|| "/".into()),
            workspace_identity: session.workspace_identity.clone().unwrap_or_default(),
        };
        let handle = match self.registry.resolve_scoped(&scope) {
            Ok(handle) => handle,
            Err(error) => {
                respond("failed", Some(error));
                return;
            }
        };
        let settings = self.settings.get().await;
        let result = provisioning::sync_local_to_remote(
            &self.provisioning,
            &settings,
            &handle.connection.client,
        )
        .await;
        let status = result["status"].as_str().unwrap_or("failed").to_owned();
        respond(&status, result["errorMessage"].as_str().map(str::to_owned));
    }

    /// 签发 endpoint 前的同步校验：session 存在且 scope 与之完全相同。
    pub fn validate_scope(&self, scope: &RemoteScope) -> Result<(), String> {
        let session = self
            .registry
            .get_session(&scope.remote_session_id)
            .ok_or_else(|| {
                format!(
                    "未找到远程 logical session，remoteSessionId={}",
                    scope.remote_session_id
                )
            })?;
        if session.workspace_path.as_deref() != Some(scope.workspace_path.as_str())
            || session.workspace_identity.as_deref() != Some(scope.workspace_identity.as_str())
        {
            return Err(format!(
                "远程 attachment scope 与 logical session 不匹配，remoteSessionId={}",
                scope.remote_session_id
            ));
        }
        Ok(())
    }

    /// 为一条远端 scope 的 attachment 组装 channel 表；先等 workspace 运行时就绪（WSL 释放屏障）。
    pub async fn attach(
        &self,
        attachment_id: &str,
        client_mode: ClientMode,
        scope: &RemoteScope,
        local: &ChannelRegistry,
    ) -> Result<RemoteAttachment, String> {
        let handle = self.registry.wait_for_scoped(scope).await?;
        let generation = self
            .registry
            .get_session(&scope.remote_session_id)
            .map(|session| session.generation)
            .ok_or_else(|| {
                format!(
                    "远程连接当前不可用，remoteSessionId={}",
                    scope.remote_session_id
                )
            })?;
        let client = handle.connection.client.clone();
        let mut registry = scope::base_registry(&client, local);
        registry.register("zcode-agent", handle.agent.clone());
        let backend = handle.connection.backend.clone();
        registry.register(
            "zcode-task",
            tasks::RemoteTaskChannel::new(
                handle.tasks.clone(),
                self.sources.clone(),
                scope.clone(),
            ),
        );
        // 远端集合的 bots 绑定该连接的远端 zcode-task（`runStartupBackgroundTasks:false`）；未实现的
        // 成员回落到本地 Host 的同名 channel。
        if let Some(bots) = self.connection_bots(&handle) {
            registry.register(
                "bots",
                crate::bots::service::channel(bots).build(local.get("bots")),
            );
        }
        registry.register(
            "zcode-session",
            tasks::RemoteSessionChannel::new(client.clone(), backend),
        );
        if let Some(backend) = &handle.connection.backend {
            registry.register(
                "prompt-attachment-transfer",
                ForwardChannel::new(backend.clone(), "prompt-attachment-transfer"),
            );
        }
        // 远端媒体按 attachment 的 clientMode 选择数据面：桌面用 Host 回环 Range，手机保持不注册。
        if client_mode == ClientMode::DesktopContinuous && self.media_range_enabled {
            registry.register(
                "media-preview",
                Arc::new(media::MediaPreviewChannel(media::MediaPreviewProxy::new(
                    client,
                    scope.clone(),
                ))),
            );
        }
        let (close, closed) = watch::channel(false);
        self.lock_leases().push(Lease {
            attachment_id: attachment_id.to_owned(),
            remote_session_id: scope.remote_session_id.clone(),
            generation,
            close,
        });
        log::info(format!(
            "attached scoped service port, attachmentId={attachment_id}, scope=remote, remoteSessionId={}",
            scope.remote_session_id
        ));
        self.log_topology("attachment-added");
        Ok(RemoteAttachment {
            registry: Arc::new(registry),
            closed,
        })
    }

    fn lock_background(&self) -> std::sync::MutexGuard<'_, Vec<AbortHandle>> {
        self.background
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Host 退出：关闭全部远端 attachment，释放全部连接。
    pub async fn shutdown(&self) {
        for task in self.lock_background().drain(..) {
            task.abort();
        }
        self.close_leases(|_| true);
        self.registry.dispose().await;
    }

    /// 测试与诊断：当前登记的远端 attachment。
    pub fn attachment_ids(&self) -> Vec<String> {
        self.lock_leases()
            .iter()
            .filter(|lease| !lease.close.is_closed())
            .map(|lease| lease.attachment_id.clone())
            .collect()
    }
}

/// 建立一条远端连接（registry 的 connect 回调）：读设置得到下载代理与 WSL 运行时网络，经传输建连，
/// 再挂上运行时偏好桥。
async fn connect_handle(
    transport: Arc<dyn Transport>,
    settings: SettingStore,
    control: Arc<dyn ControlSink>,
    progress: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    owner: std::sync::Weak<RemoteWorkspaces>,
    realtime: Option<Arc<TaskRealtimeBridge>>,
    request: ConnectRequest,
) -> Result<Arc<RemoteHandle>, String> {
    let current = settings.get().await;
    let mut network = Map::new();
    for (from, to) in [
        ("httpProxy", "httpProxy"),
        ("httpProxyNoProxy", "noProxy"),
        ("httpProxyCaCertPath", "caCertPath"),
    ] {
        if let Some(value) = current[from].as_str() {
            network.insert(to.into(), json!(value));
        }
    }
    // WSL 的 Agent 网络以设置页为权威；SSH / Docker 保持远端自身设置。
    let remote_runtime_network = (request.target.kind() == TargetKind::Wsl).then(|| {
        let mut runtime = Map::new();
        runtime.insert("authoritative".into(), json!(true));
        if let Some(proxy) = current["httpProxy"].as_str() {
            runtime.insert("httpProxy".into(), json!(proxy));
        }
        if let Some(no_proxy) = current["httpProxyNoProxy"].as_str() {
            runtime.insert("noProxy".into(), json!(no_proxy));
        }
        Value::Object(runtime)
    });
    let request_id = request.request_id.clone();
    let active = progress
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&request_id)
        .cloned();
    let log_control = control.clone();
    let log_fn: transport::LogFn = Arc::new(move |level, message: String| {
        match level {
            LogLevel::Info => log::info(format!("remote connection: {message}")),
            LogLevel::Warn => log::warn(format!("remote connection: {message}")),
            LogLevel::Error => log::error(format!("remote connection: {message}")),
        }
        // 共享 Host 内多个连接并发：日志只归发起这次建连的 requestId，连接 settle 后停止上报。
        if message.trim().is_empty()
            || !active
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::SeqCst))
        {
            return;
        }
        log_control.send(raw(json!({
            "type": "remote-workspace-connection-log",
            "requestId": request_id,
            "level": level_str(level),
            "message": message,
        })));
    });
    let connection = transport
        .connect(ConnectParams {
            target: request.target.raw().clone(),
            remote_assets: request.remote_assets,
            remote_runtime_network,
            network: Value::Object(network),
            log: log_fn,
            abort: request.abort,
        })
        .await?;
    let connection = Arc::new(connection);
    let mut background = vec![runtime_preferences_bridge(
        connection.client.clone(),
        settings,
    )];
    background.extend(resource_telemetry(
        &connection.client,
        &control,
        telemetry_environment_key(&request.target),
    ));
    // 运行中任务数（WSL 释放屏障）回到窗口 registry。
    let tracker = tasks::TaskTracker::new(
        connection.client.clone(),
        control,
        Arc::new(move |path: &str, identity: Option<&str>, count: u64| {
            if let Some(remote) = owner.upgrade() {
                remote
                    .registry
                    .set_workspace_running_task_count(path, identity, count);
            }
        }),
    );
    let tasks = tasks::RemoteTaskOps::new(
        connection.client.clone(),
        connection.backend.clone(),
        tracker.clone(),
        realtime,
    );
    Ok(Arc::new(RemoteHandle {
        tracker,
        tasks,
        bots: OnceLock::new(),
        agent: RemoteAgentChannel::new(connection.client.clone()),
        connection,
        target: request.target,
        background,
    }))
}

/// 按 Host 环境组装 [`RemoteWorkspaces`] 并登记控制消息（`install()` 的 remote 块）。
///
/// 连接器由 Main 经 `ZCODE_HOST_NODE_EXEC_PATH`（Electron）与 `ZCODE_REMOTE_CONNECTOR_PATH`
/// （`out/host/remoteConnector.js`）给出；缺任一项时建连明确失败。
pub fn install(
    context: &crate::ServiceContext,
    sources: provisioning::ProvisioningSources,
    controller: &Arc<crate::tasks::controller::Controller>,
    realtime: Option<Arc<TaskRealtimeBridge>>,
    router: &mut ControlRouter,
) -> Arc<RemoteWorkspaces> {
    let environment = &context.environment;
    let non_empty = |key: &str| {
        environment
            .vars
            .get(key)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    let child_env = crate::support::env::ProcessEnv::for_children(
        &environment.vars,
        &context.init.runtime_process_env_patch,
    );
    let transport: Arc<dyn Transport> = match (
        non_empty("ZCODE_HOST_NODE_EXEC_PATH"),
        non_empty("ZCODE_REMOTE_CONNECTOR_PATH"),
    ) {
        (Some(node_exec), Some(script)) => Arc::new(transport::ConnectorTransport::new(
            transport::ConnectorConfig {
                node_exec: node_exec.into(),
                args: vec![script],
                env: child_env.vars().clone(),
                cwd: environment.user_home.clone(),
            },
        )),
        _ => Arc::new(transport::UnavailableTransport),
    };
    let remote = install_with(
        RemoteConfig {
            transport,
            settings: sources.settings.clone(),
            control: context.control.clone(),
            provisioning: ProvisioningSource {
                personal_config_file: environment.app_config_dir().join("provider_config.json"),
                credentials: sources.credentials.clone(),
            },
            media_range_enabled: non_empty("ZCODE_REMOTE_MEDIA_RANGE_PREVIEW_ENABLED").as_deref()
                != Some("0"),
            wsl_idle_ttl: Duration::from_secs(60),
            realtime,
        },
        router,
    );
    remote.sources.attach_to(controller);
    remote.lock_background().extend(provisioning::watch_sources(
        &sources,
        context.control.clone(),
    ));
    remote
}

/// 测试与 composition root 共用：给定传输组装并登记控制消息。
pub fn install_with(config: RemoteConfig, router: &mut ControlRouter) -> Arc<RemoteWorkspaces> {
    let remote = RemoteWorkspaces::new(config);
    remote.register_control(router);
    remote
}

/// 资源分组用的环境身份（`resolveResourceTelemetryEnvironmentKey`）：sha256(buildRemoteEnvironmentKey)。
fn telemetry_environment_key(target: &RemoteTarget) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(target.environment_key().as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 远端 CLI / MCP 的资源事实转给 Main（`registerHostServiceResourceTelemetry`，runtimeSurface
/// 为 remote）：远端 stdio server 以 trusted-host-relay 角色提供这些事件，Host 只透传。
fn resource_telemetry(
    client: &zcode_host_rpc::RpcClient,
    control: &Arc<dyn ControlSink>,
    environment_key: String,
) -> Vec<AbortHandle> {
    // (远端事件, Main 消息类型, 载荷字段, 是否带 environmentKey)
    const EVENTS: [(&str, &str, &str, bool); 4] = [
        (
            "onDynamicProcessResourceSample",
            "agent-resource-sample",
            "sample",
            true,
        ),
        ("onDynamicMcpTelemetry", "mcp-telemetry", "event", false),
        (
            "onDynamicMcpResourceSamples",
            "mcp-resource-samples",
            "samples",
            true,
        ),
        (
            "onDynamicToolExecResource",
            "tool-exec-resource",
            "sample",
            false,
        ),
    ];
    EVENTS
        .iter()
        .map(|(event, kind, field, with_environment)| {
            let mut events = client.listen("zcode-agent", event, RpcValue::Undefined);
            let (control, environment_key) = (control.clone(), environment_key.clone());
            tokio::spawn(async move {
                while let Some(payload) = events.next().await {
                    let mut message = Map::new();
                    message.insert("type".into(), json!(kind));
                    message.insert("runtimeSurface".into(), json!("remote"));
                    if *with_environment {
                        message.insert("environmentKey".into(), json!(environment_key));
                    }
                    message.insert((*field).into(), payload.into_json());
                    control.send(raw(Value::Object(message)));
                }
            })
            .abort_handle()
        })
        .collect()
}

/// 远端 Agent 的 app-global 偏好仍以本机设置为权威（`runtimePreferencesBridge`）。
fn runtime_preferences_bridge(
    client: zcode_host_rpc::RpcClient,
    settings: SettingStore,
) -> AbortHandle {
    let mut requests = client.listen(
        "zcode-agent",
        "onDynamicSessionRuntimePreferencesRequest",
        RpcValue::Undefined,
    );
    tokio::spawn(async move {
        while let Some(request) = requests.next().await {
            let request = request.into_json();
            let Some(request_id) = request["requestId"].as_str().map(str::to_owned) else {
                continue;
            };
            log::info(format!(
                "runtime preferences host request received, requestId={request_id}"
            ));
            let preferences =
                crate::agent::runtime::runtime_preferences(&settings.get().await, &request);
            let response = json!({
                "requestId": request_id,
                "resolution": {"status": "resolved", "preferences": preferences},
            });
            if let Err(error) = client
                .call(
                    "zcode-agent",
                    "respondSessionRuntimePreferences",
                    vec![RpcValue::Json(response)],
                )
                .await
            {
                log::warn(format!(
                    "runtime preferences host response failed, requestId={request_id}: {}",
                    error.message
                ));
            }
        }
    })
    .abort_handle()
}

#[cfg(test)]
mod bots_tests;
#[cfg(test)]
mod fake_remote;
#[cfg(test)]
mod registry_tests;
#[cfg(test)]
mod remote_tests;
