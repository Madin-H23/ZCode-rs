//! 注册表状态机（rust-host-remote.md §2.3）：复用键、取消、断线、WSL 空闲与释放屏障、bind、scope。
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::{oneshot, watch};

use super::registry::*;
use super::target::RemoteTarget;
use super::transport::CloseEvent;

struct FakeHandle {
    closed: watch::Sender<Option<CloseEvent>>,
    disposed: AtomicUsize,
}

impl FakeHandle {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            closed: watch::channel(None).0,
            disposed: AtomicUsize::new(0),
        })
    }

    fn close(&self, exit_code: i64) {
        self.closed.send_replace(Some(CloseEvent {
            exit_code: Some(exit_code),
            error: None,
        }));
    }

    fn disposed(&self) -> usize {
        self.disposed.load(Ordering::SeqCst)
    }
}

impl RegistryHandle for FakeHandle {
    fn closed(&self) -> BoxFuture<'static, CloseEvent> {
        let mut closed = self.closed.subscribe();
        Box::pin(async move {
            let event = closed.wait_for(Option::is_some).await.unwrap().clone();
            event.unwrap()
        })
    }

    fn dispose(&self) -> BoxFuture<'static, ()> {
        self.disposed.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }
}

type Responder = oneshot::Sender<Result<Arc<FakeHandle>, String>>;

#[derive(Default)]
struct Fake {
    connects: Mutex<Vec<(String, watch::Receiver<bool>, Option<Responder>)>>,
    releases: Mutex<
        Vec<(
            WorkspaceContext,
            Option<oneshot::Sender<Result<(), String>>>,
        )>,
    >,
    closed: Mutex<Vec<SessionClosed>>,
}

impl Fake {
    fn connect_count(&self) -> usize {
        self.connects.lock().unwrap().len()
    }

    fn respond(&self, index: usize, result: Result<Arc<FakeHandle>, String>) {
        let responder = self.connects.lock().unwrap()[index].2.take().unwrap();
        let _ = responder.send(result);
    }

    fn aborted(&self, index: usize) -> bool {
        *self.connects.lock().unwrap()[index].1.borrow()
    }

    fn release_count(&self) -> usize {
        self.releases.lock().unwrap().len()
    }

    fn finish_release(&self, index: usize, result: Result<(), String>) {
        let responder = self.releases.lock().unwrap()[index].1.take().unwrap();
        let _ = responder.send(result);
    }
}

fn registry(fake: &Arc<Fake>, wsl_idle: Duration) -> RemoteRegistry<FakeHandle> {
    let connector = fake.clone();
    let mut options = RegistryOptions::new(Arc::new(move |request: ConnectRequest| {
        let (sender, receiver) = oneshot::channel();
        connector.connects.lock().unwrap().push((
            request.target.log_label(),
            request.abort,
            Some(sender),
        ));
        Box::pin(async move { receiver.await.unwrap_or(Err("dropped".into())) })
            as BoxFuture<'static, _>
    }));
    let releaser = fake.clone();
    options.release_workspace = Some(Arc::new(move |_handle, context| {
        let (sender, receiver) = oneshot::channel();
        releaser
            .releases
            .lock()
            .unwrap()
            .push((context, Some(sender)));
        Box::pin(async move { receiver.await.unwrap_or(Ok(())) }) as BoxFuture<'static, _>
    }));
    let closed = fake.clone();
    options.on_session_closed = Arc::new(move |event| closed.closed.lock().unwrap().push(event));
    let counter = Arc::new(AtomicUsize::new(0));
    options.create_id =
        Arc::new(move || format!("s{}", counter.fetch_add(1, Ordering::SeqCst) + 1));
    options.wsl_idle_ttl = wsl_idle;
    RemoteRegistry::new(options)
}

fn params(request_id: &str, target: Value, path: &str) -> ConnectParams {
    let target = RemoteTarget::parse(&target).unwrap();
    let identity = target.workspace_identity(path);
    ConnectParams {
        request_id: request_id.into(),
        target,
        remote_assets: json!({}),
        workspace_path: Some(path.into()),
        workspace_identity: Some(identity),
    }
}

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(5)).await;
}

async fn spawn_connect(
    registry: &RemoteRegistry<FakeHandle>,
    params: ConnectParams,
) -> tokio::task::JoinHandle<Result<SessionSnapshot, String>> {
    let registry = registry.clone();
    let task = tokio::spawn(async move { registry.connect(params).await });
    settle().await;
    task
}

fn ssh(password: &str) -> Value {
    json!({"kind":"ssh","host":"Host","username":"u","password": password})
}

#[tokio::test]
async fn ssh_connections_are_reused_without_credentials_in_the_key() {
    let fake = Arc::new(Fake::default());
    let registry = registry(&fake, Duration::from_secs(60));
    let first = spawn_connect(&registry, params("r1", ssh("a"), "/w1")).await;
    let second = spawn_connect(&registry, params("r2", ssh("b"), "/w2")).await;
    assert_eq!(fake.connect_count(), 1);
    let duplicate = registry
        .connect(params("r1", ssh("a"), "/w3"))
        .await
        .unwrap_err();
    assert_eq!(duplicate, "远程连接 requestId 重复，requestId=r1");
    let handle = FakeHandle::new();
    fake.respond(0, Ok(handle.clone()));
    let first = first.await.unwrap().unwrap();
    let second = second.await.unwrap().unwrap();
    assert_ne!(first.remote_session_id, second.remote_session_id);
    assert_eq!(
        first.descriptor(),
        json!({"remoteSessionId": first.remote_session_id, "target": {"kind":"ssh","host":"Host","username":"u"},
               "workspacePath": "/w1", "workspaceIdentity": "remote:ssh:host:22:u:/w1", "generation": 1})
    );
    let scope = first.scope().unwrap();
    assert!(Arc::ptr_eq(
        &registry.resolve_scoped(&scope).unwrap(),
        &handle
    ));
    // 释放全部 session：ready SSH 连接留作窗口缓存，下一次连接直接复用。
    registry.dispose_session(&first.remote_session_id).await;
    registry.dispose_session(&second.remote_session_id).await;
    assert_eq!(handle.disposed(), 0);
    let third = registry
        .connect(params("r3", ssh("c"), "/w1"))
        .await
        .unwrap();
    assert_eq!(fake.connect_count(), 1);
    assert!(third.online);
    registry.dispose().await;
    assert_eq!(handle.disposed(), 1);
}

#[tokio::test]
async fn docker_sessions_get_dedicated_connections_and_release_them() {
    let fake = Arc::new(Fake::default());
    let registry = registry(&fake, Duration::from_secs(60));
    let docker = json!({"kind":"docker","container":"c"});
    let first = spawn_connect(&registry, params("r1", docker.clone(), "/w")).await;
    let second = spawn_connect(&registry, params("r2", docker, "/w")).await;
    assert_eq!(fake.connect_count(), 2);
    let (one, two) = (FakeHandle::new(), FakeHandle::new());
    fake.respond(0, Ok(one.clone()));
    fake.respond(1, Ok(two.clone()));
    let first = first.await.unwrap().unwrap();
    let _second = second.await.unwrap().unwrap();
    let error = registry
        .find_session_for_workspace("/w", Some("remote:docker:c:/w"))
        .unwrap_err();
    assert!(error.starts_with("远程 workspace scope 匹配到多个 logical session"));
    registry.dispose_session(&first.remote_session_id).await;
    assert_eq!((one.disposed(), two.disposed()), (1, 0));
}

#[tokio::test]
async fn connect_failure_fails_every_waiter_and_retires_the_entry() {
    let fake = Arc::new(Fake::default());
    let registry = registry(&fake, Duration::from_secs(60));
    let first = spawn_connect(&registry, params("r1", ssh("a"), "/w1")).await;
    let second = spawn_connect(&registry, params("r2", ssh("a"), "/w2")).await;
    fake.respond(0, Err("SSH 认证失败".into()));
    assert_eq!(first.await.unwrap().unwrap_err(), "SSH 认证失败");
    assert_eq!(second.await.unwrap().unwrap_err(), "SSH 认证失败");
    assert!(registry.list_sessions().is_empty());
    let retry = spawn_connect(&registry, params("r1", ssh("a"), "/w1")).await;
    assert_eq!(fake.connect_count(), 2);
    fake.respond(1, Ok(FakeHandle::new()));
    assert!(retry.await.unwrap().is_ok());
}

#[tokio::test]
async fn cancelling_the_last_ssh_waiter_aborts_and_late_success_is_released() {
    let fake = Arc::new(Fake::default());
    let registry = registry(&fake, Duration::from_secs(60));
    let waiter = spawn_connect(&registry, params("r1", ssh("a"), "/w")).await;
    registry.cancel_connect("r1");
    assert_eq!(waiter.await.unwrap().unwrap_err(), CANCELLED);
    assert!(fake.aborted(0));
    // 旧 entry 已退休：立即重连新建 entry，不沿用旧凭据。
    let retry = spawn_connect(&registry, params("r2", ssh("b"), "/w")).await;
    assert_eq!(fake.connect_count(), 2);
    let late = FakeHandle::new();
    fake.respond(0, Ok(late.clone()));
    settle().await;
    assert_eq!(late.disposed(), 1);
    let fresh = FakeHandle::new();
    fake.respond(1, Ok(fresh.clone()));
    let session = retry.await.unwrap().unwrap();
    assert!(Arc::ptr_eq(
        &registry.resolve_scoped(&session.scope().unwrap()).unwrap(),
        &fresh
    ));
}

#[tokio::test]
async fn connection_close_marks_sessions_offline_and_notifies() {
    let fake = Arc::new(Fake::default());
    let registry = registry(&fake, Duration::from_secs(60));
    let waiter = spawn_connect(&registry, params("r1", ssh("a"), "/w")).await;
    let handle = FakeHandle::new();
    fake.respond(0, Ok(handle.clone()));
    let session = waiter.await.unwrap().unwrap();
    let scope = session.scope().unwrap();
    handle.close(255);
    settle().await;
    let closed = fake.closed.lock().unwrap().clone();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].remote_session_id, session.remote_session_id);
    assert_eq!(closed[0].event.exit_code, Some(255));
    let snapshot = registry.get_session(&session.remote_session_id).unwrap();
    assert_eq!(
        (snapshot.state, snapshot.online),
        (ConnState::Disconnected, false)
    );
    assert_eq!(
        registry.resolve_scoped(&scope).err().unwrap(),
        format!(
            "远程连接当前不可用，remoteSessionId={}",
            session.remote_session_id
        )
    );
    // 断线的 entry 已移出复用表：重连新建连接；释放旧 session 时释放旧连接。
    let reconnect = spawn_connect(&registry, params("r2", ssh("a"), "/w")).await;
    assert_eq!(fake.connect_count(), 2);
    fake.respond(1, Ok(FakeHandle::new()));
    reconnect.await.unwrap().unwrap();
    registry.dispose_session(&session.remote_session_id).await;
    assert_eq!(handle.disposed(), 1);
}

#[tokio::test]
async fn scope_must_match_the_logical_session_exactly() {
    let fake = Arc::new(Fake::default());
    let registry = registry(&fake, Duration::from_secs(60));
    let waiter = spawn_connect(&registry, params("r1", ssh("a"), "/w")).await;
    fake.respond(0, Ok(FakeHandle::new()));
    let session = waiter.await.unwrap().unwrap();
    let mut scope = session.scope().unwrap();
    scope.workspace_path = "/other".into();
    assert_eq!(
        registry.resolve_scoped(&scope).err().unwrap(),
        format!(
            "远程 attachment scope 与 logical session 不匹配，remoteSessionId={}",
            session.remote_session_id
        )
    );
    let ready = registry
        .bind_workspace_context(
            &session.remote_session_id,
            "/other".into(),
            "remote:ssh:host:22:u:/other".into(),
        )
        .unwrap();
    assert_eq!(ready.borrow().clone(), Some(Ok(())));
    let bound = registry.get_session(&session.remote_session_id).unwrap();
    assert_eq!(bound.generation, 2);
    assert!(registry.resolve_scoped(&bound.scope().unwrap()).is_ok());
    assert!(
        registry
            .bind_workspace_context("missing", "/x".into(), "x".into())
            .is_err()
    );
}

#[tokio::test]
async fn wsl_entries_idle_out_after_the_last_session() {
    let fake = Arc::new(Fake::default());
    let registry = registry(&fake, Duration::from_millis(80));
    let wsl = json!({"kind":"wsl","distro":"Ubuntu"});
    let waiter = spawn_connect(&registry, params("r1", wsl.clone(), "/w")).await;
    let handle = FakeHandle::new();
    fake.respond(0, Ok(handle.clone()));
    let session = waiter.await.unwrap().unwrap();
    let dispose = {
        let registry = registry.clone();
        let id = session.remote_session_id.clone();
        tokio::spawn(async move { registry.dispose_session(&id).await })
    };
    settle().await;
    // 最后一个 owner 离开：释放该 workspace 的远端运行时。
    assert_eq!(fake.release_count(), 1);
    fake.finish_release(0, Ok(()));
    dispose.await.unwrap();
    assert_eq!(handle.disposed(), 0);
    // 空闲期内复用同一连接。
    let reuse = registry
        .connect(params("r2", wsl.clone(), "/w"))
        .await
        .unwrap();
    assert_eq!(fake.connect_count(), 1);
    let dispose = {
        let registry = registry.clone();
        let id = reuse.remote_session_id.clone();
        tokio::spawn(async move { registry.dispose_session(&id).await })
    };
    settle().await;
    fake.finish_release(1, Ok(()));
    dispose.await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(handle.disposed(), 1);
}

#[tokio::test]
async fn wsl_release_waits_for_running_tasks_and_blocks_the_next_owner() {
    let fake = Arc::new(Fake::default());
    let registry = registry(&fake, Duration::from_secs(60));
    let wsl = json!({"kind":"wsl"});
    let waiter = spawn_connect(&registry, params("r1", wsl.clone(), "/w")).await;
    fake.respond(0, Ok(FakeHandle::new()));
    let session = waiter.await.unwrap().unwrap();
    let identity = session.workspace_identity.clone().unwrap();
    registry.set_workspace_running_task_count("/w", Some(&identity), 1);
    registry.dispose_session(&session.remote_session_id).await;
    // 有运行中任务：释放挂起。
    assert_eq!(fake.release_count(), 0);
    registry.set_workspace_running_task_count("/w", Some(&identity), 0);
    settle().await;
    assert_eq!(fake.release_count(), 1);
    // 释放进行中：新一代 owner 等它结束；释放失败则 fail-closed。
    let next = spawn_connect(&registry, params("r2", wsl, "/w")).await;
    assert!(!next.is_finished());
    fake.finish_release(0, Err("release failed".into()));
    assert_eq!(next.await.unwrap().unwrap_err(), "release failed");
}
