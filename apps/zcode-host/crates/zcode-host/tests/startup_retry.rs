//! 启动失败页的重试（docs/specs/rust-host-resilience.md §2.2）：真实 zcode-host 在 tasks-index 打不开时
//! 报 failed；错误的 attemptId 被忽略；snapshot 重发最近状态；故障排除后 retry 以新 attemptId、
//! 递增的 sequence 重新准备并进入 ready，之前挂起的 attachment 随之完成握手。
use std::process::Stdio;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use zcode_host_rpc::{FrameReader, Reader};

const STEP_TIMEOUT: Duration = Duration::from_secs(10);

struct Host {
    _child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
}

impl Host {
    async fn send(&mut self, message: Value) {
        let line = format!("{message}\n");
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    /// 下一条非日志控制消息；`limit` 内没有时返回 None。
    async fn recv_within(&mut self, limit: Duration) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let line = tokio::time::timeout_at(deadline, self.stdout.next_line())
                .await
                .ok()?
                .unwrap()
                .expect("control stdout open");
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["type"] != "log" {
                return Some(message);
            }
        }
    }

    async fn recv(&mut self) -> Value {
        self.recv_within(STEP_TIMEOUT)
            .await
            .expect("control message within timeout")
    }

    /// 读启动状态直到终态（ready / failed），返回全部状态。
    async fn until_terminal(&mut self) -> Vec<Value> {
        let mut states = Vec::new();
        loop {
            let message = self.recv().await;
            assert_eq!(message["type"], "database-startup-state", "{message}");
            let state = message["state"].clone();
            let phase = state["phase"].as_str().unwrap().to_owned();
            states.push(state);
            if phase == "ready" || phase == "failed" {
                return states;
            }
        }
    }
}

fn sequences_increase(states: &[Value]) {
    for pair in states.windows(2) {
        assert!(
            pair[1]["sequence"].as_u64() > pair[0]["sequence"].as_u64(),
            "{states:?}"
        );
    }
}

#[tokio::test]
async fn failed_startup_retries_with_a_new_attempt() {
    let home = tempfile::tempdir().unwrap();
    // tasks-index.sqlite 是目录：SQLite 打不开，属于可重试的失败。
    let blocker = home.path().join(".zcode/v2/tasks-index.sqlite");
    std::fs::create_dir_all(&blocker).unwrap();
    let mut child = zcode_proc::command(env!("CARGO_BIN_EXE_zcode-host"))
        .args(["--allowed-origin", "file://"])
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("ZCODE_DESKTOP_HOME_DIR", home.path())
        .env("ZCODE_DATA_BASE_DIR", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut host = Host {
        stdin: child.stdin.take().unwrap(),
        stdout: BufReader::new(child.stdout.take().unwrap()).lines(),
        _child: child,
    };
    let ready = host.recv().await;
    let port = ready["wsPort"].as_u64().unwrap();

    // 失败前建立的 attachment：一直挂起，直到某次尝试 ready。
    host.send(json!({"type": "init-local", "databaseStartupId": "startup-1"}))
        .await;
    let first = host.until_terminal().await;
    sequences_increase(&first);
    let failed = first.last().unwrap().clone();
    assert_eq!(failed["phase"], "failed", "{failed}");
    assert_eq!(failed["failedPhase"], "preparing_host_storage");
    let code = failed["errorCode"].as_str().unwrap();
    assert!(
        !["corrupt", "checksum_mismatch"].contains(&code),
        "unexpected non-retryable code {code}"
    );
    let failed_attempt = failed["attemptId"].as_str().unwrap().to_owned();

    host.send(json!({
        "type": "issue-attachment-endpoint", "requestId": "r1", "attachmentId": "a1",
        "clientMode": "desktop-continuous", "scope": {"kind": "local"},
    }))
    .await;
    let issued = host.recv().await;
    let mut request = issued["url"]
        .as_str()
        .unwrap()
        .into_client_request()
        .unwrap();
    let protocols = format!(
        "zcode-rpc.v1, zcode-cap.{}",
        issued["capability"].as_str().unwrap()
    );
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_str(&protocols).unwrap(),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), socket.next())
            .await
            .is_err(),
        "no Initialize before ready"
    );

    // 过期或伪造的 attemptId 被忽略；snapshot 原样重发最近状态。
    host.send(json!({"type": "database-startup-control", "control": {"action": "retry", "attemptId": "stale"}}))
        .await;
    host.send(json!({"type": "database-startup-control", "control": {"action": "snapshot"}}))
        .await;
    let snapshot = host.recv().await;
    assert_eq!(snapshot["state"], failed);

    std::fs::remove_dir(&blocker).unwrap();
    host.send(json!({"type": "database-startup-control", "control": {"action": "retry", "attemptId": failed_attempt}}))
        .await;
    let second = host.until_terminal().await;
    let all: Vec<Value> = first.iter().chain(&second).cloned().collect();
    sequences_increase(&all);
    let retried = second.last().unwrap();
    assert_eq!(retried["phase"], "ready", "{second:?}");
    assert_eq!(second[0]["phase"], "starting");
    assert!(second.iter().all(|state| state["startupId"] == "startup-1"));
    let new_attempt = second[0]["attemptId"].as_str().unwrap();
    assert_ne!(new_attempt, failed_attempt);
    assert!(second.iter().all(|state| state["attemptId"] == new_attempt));
    assert!(second.iter().all(|state| state.get("errorCode").is_none()));

    // 挂起的 attachment 现在完成握手。
    let mut frames = FrameReader::default();
    let initialize = tokio::time::timeout(STEP_TIMEOUT, async {
        loop {
            if let Some(message) = frames.next_message() {
                return Reader::new(&message).value().unwrap().into_json();
            }
            match socket.next().await {
                Some(Ok(Message::Binary(data))) => frames.push(&data),
                Some(Ok(_)) => {}
                other => panic!("socket ended: {other:?}"),
            }
        }
    })
    .await
    .expect("Initialize after retry");
    assert_eq!(initialize, json!([200]));

    // ready 之后的 retry 是空操作：不再产生状态。
    host.send(json!({"type": "database-startup-control", "control": {"action": "retry", "attemptId": new_attempt}}))
        .await;
    assert!(host.recv_within(Duration::from_millis(300)).await.is_none());
    assert!(port > 0);
}
