//! Gateway 主动关闭连接时 WebSocket Close 带上原因（docs/specs/rust-u2-gateway.md §2.6）：真实
//! zcode-host 二进制，经控制通道与真实 WebSocket。
//!
//! - 出站积压越过上限 → 4001 `outbound-backlog`（调试构建以 `ZCODE_HOST_DEBUG_OUTBOUND_HARD_LIMIT`
//!   设一个小上限；客户端不读，连续请求大应答）；
//! - `detach-service-port` → 4002 `detached`；
//! - `dispose`（Host 退出）→ 4005 `host-shutdown`，然后 Host 退出。
use std::process::Stdio;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use zcode_host_rpc::{FrameReader, Reader, codec, frame};

use super::common::{DEV_ORIGIN, Socket};

const STEP_TIMEOUT: Duration = Duration::from_secs(10);

struct Host {
    /// 测试 Host 的 home 与数据根：绝不能读写开发机真实的 ~/.zcode。
    home: tempfile::TempDir,
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
}

impl Host {
    async fn spawn(env: &[(&str, &str)]) -> Self {
        let home = tempfile::tempdir().expect("temp home");
        let mut command = zcode_proc::command(env!("CARGO_BIN_EXE_zcode-host"));
        command
            .args(["--allowed-origin", DEV_ORIGIN])
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("ZCODE_DESKTOP_HOME_DIR", home.path())
            .env("ZCODE_DATA_BASE_DIR", home.path())
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("spawn zcode-host");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut host = Self {
            home,
            child,
            stdin,
            stdout,
        };
        host.until(|message| message["type"] == "host-endpoint-ready")
            .await;
        host
    }

    async fn send(&mut self, message: Value) {
        self.stdin
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
        self.stdin.flush().await.unwrap();
    }

    /// 读控制消息（含结构化日志），直到 `matches` 成立；返回那一条。
    async fn until(&mut self, matches: impl Fn(&Value) -> bool) -> Value {
        loop {
            let line = tokio::time::timeout(STEP_TIMEOUT, self.stdout.next_line())
                .await
                .expect("control message within timeout")
                .unwrap()
                .expect("control stdout open");
            let message: Value = serde_json::from_str(&line).unwrap();
            if matches(&message) {
                return message;
            }
        }
    }

    async fn log_containing(&mut self, level: &str, text: &str) -> Value {
        self.until(|message| {
            message["type"] == "log"
                && message["level"] == level
                && message["message"]
                    .as_str()
                    .is_some_and(|m| m.contains(text))
        })
        .await
    }

    async fn init(&mut self) {
        self.send(json!({
            "type": "init-local",
            "databaseStartupId": "startup-close",
            "zcodeBuiltinProviderConfigFilePath": "/x",
        }))
        .await;
        let ready = self
            .until(|message| {
                message["type"] == "database-startup-state"
                    && matches!(message["state"]["phase"].as_str(), Some("ready" | "failed"))
            })
            .await;
        assert_eq!(ready["state"]["phase"], "ready", "{ready}");
    }

    /// 签发一个本地 attachment，连上并读掉 `[200]`。
    async fn attach(&mut self, attachment_id: &str) -> Socket {
        self.send(json!({
            "type": "issue-attachment-endpoint",
            "requestId": format!("issue-{attachment_id}"),
            "attachmentId": attachment_id,
            "clientMode": "desktop-continuous",
            "scope": {"kind": "local"},
        }))
        .await;
        let issued = self
            .until(|message| message["type"] == "attachment-endpoint-issued")
            .await;
        assert_eq!(issued["ok"], true, "{issued}");
        let mut request = issued["url"]
            .as_str()
            .unwrap()
            .into_client_request()
            .unwrap();
        let protocols = format!(
            "zcode-rpc.v1, zcode-cap.{}",
            issued["capability"].as_str().unwrap()
        );
        let headers = request.headers_mut();
        headers.insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_str(&protocols).unwrap(),
        );
        headers.insert("Origin", HeaderValue::from_static(DEV_ORIGIN));
        let (mut socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("handshake accepted");
        let mut frames = FrameReader::default();
        let initialize = loop {
            if let Some(message) = frames.next_message() {
                break message;
            }
            match tokio::time::timeout(STEP_TIMEOUT, socket.next()).await {
                Ok(Some(Ok(Message::Binary(data)))) => frames.push(&data),
                other => panic!("expected [200], got {other:?}"),
            }
        };
        let header = Reader::new(&initialize).value().unwrap().into_json();
        assert_eq!(header, json!([200]));
        socket
    }
}

async fn call(socket: &mut Socket, id: i64, channel: &str, method: &str, args: Value) {
    let mut message = Vec::new();
    codec::write_value(&mut message, &json!([100, id, channel, method]));
    codec::write_value(&mut message, &args);
    socket
        .send(Message::Binary(frame(&message).into()))
        .await
        .unwrap();
}

/// 读到对端的 Close 为止（之前的数据帧丢弃），再像浏览器一样回 Close、读到连接结束；返回 Close 帧，
/// 以及之前收到的数据帧字节数。
async fn read_until_close(socket: &mut Socket) -> (Option<CloseFrame>, usize) {
    let mut received = 0;
    tokio::time::timeout(STEP_TIMEOUT, async {
        let frame = loop {
            match socket.next().await {
                Some(Ok(Message::Close(frame))) => break frame,
                Some(Ok(Message::Binary(data))) => received += data.len(),
                Some(Ok(_)) => {}
                other => panic!("the socket ended without a Close frame: {other:?}"),
            }
        };
        // 继续读会写出 tungstenite 排好的 Close 回应，完成关闭握手。
        while let Some(Ok(_)) = socket.next().await {}
        (frame, received)
    })
    .await
    .expect("Close frame within timeout")
}

fn code_and_reason(frame: Option<CloseFrame>) -> (u16, String) {
    let frame = frame.expect("the Close frame carries a code");
    (u16::from(frame.code), frame.reason.to_string())
}

/// 客户端不读、连续请求大应答：积压越过（测试用的）上限后 Gateway 关闭连接，客户端恢复读取后先收到
/// 关闭前已写出的数据，再收到 Close 4001 `outbound-backlog`。上限只能在调试构建中覆盖。
#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_backlog_over_the_hard_limit_closes_with_outbound_backlog() {
    const HARD_LIMIT: usize = 1024 * 1024;
    const CALLS: i64 = 128;
    let limit = HARD_LIMIT.to_string();
    let mut host = Host::spawn(&[("ZCODE_HOST_DEBUG_OUTBOUND_HARD_LIMIT", &limit)]).await;
    host.log_containing("warn", "ZCODE_HOST_DEBUG_OUTBOUND_HARD_LIMIT=1048576")
        .await;
    host.init().await;
    // 每个应答约 250 KB，共约 32 MB：远超本机 TCP 缓冲能吸收的量，积压一定越过 1 MiB。
    let big = host.home.path().join("big.txt");
    std::fs::write(&big, "a".repeat(250_000)).unwrap();
    let mut socket = host.attach("stalled").await;
    for id in 1..=CALLS {
        call(
            &mut socket,
            id,
            "file",
            "readTextFile",
            json!([{"path": big, "length": 262_144}]),
        )
        .await;
    }
    host.log_containing("warn", "outbound backlog exceeded 1048576 bytes")
        .await;
    let (frame, received) = read_until_close(&mut socket).await;
    assert_eq!(
        code_and_reason(frame),
        (4001, "outbound-backlog".to_owned())
    );
    assert!(
        received < CALLS as usize * 250_000,
        "the queued replies were dropped when the connection closed ({received} bytes received)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detach_service_port_closes_with_detached() {
    let mut host = Host::spawn(&[]).await;
    host.init().await;
    let mut socket = host.attach("detach-me").await;
    let mut other = host.attach("stays").await;
    host.send(json!({"type": "detach-service-port", "attachmentId": "detach-me"}))
        .await;
    let (frame, _) = read_until_close(&mut socket).await;
    assert_eq!(code_and_reason(frame), (4002, "detached".to_owned()));
    // 其他 attachment 不受影响。
    call(&mut other, 1, "setting", "get", json!([])).await;
    let reply = tokio::time::timeout(STEP_TIMEOUT, other.next())
        .await
        .expect("reply within timeout");
    assert!(matches!(reply, Some(Ok(Message::Binary(_)))), "{reply:?}");
}

/// Host 退出时先关闭全部连接（§2.4）：客户端收到 4005 `host-shutdown`，而不是连接被中止后的 1006。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispose_closes_connections_with_host_shutdown() {
    let mut host = Host::spawn(&[]).await;
    host.init().await;
    let mut socket = host.attach("window").await;
    host.send(json!({"type": "dispose"})).await;
    let (frame, _) = read_until_close(&mut socket).await;
    assert_eq!(code_and_reason(frame), (4005, "host-shutdown".to_owned()));
    let status = tokio::time::timeout(STEP_TIMEOUT, host.child.wait())
        .await
        .expect("the Host exits after dispose")
        .unwrap();
    assert!(status.success(), "{status:?}");
}
