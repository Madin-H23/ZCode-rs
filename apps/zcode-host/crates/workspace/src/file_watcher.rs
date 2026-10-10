//! `file-watcher` channel（packages/services/src/fileWatcher/fileWatcherService.ts）。
//!
//! 与 TS 相同：id 是从 "0" 递增的十进制串；每个 watcher 150 ms 尾部防抖，窗口内只有一个已知
//! 路径时才带 `changedPath`；`dirPath` 原样回传 watch 时的入参；底层出错时立即发一次不带
//! `changedPath` 的事件并清理；未知 id 的订阅不报错、永不触发。
//!
//! 与 Node 实现的差异：非递归监听共用一个 notify watcher（按路径引用计数），递归监听各自独立。
//! Node/libuv 在 Linux 上同样共用一个 inotify fd；每个 watch 单独开 inotify 实例会很快耗尽
//! `max_user_instances`，而递归监听只有少数几个（.git 目录、非 Linux 的工作区），独立后也不会与
//! 非递归监听在同一 inode 的 watch 上互相覆盖。
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use futures_util::stream;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use zcode_host_kit::{EventHub, Overflow, Reply, RpcError, ServiceBuilder, log, replies};

use crate::support::{node_error, path};

/// 批量变更（如 git checkout）只触发一次刷新。
const DEBOUNCE: Duration = Duration::from_millis(150);

enum Change {
    Path(String),
    Unknown,
    Failed(String),
}

struct Registration {
    /// watch 的入参，事件的 `dirPath` 原样回传。
    path: String,
    /// 规范化后的根；notify（FSEvents 等）报告的是规范路径，按它截出相对部分再拼回入参。
    root: PathBuf,
    root_is_dir: bool,
    recursive: bool,
    /// 底层监听已建立；之前被清理的由 watch 自己释放，避免与启动交错时泄漏或重复释放。
    started: bool,
    events: EventHub<Value>,
    changes: mpsc::UnboundedSender<Change>,
}

impl Registration {
    fn classify(&self, changed: &Path) -> Option<Change> {
        let relative = changed.strip_prefix(&self.root).ok()?;
        if relative.as_os_str().is_empty() {
            // 目录自身的事件不上报（与 libuv 在 macOS 上一致）；监视单个文件时路径未知。
            return (!self.root_is_dir).then_some(Change::Unknown);
        }
        if !self.recursive && relative.components().count() != 1 {
            return None;
        }
        let relative = relative.to_string_lossy();
        Some(Change::Path(path::resolve(
            &self.path,
            &[relative.as_ref()],
        )))
    }
}

type Registrations = Arc<Mutex<HashMap<String, Registration>>>;

#[derive(Clone)]
enum Source {
    Shared,
    Own(String),
}

struct SharedWatcher {
    watcher: RecommendedWatcher,
    counts: HashMap<PathBuf, usize>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// notify 回调线程上执行：只做路由，不碰 watcher 本身（FSEvents 停止时会 join 回调线程）。
fn dispatch(registrations: &Registrations, source: &Source, result: notify::Result<notify::Event>) {
    let registrations = lock(registrations);
    let covers = |id: &str, registration: &Registration| match source {
        Source::Shared => !registration.recursive,
        Source::Own(own) => own == id,
    };
    match result {
        Ok(event) => {
            // libuv 不监听 open/close（inotify 的 IN_OPEN 等），读取文件不能触发刷新。
            if matches!(event.kind, EventKind::Access(_)) {
                return;
            }
            let rescan = event.need_rescan() || event.paths.is_empty();
            for (id, registration) in registrations.iter() {
                if !covers(id, registration) {
                    continue;
                }
                if rescan {
                    let _ = registration.changes.send(Change::Unknown);
                    continue;
                }
                for changed in &event.paths {
                    if let Some(change) = registration.classify(changed) {
                        let _ = registration.changes.send(change);
                    }
                }
            }
        }
        Err(error) => {
            let message = error.to_string();
            let mut affected = 0;
            for (id, registration) in registrations.iter() {
                let hit = match source {
                    Source::Own(own) => own == id,
                    // 共用 watcher 的错误只归给出错路径正是其根的监听；归不到的只记日志。
                    Source::Shared => {
                        !registration.recursive
                            && error.paths.iter().any(|p| p == &registration.root)
                    }
                };
                if hit && covers(id, registration) {
                    affected += 1;
                    let _ = registration.changes.send(Change::Failed(message.clone()));
                }
            }
            if affected == 0 {
                log::warn(format!("file watcher error: {message}"));
            }
        }
    }
}

/// 每个监听一个防抖任务；收到 Failed 时立即发最终事件并清理。
async fn debounce(
    id: String,
    dir_path: String,
    events: EventHub<Value>,
    mut changes: mpsc::UnboundedReceiver<Change>,
    owner: Weak<Inner>,
) {
    let mut pending: HashSet<String> = HashSet::new();
    let mut unknown = false;
    while let Some(first) = changes.recv().await {
        let mut next = Some(first);
        loop {
            match next.take() {
                Some(Change::Path(changed)) => {
                    pending.insert(changed);
                }
                Some(Change::Unknown) => unknown = true,
                Some(Change::Failed(message)) => {
                    log::warn(format!(
                        "文件监听器异常，清理 watcher id={id} path={dir_path} error={message}"
                    ));
                    events.emit(json!({"dirPath": dir_path}));
                    if let Some(inner) = owner.upgrade() {
                        inner.cleanup(&id);
                    }
                    return;
                }
                None => {}
            }
            match tokio::time::timeout(DEBOUNCE, changes.recv()).await {
                Ok(Some(change)) => next = Some(change),
                // unwatch：未触发的批次随之丢弃（TS 的 cleanup 会 clearTimeout）。
                Ok(None) => return,
                Err(_) => break,
            }
        }
        let mut payload = Map::new();
        payload.insert("dirPath".to_owned(), Value::String(dir_path.clone()));
        if !unknown
            && pending.len() == 1
            && let Some(changed) = pending.iter().next()
        {
            payload.insert("changedPath".to_owned(), Value::String(changed.clone()));
        }
        pending.clear();
        unknown = false;
        events.emit(Value::Object(payload));
    }
}

fn watch_error_message(error: &notify::Error, input: &str) -> String {
    match &error.kind {
        notify::ErrorKind::Io(io) => node_error::fs_message(io, "watch", input),
        notify::ErrorKind::PathNotFound => {
            format!("ENOENT: no such file or directory, watch '{input}'")
        }
        notify::ErrorKind::MaxFilesWatch => {
            format!("ENOSPC: System limit for number of file watchers reached, watch '{input}'")
        }
        _ => error.to_string(),
    }
}

#[derive(Clone)]
pub struct FileWatchers {
    inner: Arc<Inner>,
}

struct Inner {
    next_id: AtomicU64,
    registrations: Registrations,
    /// 递归监听各自的 watcher；不放进 registrations，回调线程上就不会析构 watcher。
    recursive: Mutex<HashMap<String, RecommendedWatcher>>,
    shared: Mutex<Option<SharedWatcher>>,
}

impl Inner {
    fn new_watcher(&self, source: Source) -> notify::Result<RecommendedWatcher> {
        let registrations = self.registrations.clone();
        RecommendedWatcher::new(
            move |result| dispatch(&registrations, &source, result),
            // 与 Node 的递归监听一致：不跟随符号链接目录，避免环。
            notify::Config::default().with_follow_symlinks(false),
        )
    }

    /// 阻塞调用：notify 的 watch 可能遍历目录树或重启 FSEvents 流。
    fn start(&self, id: &str, root: &Path, recursive: bool) -> notify::Result<()> {
        if recursive {
            let mut watcher = self.new_watcher(Source::Own(id.to_owned()))?;
            watcher.watch(root, RecursiveMode::Recursive)?;
            lock(&self.recursive).insert(id.to_owned(), watcher);
            return Ok(());
        }
        let mut shared = lock(&self.shared);
        let state = match shared.as_mut() {
            Some(state) => state,
            None => shared.insert(SharedWatcher {
                watcher: self.new_watcher(Source::Shared)?,
                counts: HashMap::new(),
            }),
        };
        let count = state.counts.get(root).copied().unwrap_or(0);
        if count == 0 {
            state.watcher.watch(root, RecursiveMode::NonRecursive)?;
        }
        state.counts.insert(root.to_path_buf(), count + 1);
        Ok(())
    }

    /// 阻塞调用：释放底层监听。
    fn release(&self, id: &str, root: &Path, recursive: bool) {
        if recursive {
            drop(lock(&self.recursive).remove(id));
            return;
        }
        let mut shared = lock(&self.shared);
        let Some(state) = shared.as_mut() else {
            return;
        };
        match state.counts.get(root).copied() {
            Some(count) if count > 1 => {
                state.counts.insert(root.to_path_buf(), count - 1);
            }
            Some(_) => {
                state.counts.remove(root);
                // 目录已被删除时 notify 可能已自行移除这条 watch，忽略 WatchNotFound。
                let _ = state.watcher.unwatch(root);
            }
            None => {}
        }
    }

    fn cleanup(self: &Arc<Self>, id: &str) {
        let Some(registration) = lock(&self.registrations).remove(id) else {
            return;
        };
        registration.events.close();
        if registration.started {
            self.release_later(id, registration.root, registration.recursive);
        }
    }

    /// 停止 notify watcher 可能要等回调线程退出，放到阻塞线程上做。
    fn release_later(self: &Arc<Self>, id: &str, root: PathBuf, recursive: bool) {
        let (inner, id) = (self.clone(), id.to_owned());
        tokio::task::spawn_blocking(move || inner.release(&id, &root, recursive));
    }
}

impl Default for FileWatchers {
    fn default() -> Self {
        Self::new()
    }
}

impl FileWatchers {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                next_id: AtomicU64::new(0),
                registrations: Arc::new(Mutex::new(HashMap::new())),
                recursive: Mutex::new(HashMap::new()),
                shared: Mutex::new(None),
            }),
        }
    }

    pub async fn watch(&self, input: String, recursive: bool) -> Result<String, RpcError> {
        let id = self
            .inner
            .next_id
            .fetch_add(1, Ordering::Relaxed)
            .to_string();
        let fail =
            |message: String| RpcError::new("Error", format!("无法监视目录 '{input}': {message}"));
        let root = match tokio::fs::canonicalize(&input).await {
            Ok(root) => PathBuf::from(path::strip_verbatim(&root.to_string_lossy())),
            Err(error) => return Err(fail(node_error::fs_message(&error, "watch", &input))),
        };
        let root_is_dir = tokio::fs::metadata(&root)
            .await
            .is_ok_and(|metadata| metadata.is_dir());
        let (changes, receiver) = mpsc::unbounded_channel();
        // 去抖后的批次，订阅者是 Renderer 的 `onDidChange`（Gateway 立即搬进出站队列）；落后 1024 批说明
        // 转发卡住，结束它的订阅（docs/specs/rust-u2-task-scope.md §6.2）。
        let events = EventHub::new("file-watcher-events", 1024, Overflow::CloseSubscriber);
        // 先登记再启动底层监听，启动后立刻到达的事件也能路由到这里。
        lock(&self.inner.registrations).insert(
            id.clone(),
            Registration {
                path: input.clone(),
                root: root.clone(),
                root_is_dir,
                recursive,
                started: false,
                events: events.clone(),
                changes,
            },
        );
        let (inner, start_id, start_root) = (self.inner.clone(), id.clone(), root.clone());
        let started =
            tokio::task::spawn_blocking(move || inner.start(&start_id, &start_root, recursive))
                .await
                .map_err(|error| notify::Error::generic(&error.to_string()))
                .and_then(|result| result);
        if let Err(error) = started {
            lock(&self.inner.registrations).remove(&id);
            return Err(fail(watch_error_message(&error, &input)));
        }
        let disposed = match lock(&self.inner.registrations).get_mut(&id) {
            Some(registration) => {
                registration.started = true;
                false
            }
            None => true,
        };
        if disposed {
            // 启动期间已被 disposeAll 清理：释放刚建立的底层监听。
            self.inner.release_later(&id, root, recursive);
            return Ok(id);
        }
        tokio::spawn(debounce(
            id.clone(),
            input,
            events,
            receiver,
            Arc::downgrade(&self.inner),
        ));
        Ok(id)
    }

    /// 未知 id 静默忽略。
    pub fn unwatch(&self, id: &str) {
        self.inner.cleanup(id);
    }

    pub fn dispose_all(&self) {
        let ids: Vec<String> = lock(&self.inner.registrations).keys().cloned().collect();
        for id in ids {
            self.inner.cleanup(&id);
        }
    }

    fn events(&self, id: &str) -> Option<EventHub<Value>> {
        lock(&self.inner.registrations)
            .get(id)
            .map(|registration| registration.events.clone())
    }
}

#[derive(Deserialize)]
struct WatchRequest {
    path: String,
    recursive: Option<bool>,
}

#[derive(Deserialize)]
struct UnwatchRequest {
    id: String,
}

pub fn service(watchers: FileWatchers) -> ServiceBuilder {
    let (watch, unwatch, dispose, events) = (
        watchers.clone(),
        watchers.clone(),
        watchers.clone(),
        watchers,
    );
    ServiceBuilder::new("file-watcher")
        .method("watch", move |_, args| {
            let watchers = watch.clone();
            let params = args.get::<WatchRequest>(0, "params");
            async move {
                let params = params?;
                let id = watchers
                    .watch(params.path, params.recursive.unwrap_or(false))
                    .await?;
                Ok(Reply::Value(json!({"id": id})))
            }
        })
        .method("unwatch", move |_, args| {
            let result = args
                .get::<UnwatchRequest>(0, "params")
                .map(|params| unwatch.unwatch(&params.id));
            async move { result.map(|()| Reply::Undefined) }
        })
        .method("disposeAll", move |_, _| {
            dispose.dispose_all();
            async { Ok(Reply::Undefined) }
        })
        .event("onDynamicChange", move |_, arg| {
            let id = match &arg {
                Value::String(id) => id.clone(),
                other => other.to_string(),
            };
            match events.events(&id) {
                Some(hub) => Ok(replies(hub.subscribe())),
                None => {
                    // watch 返回后、订阅前 watcher 可能已因目录删除等原因被清理：可恢复状态，不报错。
                    log::warn(format!("忽略已失效的文件监听订阅 id={id}"));
                    Ok(Box::pin(stream::pending()))
                }
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{call, channel, listen, reply_value};
    use futures_util::StreamExt;
    use std::time::Instant;

    async fn next_event(events: &mut zcode_host_kit::EventStream) -> Value {
        let event = tokio::time::timeout(Duration::from_secs(10), events.next())
            .await
            .expect("file change event")
            .expect("stream open");
        reply_value(event)
    }

    /// 等后端（FSEvents 等）启动完成，并丢掉监听开始前的变化被迟到上报的事件。
    async fn settle(streams: &mut [&mut zcode_host_kit::EventStream]) {
        tokio::time::sleep(Duration::from_millis(300)).await;
        for events in streams.iter_mut() {
            while tokio::time::timeout(Duration::from_millis(300), events.next())
                .await
                .is_ok()
            {}
        }
    }

    #[tokio::test]
    async fn single_change_carries_the_changed_path() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().to_str().unwrap().to_owned();
        let watcher = channel(service(FileWatchers::new()));
        let id = call(&watcher, "watch", json!({"path": input}))
            .await
            .unwrap()["id"]
            .clone();
        assert_eq!(id, "0");
        let mut events = listen(&watcher, "onDynamicChange", id.as_str().unwrap()).unwrap();
        settle(&mut [&mut events]).await;

        std::fs::write(dir.path().join("a.txt"), "a").unwrap();
        let event = next_event(&mut events).await;
        assert_eq!(event["dirPath"], input.as_str());
        assert_eq!(
            event["changedPath"],
            dir.path().join("a.txt").to_str().unwrap()
        );
    }

    #[tokio::test]
    async fn batches_are_debounced_and_recursive_watches_see_nested_changes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let input = dir.path().to_str().unwrap().to_owned();
        let watchers = FileWatchers::new();
        let watcher = channel(service(watchers.clone()));
        let flat = call(&watcher, "watch", json!({"path": input}))
            .await
            .unwrap()["id"]
            .clone();
        let deep = call(&watcher, "watch", json!({"path": input, "recursive": true}))
            .await
            .unwrap()["id"]
            .clone();
        let mut flat_events = listen(&watcher, "onDynamicChange", flat.as_str().unwrap()).unwrap();
        let mut deep_events = listen(&watcher, "onDynamicChange", deep.as_str().unwrap()).unwrap();
        settle(&mut [&mut flat_events, &mut deep_events]).await;

        // 同一防抖窗口里两个路径：只发一次，且不带 changedPath。
        let started = Instant::now();
        std::fs::write(dir.path().join("x.txt"), "x").unwrap();
        std::fs::write(dir.path().join("y.txt"), "y").unwrap();
        assert_eq!(
            next_event(&mut flat_events).await,
            json!({"dirPath": input})
        );
        assert!(started.elapsed() >= DEBOUNCE);
        assert_eq!(
            next_event(&mut deep_events).await,
            json!({"dirPath": input})
        );

        std::fs::write(dir.path().join("sub").join("nested.txt"), "n").unwrap();
        let event = next_event(&mut deep_events).await;
        assert_eq!(
            event["changedPath"],
            dir.path().join("sub").join("nested.txt").to_str().unwrap()
        );

        call(&watcher, "unwatch", json!({"id": flat}))
            .await
            .unwrap();
        assert!(next_ended(&mut flat_events).await);
        call(&watcher, "unwatch", json!({"id": "unknown"}))
            .await
            .unwrap();
        watchers.dispose_all();
        assert!(next_ended(&mut deep_events).await);
    }

    #[test]
    fn classify_limits_non_recursive_watches_to_direct_children() {
        let (changes, _receiver) = mpsc::unbounded_channel();
        let mut registration = Registration {
            path: "/ws/./link".to_owned(),
            root: PathBuf::from("/real/ws"),
            root_is_dir: true,
            recursive: false,
            started: true,
            events: EventHub::new("test", 16, Overflow::CloseSubscriber),
            changes,
        };
        let path_of = |change: Option<Change>| match change {
            Some(Change::Path(path)) => Some(path),
            _ => None,
        };
        let separator = std::path::MAIN_SEPARATOR;
        assert_eq!(
            path_of(registration.classify(Path::new("/real/ws/a.txt"))),
            Some(format!("{separator}ws{separator}link{separator}a.txt"))
        );
        assert!(
            registration
                .classify(Path::new("/real/ws/sub/a.txt"))
                .is_none()
        );
        assert!(registration.classify(Path::new("/real/ws")).is_none());
        assert!(
            registration
                .classify(Path::new("/elsewhere/a.txt"))
                .is_none()
        );
        registration.recursive = true;
        assert!(path_of(registration.classify(Path::new("/real/ws/sub/a.txt"))).is_some());
        registration.root_is_dir = false;
        assert!(matches!(
            registration.classify(Path::new("/real/ws")),
            Some(Change::Unknown)
        ));
    }

    #[tokio::test]
    async fn watcher_errors_emit_a_final_event_and_clean_up() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().to_str().unwrap().to_owned();
        let watchers = FileWatchers::new();
        let watcher = channel(service(watchers.clone()));
        let id = call(&watcher, "watch", json!({"path": input, "recursive": true}))
            .await
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut events = listen(&watcher, "onDynamicChange", &id).unwrap();
        dispatch(
            &watchers.inner.registrations,
            &Source::Own(id.clone()),
            Err(notify::Error::generic("boom")),
        );
        // 不防抖、不带 changedPath，随后订阅结束且 id 失效。
        assert_eq!(next_event(&mut events).await, json!({"dirPath": input}));
        assert!(next_ended(&mut events).await);
        assert!(watchers.events(&id).is_none());
    }

    async fn next_ended(events: &mut zcode_host_kit::EventStream) -> bool {
        tokio::time::timeout(Duration::from_secs(2), events.next())
            .await
            .is_ok_and(|event| event.is_none())
    }

    #[tokio::test]
    async fn missing_paths_and_unknown_ids() {
        let watcher = channel(service(FileWatchers::new()));
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let error = call(&watcher, "watch", json!({"path": missing}))
            .await
            .unwrap_err();
        let missing = missing.to_str().unwrap();
        assert_eq!(
            error.message,
            format!(
                "无法监视目录 '{missing}': ENOENT: no such file or directory, watch '{missing}'"
            )
        );
        // 失败也消耗 id。
        let id = call(&watcher, "watch", json!({"path": dir.path()}))
            .await
            .unwrap()["id"]
            .clone();
        assert_eq!(id, "1");
        let mut stale = listen(&watcher, "onDynamicChange", "42").unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), stale.next())
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn changed_paths_keep_the_caller_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join("link")).unwrap();
        let input = dir.path().join("link").to_str().unwrap().to_owned();
        let watcher = channel(service(FileWatchers::new()));
        let id = call(&watcher, "watch", json!({"path": input}))
            .await
            .unwrap()["id"]
            .clone();
        let mut events = listen(&watcher, "onDynamicChange", id.as_str().unwrap()).unwrap();
        settle(&mut [&mut events]).await;
        std::fs::write(real.join("f.txt"), "f").unwrap();
        let event = next_event(&mut events).await;
        assert_eq!(event["dirPath"], input.as_str());
        assert_eq!(event["changedPath"], format!("{input}/f.txt"));
    }
}
