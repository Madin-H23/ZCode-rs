//! 端到端：拉起真实的 zcode-host，经 stdin/stdout 控制通道和 WebSocket 走完 R0 验收场景
//! （docs/specs/rust-host-r0.md §6）。
use std::process::Stdio;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::TcpStream;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use zcode_host_rpc::{FrameReader, Reader, RpcValue, codec, frame};

const DEV_ORIGIN: &str = "http://localhost:5173";
const STEP_TIMEOUT: Duration = Duration::from_secs(5);

struct Host {
    /// 测试 Host 的 home 与数据根：绝不能读写开发机真实的 ~/.zcode。
    _home: tempfile::TempDir,
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    port: u16,
}

impl Host {
    async fn spawn() -> Self {
        let home = tempfile::tempdir().expect("temp home");
        let mut child = Command::new(env!("CARGO_BIN_EXE_zcode-host"))
            .args([
                "--allowed-origin",
                DEV_ORIGIN,
                "--allowed-origin",
                "file://",
            ])
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("ZCODE_DESKTOP_HOME_DIR", home.path())
            .env("ZCODE_DATA_BASE_DIR", home.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn zcode-host");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut host = Self {
            _home: home,
            child,
            stdin,
            stdout,
            port: 0,
        };
        let ready = host.recv().await;
        assert_eq!(ready["type"], "host-endpoint-ready");
        host.port = ready["wsPort"].as_u64().unwrap() as u16;
        host
    }

    async fn send(&mut self, message: Value) {
        let line = format!("{message}\n");
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    /// 下一条非日志控制消息；结构化日志只校验形状后跳过。
    async fn recv(&mut self) -> Value {
        loop {
            let line = tokio::time::timeout(STEP_TIMEOUT, self.stdout.next_line())
                .await
                .expect("control message within timeout")
                .unwrap()
                .expect("control stdout open");
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["type"] != "log" {
                return message;
            }
            assert_eq!(message["source"], "zcode-host");
            assert!(matches!(
                message["level"].as_str(),
                Some("info" | "warn" | "error")
            ));
        }
    }

    async fn init(&mut self) {
        self.send(json!({"type": "init-local", "databaseStartupId": "startup-1", "zcodeBuiltinProviderConfigFilePath": "/x"}))
            .await;
        // starting → preparing_host_storage（tasks-index 迁移，带 databasePhase）→ starting_services → ready
        let mut phases = Vec::new();
        let mut last_sequence = None;
        loop {
            let state = self.recv().await;
            assert_eq!(state["type"], "database-startup-state");
            assert_eq!(state["state"]["startupId"], "startup-1");
            assert_eq!(state["state"]["schemaVersion"], 1);
            let sequence = state["state"]["sequence"].as_u64().unwrap();
            assert!(last_sequence.is_none_or(|last| sequence > last));
            last_sequence = Some(sequence);
            let phase = state["state"]["phase"].as_str().unwrap().to_owned();
            if phase == "preparing_host_storage" {
                assert!(state["state"]["databasePhase"].is_string());
            }
            phases.push(phase.clone());
            if phase == "ready" || phase == "failed" {
                break;
            }
        }
        assert_eq!(phases.first().map(String::as_str), Some("starting"));
        assert!(phases.iter().any(|phase| phase == "preparing_host_storage"));
        assert_eq!(
            &phases[phases.len() - 2..],
            ["starting_services", "ready"],
            "phases: {phases:?}"
        );
    }

    async fn issue(&mut self) -> (String, String) {
        self.send(json!({
            "type": "issue-attachment-endpoint",
            "requestId": "r1",
            "attachmentId": "attachment-1",
            "clientMode": "desktop-continuous",
            "scope": {"kind": "local"},
        }))
        .await;
        let issued = self.recv().await;
        assert_eq!(issued["type"], "attachment-endpoint-issued");
        assert_eq!(issued["requestId"], "r1");
        assert_eq!(issued["ok"], true, "{issued}");
        let url = issued["url"].as_str().unwrap().to_owned();
        assert_eq!(url, format!("ws://127.0.0.1:{}/rpc", self.port));
        (url, issued["capability"].as_str().unwrap().to_owned())
    }
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn request(url: &str, protocols: &str, origin: Option<&str>) -> Request {
    let mut request = url.into_client_request().unwrap();
    let headers = request.headers_mut();
    headers.insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_str(protocols).unwrap(),
    );
    if let Some(origin) = origin {
        headers.insert("Origin", HeaderValue::from_str(origin).unwrap());
    }
    request
}

fn protocols(capability: &str) -> String {
    format!("zcode-rpc.v1, zcode-cap.{capability}")
}

async fn connect(request: Request) -> Result<Socket, WsError> {
    tokio_tungstenite::connect_async(request)
        .await
        .map(|(socket, _)| socket)
}

fn assert_forbidden(result: Result<Socket, WsError>) {
    match result {
        Err(WsError::Http(response)) => assert_eq!(response.status(), 403),
        Err(other) => panic!("expected 403, got {other}"),
        Ok(_) => panic!("expected 403, handshake succeeded"),
    }
}

struct Rpc {
    socket: Socket,
    frames: FrameReader,
}

impl Rpc {
    async fn send(&mut self, header: Value, body: Option<Value>) {
        let mut message = Vec::new();
        codec::write_value(&mut message, &header);
        match body {
            Some(body) => codec::write_value(&mut message, &body),
            None => codec::write_undefined(&mut message),
        }
        self.socket
            .send(Message::Binary(frame(&message).into()))
            .await
            .unwrap();
    }

    async fn next_within(&mut self, limit: Duration) -> Option<(Value, RpcValue)> {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(message) = self.frames.next_message() {
                let mut reader = Reader::new(&message);
                let header = reader.value().unwrap().into_json();
                return Some((header, reader.value().unwrap()));
            }
            let remaining = deadline.checked_duration_since(Instant::now())?;
            match tokio::time::timeout(remaining, self.socket.next()).await {
                Ok(Some(Ok(Message::Binary(data)))) => self.frames.push(&data),
                Ok(Some(Ok(_))) => {}
                Ok(other) => panic!("socket ended: {other:?}"),
                Err(_) => return None,
            }
        }
    }

    async fn next(&mut self) -> (Value, RpcValue) {
        self.next_within(STEP_TIMEOUT)
            .await
            .expect("rpc reply within timeout")
    }
}

async fn open_rpc(host: &mut Host) -> Rpc {
    let (url, capability) = host.issue().await;
    let socket = connect(request(&url, &protocols(&capability), Some(DEV_ORIGIN)))
        .await
        .expect("handshake accepted");
    Rpc {
        socket,
        frames: FrameReader::default(),
    }
}

#[tokio::test]
async fn renderer_attachment_gets_initialize_and_immediate_stub_replies() {
    let mut host = Host::spawn().await;
    host.init().await;
    let mut rpc = open_rpc(&mut host).await;
    assert_eq!(rpc.next().await.0, json!([200]));

    let started = Instant::now();
    // desktop-local Host 不注册 provider-provisioning-target（与 Node 相同），始终是 contract 桩。
    rpc.send(
        json!([100, 1, "provider-provisioning-target", "apply"]),
        Some(json!([{}])),
    )
    .await;
    let (header, body) = rpc.next().await;
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "stub reply took {:?}",
        started.elapsed()
    );
    assert_eq!(header, json!([202, 1]));
    let body = body.into_json();
    assert_eq!(body["name"], "NotImplemented");
    assert_eq!(body["code"], "fault.host.notImplemented");

    rpc.send(
        json!([100, 2, "settings-sync", "getFirstRunPromptState"]),
        Some(json!([])),
    )
    .await;
    let (header, body) = rpc.next().await;
    assert_eq!(header, json!([201, 2]));
    // 真实实现：临时 home 下首启导入提示尚未处理。
    assert_eq!(body.into_json(), json!({"handled": false}));

    // 已实现的 service：临时 home 下没有设置文件，返回默认值。
    rpc.send(json!([100, 9, "setting", "get"]), Some(json!([])))
        .await;
    let (header, body) = rpc.next().await;
    assert_eq!(header, json!([201, 9]));
    assert_eq!(body.into_json()["locale"], "zh-CN");

    rpc.send(json!([100, 3, "no-such-channel", "x"]), Some(json!([])))
        .await;
    let (header, body) = rpc.next().await;
    assert_eq!(header, json!([202, 3]));
    assert_eq!(body.into_json()["name"], "Unknown channel");

    // 订阅没有应答，也不会卡住后续调用。
    rpc.send(
        json!([102, 4, "zcode-agent", "onDynamicConversationFrame"]),
        Some(json!({"workspacePath": "/w"})),
    )
    .await;
    rpc.send(json!([100, 5, "file", "stat"]), Some(json!(["/tmp"])))
        .await;
    assert_eq!(rpc.next().await.0, json!([202, 5]));
}

#[tokio::test]
async fn capability_is_single_use_and_handshake_is_checked() {
    let mut host = Host::spawn().await;
    host.init().await;
    let (url, capability) = host.issue().await;

    // 不合规的请求在兑换前就被拒绝，不会耗掉 capability。
    assert_forbidden(
        connect(request(
            &url,
            &protocols(&capability),
            Some("https://evil.example"),
        ))
        .await,
    );
    let mut wrong_host = request(&url, &protocols(&capability), Some(DEV_ORIGIN));
    wrong_host.headers_mut().insert(
        "Host",
        HeaderValue::from_str(&format!("localhost:{}", host.port)).unwrap(),
    );
    assert_forbidden(connect(wrong_host).await);
    assert_forbidden(
        connect(request(
            &url,
            &format!("zcode-cap.{capability}"),
            Some(DEV_ORIGIN),
        ))
        .await,
    );
    let wrong_path = url.replace("/rpc", "/other");
    assert_forbidden(
        connect(request(
            &wrong_path,
            &protocols(&capability),
            Some(DEV_ORIGIN),
        ))
        .await,
    );
    assert_forbidden(connect(request(&url, &protocols(&"0".repeat(64)), Some(DEV_ORIGIN))).await);

    // file:// 页面的 Origin 可能是 "null"；没有 Origin 的非浏览器客户端仍须出示 capability。
    let first = connect(request(&url, &protocols(&capability), Some("null"))).await;
    assert!(first.is_ok(), "valid capability accepted");
    assert_forbidden(connect(request(&url, &protocols(&capability), Some(DEV_ORIGIN))).await);
    let (url, second) = host.issue().await;
    assert!(
        connect(request(&url, &protocols(&second), None))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn attachments_wait_for_init_local() {
    let mut host = Host::spawn().await;
    let mut rpc = open_rpc(&mut host).await;
    assert!(
        rpc.next_within(Duration::from_millis(200)).await.is_none(),
        "no Initialize before init-local"
    );
    host.init().await;
    assert_eq!(rpc.next().await.0, json!([200]));
}

#[tokio::test]
async fn invalid_and_unsupported_issue_requests_fail_explicitly() {
    let mut host = Host::spawn().await;
    host.send(json!({
        "type": "issue-attachment-endpoint",
        "requestId": "r-remote",
        "attachmentId": "a",
        "clientMode": "desktop-continuous",
        "scope": {"kind": "remote", "sessionId": "s"},
    }))
    .await;
    let failed = host.recv().await;
    assert_eq!(failed["requestId"], "r-remote");
    assert_eq!(failed["ok"], false);

    host.send(json!({
        "type": "issue-attachment-endpoint",
        "requestId": "r-replayable",
        "attachmentId": "a",
        "clientMode": "web-remote-replayable",
        "scope": {"kind": "local"},
    }))
    .await;
    let failed = host.recv().await;
    assert_eq!(failed["requestId"], "r-replayable");
    assert_eq!(failed["ok"], false);
}

async fn wait_exit(child: &mut Child) -> std::process::ExitStatus {
    tokio::time::timeout(STEP_TIMEOUT, child.wait())
        .await
        .expect("host exits within timeout")
        .unwrap()
}

#[tokio::test]
async fn dispose_and_closed_stdin_exit_cleanly() {
    let mut host = Host::spawn().await;
    host.init().await;
    host.send(json!({"type": "broadcast", "payload": {}})).await;
    host.send(json!({"type": "dispose"})).await;
    assert!(wait_exit(&mut host.child).await.success());

    let host = Host::spawn().await;
    let Host {
        mut child,
        stdin,
        _home,
        ..
    } = host;
    drop(stdin);
    assert!(wait_exit(&mut child).await.success());
}
