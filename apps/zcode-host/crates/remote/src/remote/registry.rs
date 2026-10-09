//! 远端连接注册表（`packages/desktop/src/host/windowRemoteConnectionRegistry.ts`）。U4 起 Host 是 App 级的，
//! 注册表随之由所有窗口共用（rust-u4-app-host.md §3）。
//!
//! 连接 entry 与 logical session 的唯一所有者。规则见 rust-host-remote.md §2.3：
//!
//! ```text
//! connect(requestId) ─► session(connecting) ─► 复用表命中（SSH/WSL 同键 connecting|online）或新建 entry
//!                                             └─ entry 后台建连 ─► online：session online；失败：session 全删
//!                    ─► 等 entry ready ─► 准备 workspace 运行时（WSL：等上一代释放）─► descriptor
//! 底层关闭 ─► entry disconnected、移出复用表 ─► session disconnected/offline ─► on_session_closed
//! dispose_session ─► 最后一个 session：WSL 空闲 60 s 再释放；SSH 保留为窗口缓存；其余立即释放
//! ```
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio::sync::watch;
use tokio::task::AbortHandle;

use super::target::{RemoteTarget, TargetKind};
use super::transport::CloseEvent;

pub const CANCELLED: &str = "远程连接已取消";
const DEFAULT_WSL_IDLE_TTL: Duration = Duration::from_secs(60);

/// 底层连接句柄：注册表只关心关闭事件与释放。
pub trait RegistryHandle: Send + Sync + 'static {
    fn closed(&self) -> BoxFuture<'static, CloseEvent>;
    fn dispose(&self) -> BoxFuture<'static, ()>;
}

pub struct ConnectRequest {
    /// 发起建连的 logical connect；连接过程日志归它。
    pub request_id: String,
    pub target: RemoteTarget,
    pub remote_assets: Value,
    pub abort: watch::Receiver<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceContext {
    pub workspace_path: String,
    pub workspace_identity: Option<String>,
}

impl WorkspaceContext {
    pub fn key(&self) -> String {
        workspace_key(&self.workspace_path, self.workspace_identity.as_deref())
    }
}

/// `workspaceIdentity?.trim() || workspacePath`。
pub fn workspace_key(path: &str, identity: Option<&str>) -> String {
    identity
        .map(str::trim)
        .filter(|identity| !identity.is_empty())
        .unwrap_or(path)
        .to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionClosed {
    pub remote_session_id: String,
    pub event: CloseEventInfo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseEventInfo {
    pub exit_code: Option<i64>,
    pub error: Option<String>,
}

type ConnectFn<H> =
    Arc<dyn Fn(ConnectRequest) -> BoxFuture<'static, Result<Arc<H>, String>> + Send + Sync>;
type ReleaseFn<H> =
    Arc<dyn Fn(Arc<H>, WorkspaceContext) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

type ReleaseErrorFn = Arc<dyn Fn(&WorkspaceContext, &str) + Send + Sync>;

pub struct RegistryOptions<H> {
    pub connect: ConnectFn<H>,
    pub release_workspace: Option<ReleaseFn<H>>,
    pub on_release_error: ReleaseErrorFn,
    pub on_session_closed: Arc<dyn Fn(SessionClosed) + Send + Sync>,
    pub create_id: Arc<dyn Fn() -> String + Send + Sync>,
    pub wsl_idle_ttl: Duration,
}

impl<H> RegistryOptions<H> {
    pub fn new(connect: ConnectFn<H>) -> Self {
        Self {
            connect,
            release_workspace: None,
            on_release_error: Arc::new(|_, _| {}),
            on_session_closed: Arc::new(|_| {}),
            create_id: Arc::new(crate::agent::service::uuid_v4),
            wsl_idle_ttl: DEFAULT_WSL_IDLE_TTL,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Online,
    Closing,
    Failed,
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadyState {
    Pending,
    Ready,
    Failed,
}

/// attachment 的远端 scope（`windowHostAttachmentScopeSchema` 的 remote 分支）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteScope {
    pub remote_session_id: String,
    pub workspace_path: String,
    pub workspace_identity: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionSnapshot {
    pub remote_session_id: String,
    pub request_id: String,
    pub target: Value,
    pub kind: TargetKind,
    pub environment_key: String,
    pub workspace_path: Option<String>,
    pub workspace_identity: Option<String>,
    pub generation: u64,
    pub state: ConnState,
    pub online: bool,
}

impl SessionSnapshot {
    /// `WindowHostRemoteWorkspaceDescriptor`。
    pub fn descriptor(&self) -> Value {
        let mut object = Map::new();
        object.insert("remoteSessionId".into(), json!(self.remote_session_id));
        object.insert("target".into(), self.target.clone());
        if let Some(path) = &self.workspace_path {
            object.insert("workspacePath".into(), json!(path));
        }
        if let Some(identity) = &self.workspace_identity {
            object.insert("workspaceIdentity".into(), json!(identity));
        }
        object.insert("generation".into(), json!(self.generation));
        Value::Object(object)
    }

    pub fn scope(&self) -> Option<RemoteScope> {
        Some(RemoteScope {
            remote_session_id: self.remote_session_id.clone(),
            workspace_path: self.workspace_path.clone()?,
            workspace_identity: self.workspace_identity.clone()?,
        })
    }
}

type ReleaseResult = Option<Result<(), String>>;

struct RuntimeState {
    context: WorkspaceContext,
    generation: u64,
    pending_release_generation: Option<u64>,
    release: Option<watch::Receiver<ReleaseResult>>,
    release_id: u64,
}

enum Ready<H> {
    Pending,
    Online(Arc<H>),
    Failed(String),
}

impl<H> Clone for Ready<H> {
    fn clone(&self) -> Self {
        match self {
            Ready::Pending => Ready::Pending,
            Ready::Online(handle) => Ready::Online(handle.clone()),
            Ready::Failed(error) => Ready::Failed(error.clone()),
        }
    }
}

struct Entry<H> {
    key: String,
    target: RemoteTarget,
    state: ConnState,
    abort: watch::Sender<bool>,
    sessions: BTreeSet<String>,
    ready: watch::Sender<Ready<H>>,
    handle: Option<Arc<H>>,
    disposed: bool,
    dispose_done: Option<watch::Receiver<bool>>,
    close_watch: Option<AbortHandle>,
    idle_timer: Option<AbortHandle>,
    workspace_keys: HashSet<String>,
    running: HashMap<String, u64>,
    runtimes: HashMap<String, RuntimeState>,
}

struct Session {
    remote_session_id: String,
    request_id: String,
    target: RemoteTarget,
    workspace_path: Option<String>,
    workspace_identity: Option<String>,
    generation: u64,
    state: ConnState,
    online: bool,
    entry: u64,
    workspace_key: Option<String>,
    workspace_generation: Option<u64>,
    ready_state: ReadyState,
    workspace_ready: watch::Sender<ReleaseResult>,
    cancelled: bool,
    cancel: watch::Sender<bool>,
}

impl Session {
    fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            remote_session_id: self.remote_session_id.clone(),
            request_id: self.request_id.clone(),
            target: self.target.sanitized(),
            kind: self.target.kind(),
            environment_key: self.target.environment_key(),
            workspace_path: self.workspace_path.clone(),
            workspace_identity: self.workspace_identity.clone(),
            generation: self.generation,
            state: self.state,
            online: self.online,
        }
    }
}

struct State<H> {
    disposed: bool,
    next_entry: u64,
    next_release: u64,
    entries: HashMap<u64, Entry<H>>,
    by_key: HashMap<String, u64>,
    sessions: HashMap<String, Session>,
    pending_requests: HashMap<String, String>,
}

pub struct RemoteRegistry<H: RegistryHandle> {
    state: Arc<Mutex<State<H>>>,
    options: Arc<RegistryOptions<H>>,
}

impl<H: RegistryHandle> Clone for RemoteRegistry<H> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            options: self.options.clone(),
        }
    }
}

pub struct ConnectParams {
    pub request_id: String,
    pub target: RemoteTarget,
    pub remote_assets: Value,
    pub workspace_path: Option<String>,
    pub workspace_identity: Option<String>,
}

fn unavailable(remote_session_id: &str) -> String {
    format!("远程连接当前不可用，remoteSessionId={remote_session_id}")
}

impl<H: RegistryHandle> RemoteRegistry<H> {
    pub fn new(options: RegistryOptions<H>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                disposed: false,
                next_entry: 1,
                next_release: 1,
                entries: HashMap::new(),
                by_key: HashMap::new(),
                sessions: HashMap::new(),
                pending_requests: HashMap::new(),
            })),
            options: Arc::new(options),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State<H>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    // ---- 连接 ----

    pub async fn connect(&self, params: ConnectParams) -> Result<SessionSnapshot, String> {
        let remote_session_id = (self.options.create_id)();
        let (mut ready, mut cancel) = {
            let mut state = self.lock();
            if state.disposed {
                return Err("窗口 Host 的远程连接 registry 已释放".into());
            }
            if state.pending_requests.contains_key(&params.request_id) {
                return Err(format!(
                    "远程连接 requestId 重复，requestId={}",
                    params.request_id
                ));
            }
            let key = params.target.connection_key(&remote_session_id);
            let entry_id = self.resolve_entry(&mut state, key, &params);
            let entry = state
                .entries
                .get_mut(&entry_id)
                .expect("entry just resolved");
            let online = entry.state == ConnState::Online;
            entry.sessions.insert(remote_session_id.clone());
            let workspace_key = params
                .workspace_path
                .as_deref()
                .map(|path| workspace_key(path, params.workspace_identity.as_deref()));
            if let Some(key) = &workspace_key {
                entry.workspace_keys.insert(key.clone());
            }
            let ready = entry.ready.subscribe();
            let (cancel_tx, cancel_rx) = watch::channel(false);
            let (workspace_ready, _) = watch::channel(None);
            state.sessions.insert(
                remote_session_id.clone(),
                Session {
                    remote_session_id: remote_session_id.clone(),
                    request_id: params.request_id.clone(),
                    target: params.target.clone(),
                    workspace_path: params.workspace_path.clone(),
                    workspace_identity: params.workspace_identity.clone(),
                    generation: 1,
                    state: if online {
                        ConnState::Online
                    } else {
                        ConnState::Connecting
                    },
                    online,
                    entry: entry_id,
                    workspace_key,
                    workspace_generation: None,
                    ready_state: ReadyState::Pending,
                    workspace_ready,
                    cancelled: false,
                    cancel: cancel_tx,
                },
            );
            state
                .pending_requests
                .insert(params.request_id.clone(), remote_session_id.clone());
            (ready, cancel_rx)
        };

        let result = async {
            let outcome = tokio::select! {
                outcome = ready.wait_for(|ready| !matches!(ready, Ready::Pending)) => {
                    outcome.map(|ready| ready.clone()).map_err(|_| CANCELLED.to_owned())?
                }
                _ = super::transport::aborted(&mut cancel) => return Err(CANCELLED.to_owned()),
            };
            if let Ready::Failed(error) = outcome {
                return Err(error);
            }
            let mut workspace_ready = {
                let mut state = self.lock();
                self.prepare_workspace_runtime(&mut state, &remote_session_id)
                    .ok_or_else(|| CANCELLED.to_owned())?
            };
            tokio::select! {
                result = workspace_ready.wait_for(Option::is_some) => {
                    match result.map(|value| value.clone()) {
                        Ok(Some(Ok(()))) => {}
                        Ok(Some(Err(error))) => return Err(error),
                        _ => return Err(CANCELLED.to_owned()),
                    }
                }
                _ = super::transport::aborted(&mut cancel) => return Err(CANCELLED.to_owned()),
            }
            let mut state = self.lock();
            let session = state
                .sessions
                .get_mut(&remote_session_id)
                .filter(|session| !session.cancelled)
                .ok_or_else(|| CANCELLED.to_owned())?;
            session.state = ConnState::Online;
            session.online = true;
            Ok(session.snapshot())
        }
        .await;

        let mut dispose_entry = None;
        {
            let mut state = self.lock();
            if result.is_err()
                && let Some(session) = state.sessions.remove(&remote_session_id)
            {
                let entry_id = session.entry;
                if let Some(entry) = state.entries.get_mut(&entry_id) {
                    entry.sessions.remove(&remote_session_id);
                    if entry.sessions.is_empty() && entry.target.kind() == TargetKind::Docker {
                        dispose_entry = Some(entry_id);
                    }
                }
            }
            if state.pending_requests.get(&params.request_id) == Some(&remote_session_id) {
                state.pending_requests.remove(&params.request_id);
            }
        }
        if let Some(entry_id) = dispose_entry {
            self.dispose_entry(entry_id).await;
        }
        result
    }

    fn resolve_entry(&self, state: &mut State<H>, key: String, params: &ConnectParams) -> u64 {
        if let Some(entry_id) = state.by_key.get(&key).copied()
            && let Some(entry) = state.entries.get_mut(&entry_id)
            && !entry.disposed
            && !*entry.abort.borrow()
            && matches!(entry.state, ConnState::Connecting | ConnState::Online)
        {
            if let Some(timer) = entry.idle_timer.take() {
                timer.abort();
            }
            return entry_id;
        }
        let entry_id = state.next_entry;
        state.next_entry += 1;
        let (abort, abort_rx) = watch::channel(false);
        let (ready, _) = watch::channel(Ready::Pending);
        state.entries.insert(
            entry_id,
            Entry {
                key: key.clone(),
                target: params.target.clone(),
                state: ConnState::Connecting,
                abort,
                sessions: BTreeSet::new(),
                ready,
                handle: None,
                disposed: false,
                dispose_done: None,
                close_watch: None,
                idle_timer: None,
                workspace_keys: HashSet::new(),
                running: HashMap::new(),
                runtimes: HashMap::new(),
            },
        );
        state.by_key.insert(key, entry_id);
        let connect = (self.options.connect)(ConnectRequest {
            request_id: params.request_id.clone(),
            target: params.target.clone(),
            remote_assets: params.remote_assets.clone(),
            abort: abort_rx,
        });
        let registry = self.clone();
        tokio::spawn(async move {
            let result = connect.await;
            registry.on_entry_connected(entry_id, result).await;
        });
        entry_id
    }

    async fn on_entry_connected(&self, entry_id: u64, result: Result<Arc<H>, String>) {
        match result {
            Ok(handle) => {
                // None：entry 已不存在；Some(true)：最后一个 owner 已离开，需要立即释放。
                let dispose_now = {
                    let mut state = self.lock();
                    state.entries.get_mut(&entry_id).map(|entry| {
                        entry.handle = Some(handle.clone());
                        entry.disposed || entry.sessions.is_empty()
                    })
                };
                match dispose_now {
                    None => {
                        // entry 已被释放：迟到的成功结果立即释放，不复活旧连接。
                        handle.dispose().await;
                        return;
                    }
                    Some(true) => {
                        // 底层 connector 无法中断认证或部署时，最后一个 owner 已离开：迟到结果直接释放。
                        self.dispose_entry(entry_id).await;
                        let ready = self
                            .lock()
                            .entries
                            .get(&entry_id)
                            .map(|entry| entry.ready.clone());
                        if let Some(ready) = ready {
                            ready.send_replace(Ready::Online(handle));
                        }
                        return;
                    }
                    Some(false) => {}
                }
                let mut state = self.lock();
                let Some(entry) = state.entries.get_mut(&entry_id) else {
                    return;
                };
                entry.state = ConnState::Online;
                let registry = self.clone();
                let closed = handle.closed();
                entry.close_watch = Some(
                    tokio::spawn(async move {
                        let event = closed.await;
                        registry.handle_connection_closed(entry_id, event);
                    })
                    .abort_handle(),
                );
                let sessions: Vec<String> = entry.sessions.iter().cloned().collect();
                entry.ready.send_replace(Ready::Online(handle));
                for id in sessions {
                    if let Some(session) = state.sessions.get_mut(&id)
                        && !session.cancelled
                    {
                        session.state = ConnState::Online;
                        session.online = true;
                    }
                }
            }
            Err(error) => {
                let mut state = self.lock();
                let Some(entry) = state.entries.get_mut(&entry_id) else {
                    return;
                };
                entry.state = ConnState::Failed;
                let key = entry.key.clone();
                let sessions: Vec<String> =
                    std::mem::take(&mut entry.sessions).into_iter().collect();
                entry.ready.send_replace(Ready::Failed(error));
                if state.by_key.get(&key) == Some(&entry_id) {
                    state.by_key.remove(&key);
                }
                for id in sessions {
                    if let Some(session) = state.sessions.remove(&id) {
                        state.pending_requests.remove(&session.request_id);
                    }
                }
                state.entries.remove(&entry_id);
            }
        }
    }

    fn handle_connection_closed(&self, entry_id: u64, event: CloseEvent) {
        let closed = {
            let mut state = self.lock();
            let Some(entry) = state.entries.get_mut(&entry_id) else {
                return;
            };
            if entry.disposed || entry.state == ConnState::Closing {
                return;
            }
            entry.state = ConnState::Disconnected;
            if let Some(timer) = entry.idle_timer.take() {
                timer.abort();
            }
            let key = entry.key.clone();
            let sessions: Vec<String> = entry.sessions.iter().cloned().collect();
            if state.by_key.get(&key) == Some(&entry_id) {
                state.by_key.remove(&key);
            }
            let mut closed = Vec::new();
            for id in sessions {
                if let Some(session) = state.sessions.get_mut(&id) {
                    session.state = ConnState::Disconnected;
                    session.online = false;
                    closed.push(id);
                }
            }
            closed
        };
        for remote_session_id in closed {
            (self.options.on_session_closed)(SessionClosed {
                remote_session_id,
                event: CloseEventInfo {
                    exit_code: event.exit_code,
                    error: event.error.clone(),
                },
            });
        }
    }

    /// 释放 entry：幂等；返回时底层句柄已释放。
    fn dispose_entry(&self, entry_id: u64) -> BoxFuture<'static, ()> {
        let (handle, mut done, done_tx) = {
            let mut state = self.lock();
            let Some(entry) = state.entries.get_mut(&entry_id) else {
                return Box::pin(async {});
            };
            if let Some(mut done) = entry.dispose_done.clone() {
                return Box::pin(async move {
                    let _ = done.wait_for(|done| *done).await;
                });
            }
            let (done_tx, done) = watch::channel(false);
            entry.dispose_done = Some(done.clone());
            entry.disposed = true;
            if let Some(timer) = entry.idle_timer.take() {
                timer.abort();
            }
            if let Some(watcher) = entry.close_watch.take() {
                watcher.abort();
            }
            entry.state = ConnState::Closing;
            entry.abort.send_replace(true);
            let key = entry.key.clone();
            let handle = entry.handle.clone();
            if state.by_key.get(&key) == Some(&entry_id) {
                state.by_key.remove(&key);
            }
            (handle, done, done_tx)
        };
        let registry = self.clone();
        Box::pin(async move {
            if let Some(handle) = handle {
                handle.dispose().await;
            }
            {
                let mut state = registry.lock();
                if state
                    .entries
                    .get(&entry_id)
                    .is_some_and(|entry| entry.sessions.is_empty())
                {
                    state.entries.remove(&entry_id);
                }
            }
            done_tx.send_replace(true);
            let _ = done.wait_for(|done| *done).await;
        })
    }

    // ---- workspace 运行时（WSL） ----

    fn session_owns_workspace(session: &Session, entry_id: u64, key: &str) -> bool {
        !session.cancelled
            && session.entry == entry_id
            && session.workspace_key.as_deref() == Some(key)
    }

    fn has_other_owner(state: &State<H>, entry_id: u64, key: &str, excluded: &str) -> bool {
        state.sessions.values().any(|candidate| {
            candidate.remote_session_id != excluded
                && Self::session_owns_workspace(candidate, entry_id, key)
        })
    }

    fn has_any_owner(state: &State<H>, entry_id: u64, key: &str) -> bool {
        state
            .sessions
            .values()
            .any(|candidate| Self::session_owns_workspace(candidate, entry_id, key))
    }

    /// 返回会话的 workspace 就绪结果接收端；会话不存在返回 None。
    fn prepare_workspace_runtime(
        &self,
        state: &mut State<H>,
        remote_session_id: &str,
    ) -> Option<watch::Receiver<ReleaseResult>> {
        let session = state.sessions.get(remote_session_id)?;
        let entry_id = session.entry;
        let kind = state
            .entries
            .get(&entry_id)
            .map(|entry| entry.target.kind());
        let (path, key) = match (&session.workspace_path, &session.workspace_key) {
            (Some(path), Some(key)) if kind == Some(TargetKind::Wsl) => (path.clone(), key.clone()),
            _ => {
                let session = state.sessions.get_mut(remote_session_id)?;
                session.ready_state = ReadyState::Ready;
                session.workspace_ready.send_replace(Some(Ok(())));
                return Some(session.workspace_ready.subscribe());
            }
        };
        let identity = session.workspace_identity.clone();
        let has_other = Self::has_other_owner(state, entry_id, &key, remote_session_id);
        let entry = state.entries.get_mut(&entry_id)?;
        let runtime = entry.runtimes.entry(key.clone()).or_insert(RuntimeState {
            context: WorkspaceContext {
                workspace_path: path.clone(),
                workspace_identity: identity.clone(),
            },
            generation: 0,
            pending_release_generation: None,
            release: None,
            release_id: 0,
        });
        runtime.context = WorkspaceContext {
            workspace_path: path,
            workspace_identity: identity,
        };
        if !has_other {
            // 上一代 release 仍在等待 running task 时，新 owner 已重新持有 workspace：
            // 尚未开始的旧 release 必须失效，不能在任务归零后误清新 runtime。
            runtime.generation += 1;
        }
        runtime.pending_release_generation = None;
        let generation = runtime.generation;
        let release = runtime.release.clone();
        let session = state.sessions.get_mut(remote_session_id)?;
        session.workspace_generation = Some(generation);
        session.ready_state = ReadyState::Pending;
        session.workspace_ready.send_replace(None);
        let receiver = session.workspace_ready.subscribe();
        let registry = self.clone();
        let id = remote_session_id.to_owned();
        tokio::spawn(async move {
            let waited = match release {
                Some(mut release) => match release.wait_for(Option::is_some).await {
                    Ok(result) => result.clone().unwrap_or(Ok(())),
                    Err(_) => Ok(()),
                },
                None => Ok(()),
            };
            let mut state = registry.lock();
            let current_generation = state
                .entries
                .get(&entry_id)
                .and_then(|entry| entry.runtimes.get(&key))
                .map(|runtime| runtime.generation);
            let Some(session) = state.sessions.get_mut(&id) else {
                return;
            };
            if session.workspace_generation != Some(generation) {
                return;
            }
            let result = match waited {
                Err(error) => Err(error),
                Ok(()) if session.cancelled || current_generation != Some(generation) => {
                    Err(CANCELLED.to_owned())
                }
                Ok(()) => Ok(()),
            };
            session.ready_state = if result.is_ok() {
                ReadyState::Ready
            } else {
                ReadyState::Failed
            };
            session.workspace_ready.send_replace(Some(result));
        });
        Some(receiver)
    }

    fn start_workspace_release(
        &self,
        state: &mut State<H>,
        entry_id: u64,
        key: &str,
        generation: u64,
    ) {
        let release_id = state.next_release;
        let Some(entry) = state.entries.get_mut(&entry_id) else {
            return;
        };
        let handle = entry.handle.clone();
        let online = entry.state == ConnState::Online;
        let is_wsl = entry.target.kind() == TargetKind::Wsl;
        let Some(runtime) = entry.runtimes.get_mut(key) else {
            return;
        };
        let Some(release_fn) = self.options.release_workspace.clone() else {
            return;
        };
        if !is_wsl || runtime.generation != generation || runtime.release.is_some() || !online {
            return;
        }
        let Some(handle) = handle else { return };
        state.next_release += 1;
        let Some(runtime) = state
            .entries
            .get_mut(&entry_id)
            .and_then(|entry| entry.runtimes.get_mut(key))
        else {
            return;
        };
        runtime.pending_release_generation = None;
        let context = runtime.context.clone();
        let (done, receiver) = watch::channel(None);
        runtime.release = Some(receiver);
        runtime.release_id = release_id;
        let registry = self.clone();
        let key = key.to_owned();
        tokio::spawn(async move {
            let result = release_fn(handle, context.clone()).await;
            if let Err(error) = &result {
                (registry.options.on_release_error)(&context, error);
            }
            {
                let mut state = registry.lock();
                let owned = Self::has_any_owner(&state, entry_id, &key);
                if let Some(entry) = state.entries.get_mut(&entry_id)
                    && let Some(runtime) = entry.runtimes.get_mut(&key)
                {
                    if runtime.release_id == release_id {
                        runtime.release = None;
                    }
                    if runtime.generation == generation
                        && runtime.pending_release_generation.is_none()
                        && !owned
                    {
                        entry.runtimes.remove(&key);
                    }
                }
            }
            // 失败结果留给同 workspace 的下一代 acquire：fail-closed，而不是踩过未完成的清理。
            done.send_replace(Some(result));
        });
    }

    fn request_workspace_release(
        &self,
        state: &mut State<H>,
        entry_id: u64,
        remote_session_id: &str,
        key: Option<&str>,
        generation: Option<u64>,
    ) -> Option<watch::Receiver<ReleaseResult>> {
        let (Some(key), Some(generation)) = (key, generation) else {
            return None;
        };
        let entry = state.entries.get(&entry_id)?;
        if entry.target.kind() != TargetKind::Wsl {
            return None;
        }
        if Self::has_other_owner(state, entry_id, key, remote_session_id) {
            return None;
        }
        let entry = state.entries.get_mut(&entry_id)?;
        let running = entry.running.get(key).copied().unwrap_or(0);
        let runtime = entry.runtimes.get_mut(key)?;
        if runtime.generation != generation {
            return None;
        }
        if running > 0 {
            runtime.pending_release_generation = Some(generation);
            return None;
        }
        self.start_workspace_release(state, entry_id, key, generation);
        state
            .entries
            .get(&entry_id)?
            .runtimes
            .get(key)?
            .release
            .clone()
    }

    fn schedule_wsl_idle_dispose(&self, state: &mut State<H>, entry_id: u64) {
        let ttl = self.options.wsl_idle_ttl;
        let Some(entry) = state.entries.get_mut(&entry_id) else {
            return;
        };
        if let Some(timer) = entry.idle_timer.take() {
            timer.abort();
        }
        let running = entry.running.values().any(|count| *count > 0);
        if entry.target.kind() != TargetKind::Wsl
            || entry.disposed
            || !entry.sessions.is_empty()
            || running
        {
            return;
        }
        let registry = self.clone();
        entry.idle_timer = Some(
            tokio::spawn(async move {
                tokio::time::sleep(ttl).await;
                let dispose = {
                    let state = registry.lock();
                    state.entries.get(&entry_id).is_some_and(|entry| {
                        state.by_key.get(&entry.key) == Some(&entry_id)
                            && !entry.disposed
                            && entry.sessions.is_empty()
                            && !entry.running.values().any(|count| *count > 0)
                    })
                };
                if dispose {
                    registry.dispose_entry(entry_id).await;
                }
            })
            .abort_handle(),
        );
    }

    // ---- 取消、bind、释放 ----

    pub fn cancel_connect(&self, request_id: &str) {
        let dispose = {
            let mut state = self.lock();
            let Some(remote_session_id) = state.pending_requests.remove(request_id) else {
                return;
            };
            let Some(session) = state.sessions.get(&remote_session_id) else {
                return;
            };
            if session.cancelled {
                return;
            }
            let (entry_id, key, generation) = (
                session.entry,
                session.workspace_key.clone(),
                session.workspace_generation,
            );
            let mut session = state
                .sessions
                .remove(&remote_session_id)
                .expect("session present");
            session.cancelled = true;
            session.cancel.send_replace(true);
            if let Some(entry) = state.entries.get_mut(&entry_id) {
                entry.sessions.remove(&remote_session_id);
            }
            let _ = self.request_workspace_release(
                &mut state,
                entry_id,
                &remote_session_id,
                key.as_deref(),
                generation,
            );
            let Some(entry) = state.entries.get(&entry_id) else {
                return;
            };
            if !entry.sessions.is_empty() {
                return;
            }
            let (kind, conn, has_handle) =
                (entry.target.kind(), entry.state, entry.handle.is_some());
            match (kind, conn) {
                (TargetKind::Ssh, ConnState::Connecting) => {
                    // 最后一个等待者取消：按对象身份退休旧 entry 再中止底层连接；迟到的旧结果
                    // 不能删除或复活同 key 的新连接，也不能沿用上一次的凭据。
                    if let Some(entry) = state.entries.get_mut(&entry_id) {
                        let key = entry.key.clone();
                        entry.state = ConnState::Closing;
                        entry.abort.send_replace(true);
                        if state.by_key.get(&key) == Some(&entry_id) {
                            state.by_key.remove(&key);
                        }
                    }
                    None
                }
                (TargetKind::Wsl, ConnState::Online) => {
                    self.schedule_wsl_idle_dispose(&mut state, entry_id);
                    None
                }
                (TargetKind::Ssh, ConnState::Online) => {
                    // ready 的 SSH 连接是窗口缓存：取消一次 logical attach 不销毁它。
                    if let Some(timer) = state
                        .entries
                        .get_mut(&entry_id)
                        .and_then(|entry| entry.idle_timer.take())
                    {
                        timer.abort();
                    }
                    None
                }
                _ => has_handle.then_some(entry_id),
            }
        };
        if let Some(entry_id) = dispose {
            let future = self.dispose_entry(entry_id);
            tokio::spawn(future);
        }
    }

    /// 改写 session 的 workspace 上下文，generation 加一；返回新一代 workspace 的就绪结果。
    pub fn bind_workspace_context(
        &self,
        remote_session_id: &str,
        workspace_path: String,
        workspace_identity: String,
    ) -> Result<watch::Receiver<ReleaseResult>, String> {
        let mut state = self.lock();
        let Some(session) = state.sessions.get_mut(remote_session_id) else {
            return Err(format!(
                "未找到远程 logical session，remoteSessionId={remote_session_id}"
            ));
        };
        let previous = (
            session.entry,
            session.workspace_key.clone(),
            session.workspace_generation,
        );
        let key = workspace_key(&workspace_path, Some(&workspace_identity));
        session.workspace_path = Some(workspace_path);
        session.workspace_identity = Some(workspace_identity);
        session.generation += 1;
        session.workspace_key = Some(key.clone());
        let entry_id = session.entry;
        if let Some(entry) = state.entries.get_mut(&entry_id) {
            entry.workspace_keys.insert(key.clone());
        }
        let ready = self
            .prepare_workspace_runtime(&mut state, remote_session_id)
            .ok_or_else(|| unavailable(remote_session_id))?;
        if previous.1.as_deref() != Some(key.as_str()) {
            let _ = self.request_workspace_release(
                &mut state,
                previous.0,
                remote_session_id,
                previous.1.as_deref(),
                previous.2,
            );
        }
        Ok(ready)
    }

    fn resolve_handle(state: &State<H>, scope: &RemoteScope) -> Result<Arc<H>, String> {
        let id = &scope.remote_session_id;
        let session = state.sessions.get(id).ok_or_else(|| unavailable(id))?;
        if session.workspace_path.as_deref() != Some(scope.workspace_path.as_str())
            || session.workspace_identity.as_deref() != Some(scope.workspace_identity.as_str())
        {
            return Err(format!(
                "远程 attachment scope 与 logical session 不匹配，remoteSessionId={id}"
            ));
        }
        let entry = state.entries.get(&session.entry);
        match entry.and_then(|entry| entry.handle.clone()) {
            Some(handle)
                if session.state == ConnState::Online
                    && session.online
                    && session.ready_state == ReadyState::Ready =>
            {
                Ok(handle)
            }
            _ => Err(unavailable(id)),
        }
    }

    /// scope 必须与 session 的 path、identity 完全相同；不在线或未就绪时报不可用。
    pub fn resolve_scoped(&self, scope: &RemoteScope) -> Result<Arc<H>, String> {
        Self::resolve_handle(&self.lock(), scope)
    }

    /// 先等该 session 当前一代的 workspace 运行时就绪（WSL 释放屏障），再解析。
    pub async fn wait_for_scoped(&self, scope: &RemoteScope) -> Result<Arc<H>, String> {
        let mut ready = {
            let state = self.lock();
            let session = state
                .sessions
                .get(&scope.remote_session_id)
                .ok_or_else(|| unavailable(&scope.remote_session_id))?;
            session.workspace_ready.subscribe()
        };
        if let Ok(result) = ready.wait_for(Option::is_some).await
            && let Some(Err(error)) = result.clone()
        {
            return Err(error);
        }
        self.resolve_scoped(scope)
    }

    pub async fn dispose_session(&self, remote_session_id: &str) {
        let (release, dispose) = {
            let mut state = self.lock();
            let Some(session) = state.sessions.remove(remote_session_id) else {
                return;
            };
            if state
                .pending_requests
                .get(&session.request_id)
                .map(String::as_str)
                == Some(remote_session_id)
            {
                state.pending_requests.remove(&session.request_id);
            }
            let entry_id = session.entry;
            if let Some(entry) = state.entries.get_mut(&entry_id) {
                entry.sessions.remove(remote_session_id);
            }
            let release = self.request_workspace_release(
                &mut state,
                entry_id,
                remote_session_id,
                session.workspace_key.as_deref(),
                session.workspace_generation,
            );
            let mut dispose = None;
            let decision = state
                .entries
                .get(&entry_id)
                .filter(|entry| entry.sessions.is_empty())
                .map(|entry| (entry.target.kind(), entry.state));
            match decision {
                Some((TargetKind::Wsl, ConnState::Online)) => {
                    self.schedule_wsl_idle_dispose(&mut state, entry_id)
                }
                Some((TargetKind::Ssh, ConnState::Online)) => {
                    // 只迁移 SSH pool 的 owner：ready 连接继续作为窗口缓存，Host 退出时统一释放。
                    if let Some(timer) = state
                        .entries
                        .get_mut(&entry_id)
                        .and_then(|entry| entry.idle_timer.take())
                    {
                        timer.abort();
                    }
                }
                Some(_) => dispose = Some(entry_id),
                None => {}
            }
            (release, dispose)
        };
        if let Some(entry_id) = dispose {
            self.dispose_entry(entry_id).await;
        }
        if let Some(mut release) = release {
            let _ = release.wait_for(Option::is_some).await;
        }
    }

    // ---- 查询 ----

    pub fn get_session(&self, remote_session_id: &str) -> Option<SessionSnapshot> {
        self.lock()
            .sessions
            .get(remote_session_id)
            .map(Session::snapshot)
    }

    pub fn list_sessions(&self) -> Vec<SessionSnapshot> {
        self.lock()
            .sessions
            .values()
            .map(Session::snapshot)
            .collect()
    }

    /// (path, identity) 精确匹配；在线优先，歧义时报错。
    pub fn find_session_for_workspace(
        &self,
        workspace_path: &str,
        workspace_identity: Option<&str>,
    ) -> Result<Option<SessionSnapshot>, String> {
        let state = self.lock();
        let matches: Vec<&Session> = state
            .sessions
            .values()
            .filter(|session| {
                session.workspace_path.as_deref() == Some(workspace_path)
                    && session.workspace_identity.as_deref() == workspace_identity
            })
            .collect();
        let online: Vec<&&Session> = matches
            .iter()
            .filter(|session| session.state == ConnState::Online && session.online)
            .collect();
        if online.len() > 1 || (online.is_empty() && matches.len() > 1) {
            return Err(format!(
                "远程 workspace scope 匹配到多个 logical session，workspacePath={workspace_path}"
            ));
        }
        Ok(online
            .first()
            .map(|session| session.snapshot())
            .or_else(|| matches.first().map(|session| session.snapshot())))
    }

    /// Bot 远端运行时的连接判定（rust-host-remote.md §2.9.2）：存在同 workspaceKey、同 target、在线且
    /// workspace 运行时就绪的 session。与 Main 原来按 route 表判定（attachable、同 target、同 workspaceKey）
    /// 的条件对应；注册表是 App 级的，不再区分窗口。
    pub fn has_connected_session(&self, key: &str, target: &Value) -> bool {
        let state = self.lock();
        state.sessions.values().any(|session| {
            session.state == ConnState::Online
                && session.online
                && session.ready_state == ReadyState::Ready
                && session.workspace_path.as_deref().is_some_and(|path| {
                    workspace_key(path, session.workspace_identity.as_deref()) == key
                })
                && session.target.same_as(target)
                && state
                    .entries
                    .get(&session.entry)
                    .is_some_and(|entry| entry.handle.is_some())
        })
    }

    pub fn stats(&self) -> (usize, usize) {
        let state = self.lock();
        (state.by_key.len(), state.sessions.len())
    }

    pub fn set_workspace_running_task_count(
        &self,
        workspace_path: &str,
        workspace_identity: Option<&str>,
        count: u64,
    ) {
        let key = workspace_key(workspace_path, workspace_identity);
        let mut state = self.lock();
        let entry_ids: Vec<u64> = state
            .by_key
            .values()
            .copied()
            .filter(|entry_id| {
                state.entries.get(entry_id).is_some_and(|entry| {
                    entry.target.kind() == TargetKind::Wsl && entry.workspace_keys.contains(&key)
                })
            })
            .collect();
        for entry_id in entry_ids {
            let Some(entry) = state.entries.get_mut(&entry_id) else {
                continue;
            };
            if count > 0 {
                entry.running.insert(key.clone(), count);
                if let Some(timer) = entry.idle_timer.take() {
                    timer.abort();
                }
                continue;
            }
            entry.running.remove(&key);
            let pending = entry
                .runtimes
                .get(&key)
                .and_then(|runtime| runtime.pending_release_generation);
            if let Some(generation) = pending {
                self.start_workspace_release(&mut state, entry_id, &key, generation);
            }
            self.schedule_wsl_idle_dispose(&mut state, entry_id);
        }
    }

    /// Host 退出：取消所有等待中的连接，释放所有 entry。
    pub async fn dispose(&self) {
        let requests: Vec<String> = {
            let mut state = self.lock();
            if state.disposed {
                Vec::new()
            } else {
                state.disposed = true;
                state.pending_requests.keys().cloned().collect()
            }
        };
        for request in requests {
            self.cancel_connect(&request);
        }
        let entries: Vec<u64> = {
            let mut state = self.lock();
            state.sessions.clear();
            state.pending_requests.clear();
            for entry in state.entries.values_mut() {
                entry.sessions.clear();
            }
            state.entries.keys().copied().collect()
        };
        let disposals: Vec<_> = entries
            .into_iter()
            .map(|entry_id| self.dispose_entry(entry_id))
            .collect();
        futures_util::future::join_all(disposals).await;
    }
}
