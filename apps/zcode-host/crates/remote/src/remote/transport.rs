//! 远端传输：拿到一条与远端 `zcode-server` 之间的 `@zcode/rpc` 连接（rust-host-remote.md §2.1）。
//!
//! - [`ConnectorTransport`]（产品路径）：每个连接拉起一个 TS 连接器进程（Electron 以 Node 模式运行
//!   `remoteConnector.js`），由它完成探测、部署、启动远端 server 与 hello 握手，之后经二进制帧转发：
//!   `stream u8 | len u32BE | payload`；stream 0 控制 JSON，1 远端 server 的 stdio 字节，2 连接器自身的
//!   rpc 服务（每帧一条消息）。
//! - [`ProcessTransport`]（测试注入）：直接拉起一个说 hello 协议的命令（本地构建的 `zcode-server.cjs`），
//!   Host 自己完成握手（`packages/server/src/remote/handshake.ts`）。
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{OnceCell, mpsc, oneshot, watch};
use zcode_host_control::LogLevel;
use zcode_host_kit::log;
use zcode_host_rpc::{FrameReader, RpcClient, frame};

use crate::agent::command::AgentCommand;
use crate::agent::process::{self, StderrTail};

const STREAM_CONTROL: u8 = 0;
const STREAM_SERVER: u8 = 1;
const STREAM_BACKEND: u8 = 2;
const FRAME_HEADER: usize = 5;
/// 连接器自身的 `disposeAndWait` 上限是 5 s，再给 1.5 s 退出。
const CONNECTOR_DISPOSE_WAIT: Duration = Duration::from_millis(6_500);
/// 连接阶段取消后等待连接器自行退出的时间。
const CANCEL_EXIT_WAIT: Duration = Duration::from_millis(1_500);
/// WSL / Docker 的建连不可中断（同 Node Host）：最多再等一个远端部署锁周期，之后结束进程树。
const CANCELLED_CONNECT_WAIT: Duration = Duration::from_secs(120);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HANDSHAKE_DIAGNOSTIC_CHARS: usize = 2048;
/// 单帧上限，防止损坏的长度字段让 Host 申请超大缓冲。
const MAX_CONNECTOR_FRAME: usize = 64 * 1024 * 1024;

/// 远端连接关闭：远端 server 的退出码，或 backend 断连（-1）；连接器自身异常退出时 `exit_code` 为空。
#[derive(Debug, Clone, PartialEq)]
pub struct CloseEvent {
    pub exit_code: Option<i64>,
    pub error: Option<String>,
}

pub type LogFn = Arc<dyn Fn(LogLevel, String) + Send + Sync>;

/// 一条已握手的远端连接。
pub struct Connection {
    /// 远端 `zcode-server` 的 channel。
    pub client: RpcClient,
    /// 连接器自身的 channel（`remote-backend`、`prompt-attachment-transfer`）；测试传输没有。
    pub backend: Option<RpcClient>,
    /// 远端 `zcode-hello`。
    pub hello: Value,
    closed: watch::Receiver<Option<CloseEvent>>,
    disposer: Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>,
}

impl Connection {
    /// 测试：用现成的 rpc 客户端组装连接；`closed` 置 Some 即底层关闭。
    #[cfg(test)]
    pub(crate) fn for_test(
        client: RpcClient,
        backend: Option<RpcClient>,
        closed: watch::Receiver<Option<CloseEvent>>,
    ) -> Self {
        Self {
            client,
            backend,
            hello: json!({"type": "zcode-hello"}),
            closed,
            disposer: Arc::new(|| -> BoxFuture<'static, ()> { Box::pin(async {}) }),
        }
    }

    /// 底层关闭时完成（只完成一次，之后立即返回同一事件）。
    pub async fn closed(&self) -> CloseEvent {
        let mut closed = self.closed.clone();
        match closed.wait_for(Option::is_some).await {
            Ok(event) => event.clone().unwrap_or(CloseEvent {
                exit_code: None,
                error: None,
            }),
            Err(_) => CloseEvent {
                exit_code: None,
                error: None,
            },
        }
    }

    /// 释放连接：幂等，返回时子进程已退出（或已被强制结束）。
    pub async fn dispose(&self) {
        self.client.close();
        if let Some(backend) = &self.backend {
            backend.close();
        }
        (self.disposer)().await;
    }
}

/// 建连参数。`target` 是含凭据的原文，只交给连接器。
pub struct ConnectParams {
    pub target: Value,
    pub remote_assets: Value,
    pub remote_runtime_network: Option<Value>,
    /// 资源下载的代理设置：`{httpProxy?, noProxy?, caCertPath?}`。
    pub network: Value,
    pub log: LogFn,
    /// 置 true 即取消（连接阶段）。
    pub abort: watch::Receiver<bool>,
}

pub trait Transport: Send + Sync {
    fn connect(&self, params: ConnectParams) -> BoxFuture<'static, Result<Connection, String>>;
}

// ---- 进程与帧 ----

/// 等到 abort 置 true（发送端丢弃时永远等待：没人能再取消）。
pub(crate) async fn aborted(abort: &mut watch::Receiver<bool>) {
    if abort.wait_for(|aborted| *aborted).await.is_err() {
        std::future::pending::<()>().await;
    }
}

struct ChildHandle {
    exit: watch::Receiver<Option<Option<i32>>>,
    kill: Mutex<Option<oneshot::Sender<()>>>,
}

impl ChildHandle {
    /// 子进程由后台任务等待；收到 kill 请求时按进程树回收（与 CLI 相同的升级顺序）。
    fn watch(mut child: tokio::process::Child, pid: Option<u32>) -> Arc<Self> {
        let (exit_tx, exit) = watch::channel(None);
        let (kill_tx, kill_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status.ok(),
                _ = kill_rx => {
                    process::terminate(&mut child, pid).await;
                    child.try_wait().ok().flatten()
                }
            };
            exit_tx.send_replace(Some(status.and_then(|status| status.code())));
        });
        Arc::new(Self {
            exit,
            kill: Mutex::new(Some(kill_tx)),
        })
    }

    async fn wait(&self, timeout: Duration) -> bool {
        let mut exit = self.exit.clone();
        tokio::time::timeout(timeout, exit.wait_for(Option::is_some))
            .await
            .is_ok()
    }

    fn exit_code(&self) -> Option<i32> {
        self.exit.borrow().flatten()
    }

    async fn kill(&self) {
        let sender = self
            .kill
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(sender) = sender {
            let _ = sender.send(());
        }
        let mut exit = self.exit.clone();
        let _ = exit.wait_for(Option::is_some).await;
    }
}

fn encode_frame(stream: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_HEADER + payload.len());
    out.push(stream);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// 写任务：按入队顺序写 stdin；通道关闭时关闭 stdin（连接器把 EOF 当作 dispose）。
fn spawn_writer(mut stdin: ChildStdin) -> mpsc::UnboundedSender<Vec<u8>> {
    let (sender, mut receiver) = mpsc::unbounded_channel::<Vec<u8>>();
    tokio::spawn(async move {
        while let Some(bytes) = receiver.recv().await {
            if stdin.write_all(&bytes).await.is_err() || stdin.flush().await.is_err() {
                return;
            }
        }
        let _ = stdin.shutdown().await;
    });
    sender
}

/// 远端 rpc 客户端的出站：每条消息加 `SocketProtocol` 帧头后交给 `send`。
fn server_client(
    send: impl Fn(Vec<u8>) + Send + 'static,
    inbound: mpsc::UnboundedReceiver<Vec<u8>>,
) -> RpcClient {
    let (outbound, mut messages) = mpsc::unbounded_channel::<Vec<u8>>();
    tokio::spawn(async move {
        while let Some(message) = messages.recv().await {
            send(frame(&message));
        }
    });
    RpcClient::start(outbound, inbound)
}

// ---- 连接器传输 ----

/// 连接器的位置与环境：Electron 可执行文件（以 Node 模式运行）与参数（产品路径为
/// `[remoteConnector.js]`）。
#[derive(Debug, Clone)]
pub struct ConnectorConfig {
    pub node_exec: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
}

pub struct ConnectorTransport {
    config: ConnectorConfig,
}

impl ConnectorTransport {
    pub fn new(config: ConnectorConfig) -> Self {
        Self { config }
    }
}

enum ControlEvent {
    Connected(Value),
    Failed(String),
}

impl Transport for ConnectorTransport {
    fn connect(&self, params: ConnectParams) -> BoxFuture<'static, Result<Connection, String>> {
        let config = self.config.clone();
        Box::pin(async move { connect_via_connector(config, params).await })
    }
}

async fn connect_via_connector(
    config: ConnectorConfig,
    mut params: ConnectParams,
) -> Result<Connection, String> {
    let mut env = config.env.clone();
    env.insert("ELECTRON_RUN_AS_NODE".into(), "1".into());
    let command = AgentCommand {
        program: config.node_exec.clone(),
        args: config.args.clone(),
        cwd: None,
        supports_storage_startup: false,
        embedded: false,
    };
    let spawned = process::spawn(&command, &config.cwd, &env)
        .map_err(|error| format!("无法启动远程连接器：{error}"))?;
    let (stderr, _) = process::drain_stderr(Some(spawned.stderr), "remote-connector".into(), None);
    let child = ChildHandle::watch(spawned.child, spawned.pid);
    let writer = spawn_writer(spawned.stdin);

    let (control_tx, control_rx) = oneshot::channel::<ControlEvent>();
    let (closed_tx, closed) = watch::channel(None::<CloseEvent>);
    let (server_tx, server_rx) = mpsc::unbounded_channel();
    let (backend_tx, backend_rx) = mpsc::unbounded_channel();
    tokio::spawn(read_connector(
        spawned.stdout,
        ConnectorSinks {
            control: Mutex::new(Some(control_tx)),
            closed: closed_tx,
            server: server_tx,
            backend: backend_tx,
            log: params.log.clone(),
            child: child.clone(),
            stderr,
        },
    ));

    let mut connect = json!({
        "type": "connect",
        "target": params.target,
        "remoteAssets": params.remote_assets,
        "network": params.network,
    });
    if let Some(network) = params.remote_runtime_network.take() {
        connect["remoteRuntimeNetwork"] = network;
    }
    let _ = writer.send(encode_frame(STREAM_CONTROL, connect.to_string().as_bytes()));

    let outcome = tokio::select! {
        event = control_rx => event.unwrap_or(ControlEvent::Failed("远程连接器已退出".into())),
        _ = aborted(&mut params.abort) => {
            // 连接阶段取消：连接器释放它独占的 backend 后退出（SSH 立即中止；WSL / Docker 与 Node Host
            // 相同，等建连自然结束再释放）。退出等待放到后台，取消本身立即返回。
            let _ = writer.send(encode_frame(STREAM_CONTROL, br#"{"type":"dispose"}"#));
            drop(writer);
            tokio::spawn(async move {
                if !child.wait(CANCEL_EXIT_WAIT).await {
                    log::info("remote connector is finishing an in-flight connect before exiting");
                    if !child.wait(CANCELLED_CONNECT_WAIT).await {
                        child.kill().await;
                    }
                }
            });
            return Err("远程连接已取消".into());
        }
    };
    let hello = match outcome {
        ControlEvent::Connected(hello) => hello,
        ControlEvent::Failed(error) => {
            drop(writer);
            if !child.wait(CANCEL_EXIT_WAIT).await {
                child.kill().await;
            }
            return Err(error);
        }
    };

    let server_writer = writer.clone();
    let client = server_client(
        move |bytes| {
            let _ = server_writer.send(encode_frame(STREAM_SERVER, &bytes));
        },
        server_rx,
    );
    let (backend_out, mut backend_messages) = mpsc::unbounded_channel::<Vec<u8>>();
    let backend_writer = writer.clone();
    tokio::spawn(async move {
        while let Some(message) = backend_messages.recv().await {
            let _ = backend_writer.send(encode_frame(STREAM_BACKEND, &message));
        }
    });
    let backend = RpcClient::start(backend_out, backend_rx);

    let writer = Arc::new(Mutex::new(Some(writer)));
    let disposed = Arc::new(OnceCell::new());
    let disposer = {
        let (child, writer, disposed) = (child.clone(), writer, disposed);
        Arc::new(move || -> BoxFuture<'static, ()> {
            let (child, writer, disposed) = (child.clone(), writer.clone(), disposed.clone());
            Box::pin(async move {
                disposed
                    .get_or_init(|| async move {
                        let writer = writer
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .take();
                        if let Some(writer) = writer {
                            let _ =
                                writer.send(encode_frame(STREAM_CONTROL, br#"{"type":"dispose"}"#));
                        }
                        if !child.wait(CONNECTOR_DISPOSE_WAIT).await {
                            log::warn("remote connector did not exit after dispose; killing it");
                            child.kill().await;
                        }
                    })
                    .await;
            })
        })
    };
    Ok(Connection {
        client,
        backend: Some(backend),
        hello,
        closed,
        disposer,
    })
}

struct ConnectorSinks {
    control: Mutex<Option<oneshot::Sender<ControlEvent>>>,
    closed: watch::Sender<Option<CloseEvent>>,
    server: mpsc::UnboundedSender<Vec<u8>>,
    backend: mpsc::UnboundedSender<Vec<u8>>,
    log: LogFn,
    child: Arc<ChildHandle>,
    stderr: StderrTail,
}

impl ConnectorSinks {
    fn settle(&self, event: ControlEvent) {
        let sender = self
            .control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(sender) = sender {
            let _ = sender.send(event);
        }
    }

    fn close(&self, event: CloseEvent) {
        self.closed.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(event);
            true
        });
    }

    fn on_control(&self, payload: &[u8]) {
        let Ok(message) = serde_json::from_slice::<Value>(payload) else {
            log::warn("remote connector sent a malformed control frame");
            return;
        };
        let text = |key: &str| message[key].as_str().unwrap_or_default().to_owned();
        match message["type"].as_str() {
            Some("log") => {
                let level = match message["level"].as_str() {
                    Some("warn") => LogLevel::Warn,
                    Some("error") => LogLevel::Error,
                    _ => LogLevel::Info,
                };
                (self.log)(level, text("message"));
            }
            Some("connected") => self.settle(ControlEvent::Connected(message["hello"].clone())),
            Some("failed") => self.settle(ControlEvent::Failed(text("error"))),
            Some("closed") => self.close(CloseEvent {
                exit_code: message["exitCode"].as_i64(),
                error: message["error"].as_str().map(str::to_owned),
            }),
            _ => {}
        }
    }
}

async fn read_connector(mut stdout: ChildStdout, sinks: ConnectorSinks) {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let mut server = FrameReader::default();
    'read: loop {
        let read = match stdout.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        buffer.extend_from_slice(&chunk[..read]);
        let mut offset = 0;
        while buffer.len() - offset >= FRAME_HEADER {
            let stream = buffer[offset];
            let mut length = [0u8; 4];
            length.copy_from_slice(&buffer[offset + 1..offset + FRAME_HEADER]);
            let length = u32::from_be_bytes(length) as usize;
            if length > MAX_CONNECTOR_FRAME {
                log::error("remote connector frame exceeds the size limit; closing");
                break 'read;
            }
            if buffer.len() - offset < FRAME_HEADER + length {
                break;
            }
            let payload = &buffer[offset + FRAME_HEADER..offset + FRAME_HEADER + length];
            match stream {
                STREAM_CONTROL => sinks.on_control(payload),
                STREAM_SERVER => {
                    server.push(payload);
                    while let Some(message) = server.next_message() {
                        let _ = sinks.server.send(message);
                    }
                }
                STREAM_BACKEND => {
                    let _ = sinks.backend.send(payload.to_vec());
                }
                _ => {}
            }
            offset += FRAME_HEADER + length;
        }
        buffer.drain(..offset);
    }
    // stdout 结束：连接器退出（或被结束）。连接阶段的等待者收到失败，已连接的连接收到关闭。
    sinks.child.wait(Duration::from_secs(2)).await;
    let code = sinks.child.exit_code();
    let tail = sinks.stderr.lines().join("\n");
    let error = if tail.is_empty() {
        format!("远程连接器已退出（exit code {code:?}）")
    } else {
        format!("远程连接器已退出（exit code {code:?}）：{tail}")
    };
    sinks.settle(ControlEvent::Failed(error.clone()));
    sinks.close(CloseEvent {
        exit_code: None,
        error: Some(error),
    });
}

/// 缺少连接器配置时的传输：建连明确失败，不静默回退。
pub struct UnavailableTransport;

impl Transport for UnavailableTransport {
    fn connect(&self, _params: ConnectParams) -> BoxFuture<'static, Result<Connection, String>> {
        Box::pin(async {
            Err("远程连接器不可用：Main 未提供 ZCODE_REMOTE_CONNECTOR_PATH 或 ZCODE_HOST_NODE_EXEC_PATH".into())
        })
    }
}

// ---- 测试传输：直接拉起说 hello 协议的命令 ----

#[derive(Debug, Clone)]
pub struct ProcessTransport {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
}

impl Transport for ProcessTransport {
    fn connect(&self, params: ConnectParams) -> BoxFuture<'static, Result<Connection, String>> {
        let config = self.clone();
        Box::pin(async move { connect_process(config, params).await })
    }
}

async fn connect_process(
    config: ProcessTransport,
    mut params: ConnectParams,
) -> Result<Connection, String> {
    let command = AgentCommand {
        program: config.program.clone(),
        args: config.args.clone(),
        cwd: None,
        supports_storage_startup: false,
        embedded: false,
    };
    let spawned = process::spawn(&command, &config.cwd, &config.env)
        .map_err(|error| format!("无法启动远端 server：{error}"))?;
    let (stderr, _) = process::drain_stderr(Some(spawned.stderr), "remote-server".into(), None);
    let child = ChildHandle::watch(spawned.child, spawned.pid);
    let writer = spawn_writer(spawned.stdin);
    let mut stdout = spawned.stdout;
    let client_id = format!("rust-host-{}", std::process::id());
    let handshake = tokio::select! {
        result = perform_handshake(&mut stdout, &stderr, &child) => result,
        _ = aborted(&mut params.abort) => Err("远程连接已取消".into()),
    };
    let (hello, remaining) = match handshake {
        Ok(result) => result,
        Err(error) => {
            drop(writer);
            child.kill().await;
            return Err(error);
        }
    };
    let ack = json!({"type": "zcode-hello-ack", "version": env!("CARGO_PKG_VERSION"), "clientId": client_id});
    let _ = writer.send(format!("{ack}\n").into_bytes());
    (params.log)(
        LogLevel::Info,
        format!("handshake done, server version: {}", hello["version"]),
    );

    let (server_tx, server_rx) = mpsc::unbounded_channel();
    let (closed_tx, closed) = watch::channel(None::<CloseEvent>);
    {
        let child = child.clone();
        tokio::spawn(async move {
            let mut reader = FrameReader::default();
            reader.push(&remaining);
            while let Some(message) = reader.next_message() {
                let _ = server_tx.send(message);
            }
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                match stdout.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        reader.push(&chunk[..read]);
                        while let Some(message) = reader.next_message() {
                            let _ = server_tx.send(message);
                        }
                    }
                }
            }
            child.wait(Duration::from_secs(5)).await;
            closed_tx.send_replace(Some(CloseEvent {
                exit_code: child.exit_code().map(i64::from),
                error: None,
            }));
        });
    }
    let server_writer = writer.clone();
    let client = server_client(
        move |bytes| {
            let _ = server_writer.send(bytes);
        },
        server_rx,
    );
    let writer = Arc::new(Mutex::new(Some(writer)));
    let disposed = Arc::new(OnceCell::new());
    let disposer = Arc::new(move || -> BoxFuture<'static, ()> {
        let (child, writer, disposed) = (child.clone(), writer.clone(), disposed.clone());
        Box::pin(async move {
            disposed
                .get_or_init(|| async move {
                    // stdin EOF 让远端 stdio server 自行收口（stdio-lifecycle）；超时再结束进程树。
                    writer
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take();
                    if !child.wait(Duration::from_secs(5)).await {
                        child.kill().await;
                    }
                })
                .await;
        })
    });
    Ok(Connection {
        client,
        backend: None,
        hello,
        closed,
        disposer,
    })
}

fn append_limited(current: &mut String, next: &str) {
    current.push_str(next);
    let count = current.chars().count();
    if count > MAX_HANDSHAKE_DIAGNOSTIC_CHARS {
        *current = current
            .chars()
            .skip(count - MAX_HANDSHAKE_DIAGNOSTIC_CHARS)
            .collect();
    }
}

/// `helloMessageSchema`：type、version、platform、arch 为字符串，pid 为整数。
fn valid_hello(value: &Value) -> Result<(), String> {
    for key in ["version", "platform", "arch"] {
        if !value[key].is_string() {
            return Err(format!("{key}: expected string"));
        }
    }
    if !value["pid"].is_i64() && !value["pid"].is_u64() {
        return Err("pid: expected integer".into());
    }
    Ok(())
}

/// 逐行读 stdout 直到出现合法的 `zcode-hello`（跳过 SSH banner 等非 JSON 行），返回 hello 与
/// 同一块数据里 hello 行之后的剩余字节（属于 rpc 字节流）。
async fn perform_handshake(
    stdout: &mut (impl AsyncRead + Unpin),
    stderr: &StderrTail,
    child: &ChildHandle,
) -> Result<(Value, Vec<u8>), String> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut stdout_text = String::new();
    let mut chunk = vec![0u8; 16 * 1024];
    let deadline = tokio::time::Instant::now() + HANDSHAKE_TIMEOUT;
    let failure = |message: &str, exit_code: Option<i32>, stdout_text: &str| {
        let mut parts = vec![message.to_owned()];
        if let Some(code) = exit_code {
            parts.push(format!("exit code {code}"));
        }
        let stderr_text = stderr.lines().join("\n");
        if !stderr_text.trim().is_empty() {
            parts.push(format!("stderr: {}", json!(stderr_text.trim())));
        }
        if !stdout_text.trim().is_empty() {
            parts.push(format!("stdout: {}", json!(stdout_text.trim())));
        }
        if parts.len() == 1 {
            message.to_owned()
        } else {
            format!("{message} ({})", parts[1..].join("; "))
        }
    };
    loop {
        let read = match tokio::time::timeout_at(deadline, stdout.read(&mut chunk)).await {
            Err(_) => {
                return Err(failure(
                    "Handshake timeout: no zcode-hello received within timeout",
                    None,
                    &stdout_text,
                ));
            }
            Ok(Ok(0)) | Ok(Err(_)) => {
                child.wait(Duration::from_secs(1)).await;
                return Err(failure(
                    "Stream closed before handshake completed",
                    child.exit_code(),
                    &stdout_text,
                ));
            }
            Ok(Ok(read)) => read,
        };
        append_limited(&mut stdout_text, &String::from_utf8_lossy(&chunk[..read]));
        buffer.extend_from_slice(&chunk[..read]);
        while let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffer.drain(..=newline).collect();
            let text = String::from_utf8_lossy(&line);
            let text = text.trim();
            if !text.starts_with('{') {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(text) else {
                continue;
            };
            if value["type"] != "zcode-hello" {
                continue;
            }
            return match valid_hello(&value) {
                Ok(()) => Ok((value, buffer)),
                Err(detail) => Err(failure(
                    &format!("Invalid zcode-hello: {detail}"),
                    None,
                    &stdout_text,
                )),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connector_frames_are_length_prefixed() {
        assert_eq!(encode_frame(2, b"ab"), vec![2, 0, 0, 0, 2, b'a', b'b']);
    }

    #[test]
    fn hello_schema_requires_all_fields() {
        assert!(
            valid_hello(
                &json!({"type":"zcode-hello","version":"1","platform":"linux","arch":"x64","pid":3})
            )
            .is_ok()
        );
        assert!(valid_hello(&json!({"type":"zcode-hello","version":"1","platform":"linux","arch":"x64","pid":"3"})).is_err());
        assert!(
            valid_hello(&json!({"type":"zcode-hello","platform":"linux","arch":"x64","pid":3}))
                .is_err()
        );
    }

    /// 握手跳过 banner，剩余字节原样交给 rpc 流；超时与提前关闭给出带诊断的错误。
    #[cfg(unix)]
    #[tokio::test]
    async fn process_transport_handshakes_and_reports_failures() {
        use futures_util::StreamExt as _;
        let script = r#"printf 'Welcome banner\n{"type":"zcode-hello","version":"9.9.9","platform":"linux","arch":"x64","pid":1}\n'; read ack; printf "%s\n" "$ack" >&2; cat >/dev/null"#;
        let transport = ProcessTransport {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            env: [("PATH".to_owned(), "/usr/bin:/bin".to_owned())].into(),
            cwd: std::env::temp_dir(),
        };
        let (_abort_tx, abort) = watch::channel(false);
        let logs = Arc::new(Mutex::new(Vec::new()));
        let sink = logs.clone();
        let connection = transport
            .connect(ConnectParams {
                target: json!({"kind": "docker", "container": "c"}),
                remote_assets: json!({}),
                remote_runtime_network: None,
                network: json!({}),
                log: Arc::new(move |_, message| sink.lock().unwrap().push(message)),
                abort,
            })
            .await
            .unwrap();
        assert_eq!(connection.hello["version"], "9.9.9");
        assert!(logs.lock().unwrap()[0].contains("9.9.9"));
        connection.dispose().await;
        let event = connection.closed().await;
        assert!(event.error.is_none());
        let _ = futures_util::stream::empty::<()>().next().await;

        let failing = ProcessTransport {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "echo boom >&2; echo not-json; exit 3".into()],
            env: [("PATH".to_owned(), "/usr/bin:/bin".to_owned())].into(),
            cwd: std::env::temp_dir(),
        };
        let (_abort_tx, abort) = watch::channel(false);
        let error = failing
            .connect(ConnectParams {
                target: json!({}),
                remote_assets: json!({}),
                remote_runtime_network: None,
                network: json!({}),
                log: Arc::new(|_, _| {}),
                abort,
            })
            .await
            .err()
            .unwrap();
        assert!(
            error.starts_with("Stream closed before handshake completed"),
            "{error}"
        );
        assert!(error.contains("exit code 3"), "{error}");
        assert!(error.contains("stdout: \"not-json\""), "{error}");
    }
}
