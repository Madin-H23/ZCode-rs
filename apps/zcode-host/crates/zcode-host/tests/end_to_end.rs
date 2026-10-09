//! 端到端：拉起真实的 zcode-host，经 stdin/stdout 控制通道和 WebSocket 走完 R0 验收场景
//! （docs/specs/rust-host-r0.md §6）。
use std::process::Stdio;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::TcpStream;
use tokio::process::{Child, ChildStdin, ChildStdout};
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
        let mut child = zcode_proc::command(env!("CARGO_BIN_EXE_zcode-host"))
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
        self.issue_mode("desktop-continuous").await
    }

    /// 签发一个本地 attachment；`window_id` 是 Main 给的主窗口 id（rust-u4-app-host.md §2.2）。
    async fn issue_for_window(
        &mut self,
        attachment_id: &str,
        window_id: Option<i64>,
    ) -> (String, String) {
        let mut request = json!({
            "type": "issue-attachment-endpoint",
            "requestId": format!("issue-{attachment_id}"),
            "attachmentId": attachment_id,
            "clientMode": "desktop-continuous",
            "scope": {"kind": "local"},
        });
        if let Some(window_id) = window_id {
            request["windowId"] = json!(window_id);
        }
        self.send(request).await;
        let issued = self.recv_kind(&["attachment-endpoint-issued"]).await;
        assert_eq!(issued["ok"], true, "{issued}");
        (
            issued["url"].as_str().unwrap().to_owned(),
            issued["capability"].as_str().unwrap().to_owned(),
        )
    }

    /// 下一条类型在 `kinds` 里的控制消息；其余（遥测等异步上报）跳过。
    async fn recv_kind(&mut self, kinds: &[&str]) -> Value {
        loop {
            let message = self.recv().await;
            if kinds.iter().any(|kind| message["type"] == *kind) {
                return message;
            }
        }
    }

    async fn issue_mode(&mut self, client_mode: &str) -> (String, String) {
        self.send(json!({
            "type": "issue-attachment-endpoint",
            "requestId": "r1",
            "attachmentId": "attachment-1",
            "clientMode": client_mode,
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

impl Rpc {
    /// 下一条 id 为 `id` 的应答（Promise 结果或事件），其余消息按到达顺序留给 `pending`。
    async fn reply_to(
        &mut self,
        id: i64,
        pending: &mut Vec<(Value, RpcValue)>,
    ) -> (Value, RpcValue) {
        if let Some(index) = pending.iter().position(|(header, _)| header[1] == id) {
            return pending.remove(index);
        }
        loop {
            let (header, body) = self.next().await;
            if header[1] == id {
                return (header, body);
            }
            pending.push((header, body));
        }
    }

    /// 调用并等待结果；期间到达的事件留在 `pending`。
    async fn call(
        &mut self,
        id: i64,
        channel: &str,
        method: &str,
        args: Value,
        pending: &mut Vec<(Value, RpcValue)>,
    ) -> Value {
        self.send(json!([100, id, channel, method]), Some(args))
            .await;
        let (header, body) = self.reply_to(id, pending).await;
        assert_eq!(header, json!([201, id]), "{}", body.into_json());
        body.into_json()
    }
}

async fn open_window_rpc(host: &mut Host, attachment_id: &str, window_id: Option<i64>) -> Rpc {
    let (url, capability) = host.issue_for_window(attachment_id, window_id).await;
    let socket = connect(request(&url, &protocols(&capability), Some(DEV_ORIGIN)))
        .await
        .expect("handshake accepted");
    let mut rpc = Rpc {
        socket,
        frames: FrameReader::default(),
    };
    assert_eq!(rpc.next().await.0, json!([200]));
    rpc
}

/// 两个窗口与一条没有窗口身份的连接接入同一个 Host：`windowId` 经真实 WebSocket attach 进入调用上下文，
/// 广播按窗口身份扇出，claim 跨窗口先到先得、连接关闭时回收（rust-u4-app-host.md §2.2、§2.3）。
#[tokio::test]
async fn windows_share_one_host_and_broadcasts_follow_window_identity() {
    let mut host = Host::spawn().await;
    host.init().await;
    let mut window_1 = open_window_rpc(&mut host, "w1", Some(1)).await;
    let mut window_2 = open_window_rpc(&mut host, "w2", Some(2)).await;
    let mut windowless = open_window_rpc(&mut host, "phone", None).await;
    let mut pending = [Vec::new(), Vec::new(), Vec::new()];
    for (index, rpc) in [&mut window_1, &mut window_2, &mut windowless]
        .into_iter()
        .enumerate()
    {
        rpc.send(json!([102, 1, "broadcast", "onMessage"]), None)
            .await;
        // 同一连接按序处理：get 应答到达时订阅已经登记。
        rpc.call(2, "setting", "get", json!([]), &mut pending[index])
            .await;
    }

    let message = json!({"channel": "state:locale", "payload": "en-US"});
    window_1
        .call(3, "broadcast", "send", json!([message]), &mut pending[0])
        .await;
    let event = |reply: (Value, RpcValue)| {
        assert_eq!(reply.0, json!([204, 1]));
        reply.1.into_json()
    };
    assert_eq!(event(window_1.reply_to(1, &mut pending[0]).await), message);
    let mut from_window_1 = message.clone();
    from_window_1["sourceWindowId"] = json!(1);
    assert_eq!(
        event(window_2.reply_to(1, &mut pending[1]).await),
        from_window_1
    );
    assert_eq!(
        event(windowless.reply_to(1, &mut pending[2]).await),
        from_window_1
    );

    let message = json!({"channel": "code-comment:remove", "payload": {"id": "c"}});
    windowless
        .call(3, "broadcast", "send", json!([message]), &mut pending[2])
        .await;
    assert_eq!(
        event(windowless.reply_to(1, &mut pending[2]).await),
        message
    );
    let mut from_windowless = message.clone();
    from_windowless["sourceWindowId"] = json!(0);
    assert_eq!(
        event(window_1.reply_to(1, &mut pending[0]).await),
        from_windowless
    );
    assert_eq!(
        event(window_2.reply_to(1, &mut pending[1]).await),
        from_windowless
    );

    // claim：窗口 1 先拿到，窗口 2 busy；窗口 1 的连接关闭后它的 reservation 被回收。
    let acquired = window_1
        .call(
            4,
            "broadcast",
            "acquireClaim",
            json!(["reset:week"]),
            &mut pending[0],
        )
        .await;
    assert_eq!(acquired["status"], "acquired", "{acquired}");
    let busy = window_2
        .call(
            4,
            "broadcast",
            "acquireClaim",
            json!(["reset:week"]),
            &mut pending[1],
        )
        .await;
    assert_eq!(busy["status"], "busy", "{busy}");
    drop(window_1);
    let deadline = Instant::now() + STEP_TIMEOUT;
    let mut id = 5;
    loop {
        let result = window_2
            .call(
                id,
                "broadcast",
                "acquireClaim",
                json!(["reset:week"]),
                &mut pending[1],
            )
            .await;
        if result["status"] == "acquired" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "reservation not released: {result}"
        );
        id += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Main 的 `setting-write`：结果先于它引起的 `setting-changed`；Renderer 经 `setting` channel 的写入同样
/// 推送完整设置（rust-u4-app-host.md §2.6）。
#[tokio::test]
async fn setting_writes_from_main_and_renderer_publish_setting_changed() {
    let mut host = Host::spawn().await;
    host.init().await;
    const SETTING_MESSAGES: &[&str] = &["setting-write-result", "setting-changed"];
    host.send(
        json!({"type": "setting-write", "requestId": "zoom-1", "patch": {"desktopZoomLevel": 2}}),
    )
    .await;
    assert_eq!(
        host.recv_kind(SETTING_MESSAGES).await,
        json!({"type": "setting-write-result", "requestId": "zoom-1", "ok": true})
    );
    let changed = host.recv_kind(SETTING_MESSAGES).await;
    assert_eq!(changed["type"], "setting-changed");
    assert_eq!(changed["setting"]["desktopZoomLevel"], 2);
    let saved: Value = serde_json::from_str(
        &std::fs::read_to_string(host._home.path().join(".zcode/v2/setting.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["desktopZoomLevel"], 2);

    let mut rpc = open_window_rpc(&mut host, "w1", Some(1)).await;
    let mut pending = Vec::new();
    rpc.call(
        1,
        "setting",
        "update",
        json!([{"localePreference": "en-US"}]),
        &mut pending,
    )
    .await;
    let changed = host.recv_kind(SETTING_MESSAGES).await;
    assert_eq!(changed["type"], "setting-changed");
    assert_eq!(changed["setting"]["localePreference"], "en-US");
    assert_eq!(changed["setting"]["desktopZoomLevel"], 2);
    let current = rpc.call(2, "setting", "get", json!([]), &mut pending).await;
    assert_eq!(changed["setting"], current);
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
    // windowId 必须是 ≥ 1 的整数（rust-u4-app-host.md §4.1）。
    for (request_id, window_id) in [
        ("r-zero", json!(0)),
        ("r-float", json!(1.5)),
        ("r-text", json!("1")),
    ] {
        host.send(json!({
            "type": "issue-attachment-endpoint",
            "requestId": request_id,
            "attachmentId": "a",
            "clientMode": "desktop-continuous",
            "scope": {"kind": "local"},
            "windowId": window_id,
        }))
        .await;
        let failed = host.recv().await;
        assert_eq!(failed["requestId"], request_id);
        assert_eq!(failed["ok"], false);
    }
}

/// web-remote-replayable 的本地 attachment：可签发，握手 hello 报告 replayable 投递与受限能力。
#[tokio::test]
async fn replayable_attachment_reports_replayable_delivery() {
    let mut host = Host::spawn().await;
    host.init().await;
    let (url, capability) = host.issue_mode("web-remote-replayable").await;
    let socket = connect(request(&url, &protocols(&capability), None))
        .await
        .expect("non-browser client without Origin is accepted");
    let mut rpc = Rpc {
        socket,
        frames: FrameReader::default(),
    };
    assert_eq!(rpc.next().await.0, json!([200]));
    rpc.send(
        json!([100, 1, "zcode-agent", "helloConversationV4"]),
        Some(json!([{"workspacePath": "/w"}])),
    )
    .await;
    let (header, body) = rpc.next().await;
    assert_eq!(header, json!([201, 1]));
    let hello = body.into_json();
    assert_eq!(hello["clientMode"], "web-remote-replayable");
    assert_eq!(hello["deliveryProfile"], "replayable");
    assert_eq!(hello["capabilities"]["nativeDialogs"], false);
    assert_eq!(hello["capabilities"]["localTerminal"], false);
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
