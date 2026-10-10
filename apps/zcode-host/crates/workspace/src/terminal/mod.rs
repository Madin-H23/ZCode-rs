//! `terminal` channel（packages/services/src/terminal/terminalService.ts、terminal.ts）。
//!
//! 与 TS 相同：id 从 "0" 递增；输出块原样逐帧转发，没有缓冲与重放（订阅前的输出丢失，Renderer
//! 创建后立即 resize，shell 收到 SIGWINCH 会重绘提示符）；dispose 后不再发 exit；未知 id 的
//! write / resize 报 `Terminal not found`，dispose 静默忽略。实例属于整个 Host，不随连接清理。
//! 与 TS 不同：读输出的连接积压过高时暂停读 PTY（`flow.rs`，docs/specs/rust-u2-gateway.md §2.3）。
mod flow;
pub mod launch;
pub mod profile;
pub mod pty;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use futures_util::StreamExt;
use portable_pty::{Child, ChildKiller, MasterPty};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use zcode_host_kit::{
    EventHub, EventStream, Overflow, Reply, RpcError, ServiceBuilder, log, replies,
};

use crate::settings::SettingStore;
use crate::support::env::ProcessEnv;
use pty::PtyIo;

/// node-pty（unixTerminal.ts）在子进程退出后最多等 200 ms 让输出读完，再销毁 socket 并发 exit。
const EXIT_FLUSH_TIMEOUT: Duration = Duration::from_millis(200);

struct Terminal {
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    input: mpsc::UnboundedSender<String>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    data: EventHub<Value>,
    exit: EventHub<Value>,
}

impl Terminal {
    /// 关闭 master（Windows 上即 ClosePseudoConsole，可能阻塞到输出排空）。
    async fn close_master(&self) {
        let master = lock(&self.master).take();
        if let Some(master) = master {
            let _ = tokio::task::spawn_blocking(move || drop(master)).await;
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn not_found(id: &str) -> RpcError {
    RpcError::new("Error", format!("Terminal not found: {id}"))
}

/// node-pty 的 `cols || 80`：缺失、0、NaN 用默认值；小数截断。
fn initial_dimension(value: Option<f64>, default: u16) -> u16 {
    match value {
        Some(value) if value > 0.0 => value as u16,
        _ => default,
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn windows_build_number() -> Option<u32> {
    use windows_sys::Wdk::System::SystemServices::RtlGetVersion;
    use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;
    // SAFETY: OSVERSIONINFOW 是纯数据结构，全零是合法初值；RtlGetVersion 只写入这块栈内存。
    let mut info: OSVERSIONINFOW = unsafe { std::mem::zeroed() };
    info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
    // SAFETY: 指针指向上面已设置大小字段的有效结构体。
    let status = unsafe { RtlGetVersion(&mut info) };
    (status == 0).then_some(info.dwBuildNumber)
}

/// `windowsPty`：node-pty 在 Windows 上总是报告 conpty（旧版本 Windows 实际回退 winpty 也一样）。
fn windows_pty_info() -> Option<Value> {
    #[cfg(windows)]
    {
        let mut info = Map::new();
        info.insert("backend".to_owned(), Value::String("conpty".to_owned()));
        if let Some(build) = windows_build_number() {
            info.insert("buildNumber".to_owned(), Value::from(build));
        }
        Some(Value::Object(info))
    }
    #[cfg(not(windows))]
    None
}

#[derive(Clone)]
pub struct Terminals {
    inner: Arc<Inner>,
}

struct Inner {
    env: ProcessEnv,
    settings: SettingStore,
    next_id: AtomicU64,
    terminals: Mutex<HashMap<String, Arc<Terminal>>>,
    flow: Arc<flow::Flow>,
}

impl Terminals {
    pub fn new(env: ProcessEnv, settings: SettingStore) -> Self {
        Self {
            inner: Arc::new(Inner {
                env,
                settings,
                next_id: AtomicU64::new(0),
                terminals: Mutex::new(HashMap::new()),
                flow: Arc::default(),
            }),
        }
    }

    fn get(&self, id: &str) -> Result<Arc<Terminal>, RpcError> {
        lock(&self.inner.terminals)
            .get(id)
            .cloned()
            .ok_or_else(|| not_found(id))
    }

    pub async fn create(
        &self,
        cols: Option<f64>,
        rows: Option<f64>,
        cwd: Option<&str>,
    ) -> Result<Value, RpcError> {
        let id = self
            .inner
            .next_id
            .fetch_add(1, Ordering::Relaxed)
            .to_string();
        let base = &self.inner.env;
        let shell = launch::resolve_shell(base).await?;
        let cwd = launch::resolve_cwd(cwd, base).await?;
        let env = launch::terminal_env(base, cfg!(target_os = "macos"));
        let settings = self.inner.settings.get().await;
        let font = profile::resolve(&settings, base).await;
        let (cols, rows) = (initial_dimension(cols, 80), initial_dimension(rows, 24));

        let failed = |message: String| {
            RpcError::new(
                "Error",
                format!("Failed to start terminal with shell '{shell}' in '{cwd}': {message}"),
            )
        };
        let spawned = {
            let (shell, cwd) = (shell.clone(), cwd.clone());
            tokio::task::spawn_blocking(move || pty::spawn(&shell, &cwd, &env, cols, rows))
                .await
                .map_err(|error| failed(error.to_string()))?
                .map_err(|error| failed(format!("{error:#}")))?
        };
        let pty::Spawned {
            master,
            child,
            mut killer,
        } = spawned;
        // 订阅者是 Renderer 的 `onDynamicData` / `onDynamicExit`（Gateway 立即搬进出站队列，saturated 时
        // 由流控暂停读 PTY）；落后 4096 块说明转发卡住，结束它的订阅（docs/specs/rust-u2-task-scope.md
        // §6.2）。退出事件每个终端一条。
        let data = EventHub::new("terminal-data", 4096, Overflow::CloseSubscriber);
        let exit = EventHub::new("terminal-exit", 16, Overflow::CloseSubscriber);
        let emitter = data.clone();
        let paused = self.inner.flow.open(&id);
        let io = match pty::start(master.as_ref(), paused, move |text| {
            emitter.emit(Value::String(text))
        }) {
            Ok(io) => io,
            Err(error) => {
                self.inner.flow.close(&id);
                let _ = killer.kill();
                return Err(failed(error.to_string()));
            }
        };
        let terminal = Arc::new(Terminal {
            master: Mutex::new(Some(master)),
            input: io.input.clone(),
            killer: Mutex::new(killer),
            data,
            exit,
        });
        lock(&self.inner.terminals).insert(id.clone(), terminal.clone());
        tokio::spawn(supervise(
            Arc::downgrade(&self.inner),
            id.clone(),
            terminal,
            child,
            io,
        ));

        let mut response = Map::new();
        response.insert("id".to_owned(), Value::String(id));
        response.insert("shell".to_owned(), Value::String(shell));
        response.insert("fontFamily".to_owned(), Value::String(font.font_family));
        if let Some(size) = font.font_size {
            response.insert("fontSize".to_owned(), profile::js_number(size));
        }
        if let Some(theme) = font.theme {
            response.insert("theme".to_owned(), Value::Object(theme));
        }
        response.insert(
            "fontFamilySource".to_owned(),
            Value::String(font.source.to_owned()),
        );
        if let Some(windows_pty) = windows_pty_info() {
            response.insert("windowsPty".to_owned(), windows_pty);
        }
        Ok(Value::Object(response))
    }

    /// 写入排队后立即返回（与 node-pty 相同，没有大小限制与流控）。
    pub fn write(&self, id: &str, data: String) -> Result<(), RpcError> {
        let terminal = self.get(id)?;
        let _ = terminal.input.send(data);
        Ok(())
    }

    pub fn resize(&self, id: &str, cols: f64, rows: f64) -> Result<(), RpcError> {
        let terminal = self.get(id)?;
        let valid = |value: f64| value.is_finite() && value > 0.0;
        if !valid(cols) || !valid(rows) {
            return Err(RpcError::new(
                "Error",
                "resizing must be done using positive cols and rows",
            ));
        }
        if let Some(master) = lock(&terminal.master).as_ref() {
            master
                .resize(pty::size(cols as u16, rows as u16))
                .map_err(|error| RpcError::new("Error", format!("{error:#}")))?;
        }
        Ok(())
    }

    /// POSIX 上发 SIGHUP（与 node-pty `kill()` 相同），Windows 上结束进程并关闭伪控制台。
    pub fn dispose(&self, id: &str) {
        let Some(terminal) = lock(&self.inner.terminals).remove(id) else {
            return;
        };
        terminal.data.close();
        terminal.exit.close();
        // 没有人再读它的输出：不再暂停（Windows 关闭伪控制台要等输出排空）。
        self.inner.flow.close(id);
        if let Err(error) = lock(&terminal.killer).kill() {
            log::debug(format!("terminal {id} kill failed: {error}"));
        }
        if cfg!(windows) {
            tokio::spawn(async move { terminal.close_master().await });
        }
    }

    /// Host 退出：逐个回收仍存活的终端（Node `disposeAll`），子 shell 不随 Host 残留。
    pub fn dispose_all(&self) {
        let ids: Vec<String> = lock(&self.inner.terminals).keys().cloned().collect();
        for id in ids {
            self.dispose(&id);
        }
    }

    /// `connection` 订阅终端 `id` 的输出；订阅流结束时撤销它在流控里的登记。
    fn output(&self, id: &str, connection: u64) -> Result<EventStream, RpcError> {
        let events = Box::pin(self.events(id, false)?.subscribe());
        let reader = self.inner.flow.reader(id, connection);
        Ok(replies(futures_util::stream::unfold(
            (events, reader),
            |(mut events, reader)| async move {
                let value = events.next().await?;
                Some((value, (events, reader)))
            },
        )))
    }

    fn events(&self, id: &str, exit: bool) -> Result<EventHub<Value>, RpcError> {
        let terminal = self.get(id)?;
        Ok(if exit {
            terminal.exit.clone()
        } else {
            terminal.data.clone()
        })
    }
}

/// 子进程退出后：等输出读完（最多 200 ms）、关闭 pty，再发 exit 并移除；已 dispose 的不再发 exit。
async fn supervise(
    owner: Weak<Inner>,
    id: String,
    terminal: Arc<Terminal>,
    mut child: Box<dyn Child + Send + Sync>,
    mut io: PtyIo,
) {
    let status = tokio::task::spawn_blocking(move || child.wait()).await;
    // 子进程已退出：剩余输出有限（PTY 缓冲），不再按流控暂停，照常读完。
    if let Some(inner) = owner.upgrade() {
        inner.flow.close(&id);
    }
    let code = match status {
        // node-pty 只转发 exitCode；被信号终止时它是 0（信号值单独放在被丢弃的 signal 里）。
        Ok(Ok(status)) if status.signal().is_none() => status.exit_code(),
        Ok(Ok(_)) => 0,
        Ok(Err(error)) => {
            log::warn(format!("terminal {id} wait failed: {error}"));
            0
        }
        Err(error) => {
            log::warn(format!("terminal {id} wait task failed: {error}"));
            0
        }
    };
    if cfg!(windows) {
        // ConPTY 只有关闭伪控制台后读端才会结束。
        terminal.close_master().await;
    }
    let _ = tokio::time::timeout(EXIT_FLUSH_TIMEOUT, &mut io.reader_done).await;
    io.close();
    terminal.close_master().await;
    let registered = owner.upgrade().is_some_and(|inner| {
        let mut terminals = lock(&inner.terminals);
        let current = terminals
            .get(&id)
            .is_some_and(|current| Arc::ptr_eq(current, &terminal));
        if current {
            terminals.remove(&id);
        }
        current
    });
    if registered {
        terminal.exit.emit(json!(code));
    }
    terminal.data.close();
    terminal.exit.close();
}

#[derive(Deserialize)]
struct CreateRequest {
    cols: Option<f64>,
    rows: Option<f64>,
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct IdRequest {
    id: String,
}

#[derive(Deserialize)]
struct WriteRequest {
    id: String,
    data: String,
}

#[derive(Deserialize)]
struct ResizeRequest {
    id: String,
    cols: Option<Value>,
    rows: Option<Value>,
}

fn number_or_nan(value: Option<&Value>) -> f64 {
    value.and_then(Value::as_f64).unwrap_or(f64::NAN)
}

pub fn service(terminals: Terminals) -> ServiceBuilder {
    let (create, write, resize, dispose) = (
        terminals.clone(),
        terminals.clone(),
        terminals.clone(),
        terminals.clone(),
    );
    let (data, exit, flow, closed) = (
        terminals.clone(),
        terminals.clone(),
        terminals.inner.flow.clone(),
        terminals.inner.flow.clone(),
    );
    let event_id = |arg: &Value| match arg {
        Value::String(id) => id.clone(),
        other => other.to_string(),
    };
    ServiceBuilder::new("terminal")
        .method("create", move |_, args| {
            let terminals = create.clone();
            let params = args.get::<CreateRequest>(0, "params");
            async move {
                let params = params?;
                let created = terminals
                    .create(params.cols, params.rows, params.cwd.as_deref())
                    .await?;
                Ok(Reply::Value(created))
            }
        })
        .method("write", move |_, args| {
            let result = args
                .get::<WriteRequest>(0, "params")
                .and_then(|params| write.write(&params.id, params.data));
            async move { result.map(|()| Reply::Undefined) }
        })
        .method("resize", move |_, args| {
            let result = args.get::<ResizeRequest>(0, "params").and_then(|params| {
                resize.resize(
                    &params.id,
                    number_or_nan(params.cols.as_ref()),
                    number_or_nan(params.rows.as_ref()),
                )
            });
            async move { result.map(|()| Reply::Undefined) }
        })
        .method("dispose", move |_, args| {
            let result = args
                .get::<IdRequest>(0, "params")
                .map(|params| dispose.dispose(&params.id));
            async move { result.map(|()| Reply::Undefined) }
        })
        // 未知 id：TS 在订阅处理里抛错（不应答）；这里同样拒绝订阅，Host 只记日志。
        .event("onDynamicData", move |context, arg| {
            data.output(&event_id(&arg), context.connection)
        })
        .event("onDynamicExit", move |_, arg| {
            Ok(replies(exit.events(&event_id(&arg), true)?.subscribe()))
        })
        .on_connection_flow(move |context, state| flow.set_state(context.connection, state))
        .on_connection_closed(move |context| closed.connection_closed(context.connection))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::testing::{call, channel, listen, reply_value};
    use zcode_host_kit::{CallContext, FlowState, RpcValue};

    fn terminals(home: &std::path::Path) -> Terminals {
        let env = ProcessEnv::new(
            [
                ("PATH", "/usr/bin:/bin"),
                ("HOME", home.to_str().unwrap()),
                ("SHELL", "/bin/sh"),
                ("PS1", "$ "),
            ]
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
        );
        Terminals::new(
            env,
            SettingStore::new(&zcode_host_base::testing::spawner(), home),
        )
    }

    async fn read_until(events: &mut EventStream, needle: &str) -> String {
        let mut output = String::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !output.contains(needle) {
            match tokio::time::timeout_at(deadline, events.next()).await {
                Ok(Some(reply)) => output.push_str(reply_value(reply).as_str().unwrap()),
                _ => panic!("terminal output did not contain {needle:?}: {output:?}"),
            }
        }
        output
    }

    #[tokio::test]
    async fn runs_a_shell_streams_output_and_reports_exit() {
        let home = tempfile::tempdir().unwrap();
        let terminal = channel(service(terminals(home.path())));
        let created = call(
            &terminal,
            "create",
            json!({"cols": 80, "rows": 24, "cwd": "/nonexistent"}),
        )
        .await
        .unwrap();
        assert_eq!(created["id"], "0");
        assert_eq!(created["shell"], "/bin/sh");
        assert_eq!(created["fontFamilySource"], "fallback");
        assert!(created.get("windowsPty").is_none());
        let id = created["id"].as_str().unwrap().to_owned();
        let mut data = listen(&terminal, "onDynamicData", &id).unwrap();
        let mut exit = listen(&terminal, "onDynamicExit", &id).unwrap();

        call(
            &terminal,
            "resize",
            json!({"id": id, "cols": 100, "rows": 30}),
        )
        .await
        .unwrap();
        let error = call(
            &terminal,
            "resize",
            json!({"id": id, "cols": 0, "rows": 30}),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.message,
            "resizing must be done using positive cols and rows"
        );

        // 失效的 cwd 回退到 HOME；环境按交互终端构造。
        call(&terminal, "write", json!({"id": id, "data": "echo \"[$PWD|$TERM|$(stty size)]\"; printf '\\344\\270\\255\\n'\n"}))
            .await
            .unwrap();
        let home_path = home.path().to_str().unwrap();
        let output = read_until(&mut data, "中").await;
        assert!(
            output.contains(&format!("[{home_path}|xterm-256color|30 100]")),
            "{output:?}"
        );

        call(&terminal, "write", json!({"id": id, "data": "exit 3\n"}))
            .await
            .unwrap();
        let code = tokio::time::timeout(Duration::from_secs(10), exit.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply_value(code), json!(3));
        // 退出后数据流结束、id 失效。
        while tokio::time::timeout(Duration::from_secs(2), data.next())
            .await
            .expect("data stream ends after exit")
            .is_some()
        {}
        let error = call(&terminal, "write", json!({"id": id, "data": "x"}))
            .await
            .unwrap_err();
        assert_eq!(error.message, "Terminal not found: 0");
    }

    fn on_connection(connection: u64) -> CallContext {
        CallContext {
            connection,
            attachment_id: format!("connection-{connection}"),
            client_mode: zcode_host_control::ClientMode::DesktopContinuous,
            window_id: None,
        }
    }

    /// 读到输出停下来（`quiet` 内没有新输出）为止，返回读到的字节数；结束标记出现时返回 None。
    async fn read_until_quiet(
        events: &mut EventStream,
        quiet: Duration,
        end: &str,
    ) -> Option<usize> {
        let mut bytes = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            match tokio::time::timeout(quiet, events.next()).await {
                Ok(Some(reply)) => {
                    let text = reply_value(reply);
                    let text = text.as_str().unwrap();
                    if text.contains(end) {
                        return None;
                    }
                    bytes += text.len();
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "output kept flowing while the reading connection was saturated ({bytes} bytes)"
                    );
                }
                Ok(None) => panic!("terminal output ended"),
                Err(_) => return Some(bytes),
            }
        }
    }

    #[tokio::test]
    async fn output_pauses_while_a_reading_connection_is_saturated() {
        let home = tempfile::tempdir().unwrap();
        let terminal = channel(service(terminals(home.path())));
        let created = call(&terminal, "create", json!({"cols": 80, "rows": 24}))
            .await
            .unwrap();
        let id = created["id"].as_str().unwrap().to_owned();
        let reader = on_connection(1);
        let mut data = terminal
            .listen(&reader, "onDynamicData", RpcValue::String(id.clone()))
            .unwrap();

        // 不读这个终端的连接 saturated：输出照常。
        terminal.connection_flow(&on_connection(2), FlowState::Saturated);
        call(
            &terminal,
            "write",
            json!({"id": id, "data": "echo READY-$((40+2))\n"}),
        )
        .await
        .unwrap();
        read_until(&mut data, "READY-42").await;

        // 远多于 PTY 与读缓冲能容纳的输出；结束标记由 shell 计算，回显的命令行里没有它。
        call(
            &terminal,
            "write",
            json!({"id": id, "data": "head -c 20000000 /dev/zero | tr '\\0' x; echo; echo FLOOD-$((6*7))\n"}),
        )
        .await
        .unwrap();
        read_until(&mut data, "xxxx").await;
        terminal.connection_flow(&reader, FlowState::Saturated);
        // 已经读出的块还会到达；之后 PTY 不再被读，输出停止。
        let delivered = read_until_quiet(&mut data, Duration::from_millis(500), "FLOOD-42")
            .await
            .expect("the flood finished while the reading connection was saturated");
        assert!(
            tokio::time::timeout(Duration::from_millis(800), data.next())
                .await
                .is_err(),
            "output resumed while saturated (after {delivered} bytes)"
        );

        // drained：输出继续到结束。
        terminal.connection_flow(&reader, FlowState::Drained);
        read_until_quiet(&mut data, Duration::from_secs(10), "FLOOD-42").await;

        // saturated 的连接关闭（订阅随之结束）后，不再暂停这个终端。
        terminal.connection_flow(&reader, FlowState::Saturated);
        drop(data);
        terminal.connection_closed(&reader);
        let mut next = terminal
            .listen(
                &on_connection(3),
                "onDynamicData",
                RpcValue::String(id.clone()),
            )
            .unwrap();
        call(
            &terminal,
            "write",
            json!({"id": id, "data": "echo AFTER-$((50+5))\n"}),
        )
        .await
        .unwrap();
        read_until(&mut next, "AFTER-55").await;
        call(&terminal, "dispose", json!({"id": id})).await.unwrap();
    }

    #[tokio::test]
    async fn dispose_kills_without_exit_and_unknown_ids_are_rejected() {
        let home = tempfile::tempdir().unwrap();
        let terminals = terminals(home.path());
        let terminal = channel(service(terminals.clone()));
        let created = call(&terminal, "create", json!({"cols": 80, "rows": 24}))
            .await
            .unwrap();
        let id = created["id"].as_str().unwrap().to_owned();
        let mut exit = listen(&terminal, "onDynamicExit", &id).unwrap();
        call(&terminal, "dispose", json!({"id": id})).await.unwrap();
        // dispose 后不发 exit，订阅直接结束。
        let ended = tokio::time::timeout(Duration::from_secs(5), exit.next())
            .await
            .unwrap();
        assert!(ended.is_none());
        call(&terminal, "dispose", json!({"id": "missing"}))
            .await
            .unwrap();
        assert!(listen(&terminal, "onDynamicData", "missing").is_err());
        let error = call(
            &terminal,
            "resize",
            json!({"id": "missing", "cols": 1, "rows": 1}),
        )
        .await
        .unwrap_err();
        assert_eq!(error.message, "Terminal not found: missing");
        // 子进程确实被结束：表里没有残留。
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(lock(&terminals.inner.terminals).is_empty());
    }
}
