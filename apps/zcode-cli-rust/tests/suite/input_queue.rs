//! 输入队列对外的协议能力（docs/specs/rust-u3-stage-b.md 3.4.5），经 App 协议驱动嵌入的 runtime：
//! `v4/conversation/inputQueue`、`sendText.externalId`、sessions-index 摘要的 `activeInputId`、
//! legacy `session/send` 的 `requestedDelivery: "queue"`。
#![expect(clippy::disallowed_methods, reason = "读取测试进程的 PATH")]
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::sync::Semaphore;
use zcode_cli_rust::{embed, runtime::RuntimeOptions, serve::AppServerOptions};

const LIMIT: Duration = Duration::from_secs(20);
/// 带这个标记的输入，模型请求要等测试放行才回复。
const HOLD: &str = "[hold]";
const TITLE: &str = "Generate a concise title for this coding session.";

fn sse(content: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": {"content": content}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 4}})
    )
}

/// 本轮新增的消息（最后一条 assistant 之后），序列化成文本。
fn pending(body: &str) -> String {
    let request: Value = serde_json::from_str(body).unwrap_or_default();
    let messages = request["messages"].as_array().cloned().unwrap_or_default();
    let start = messages
        .iter()
        .rposition(|m| m["role"] == "assistant")
        .map_or(0, |at| at + 1);
    Value::from(messages[start..].to_vec()).to_string()
}

async fn read_body(socket: &mut tokio::net::TcpStream) -> Option<String> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let end = loop {
        if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
    let length: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))?
        .trim()
        .parse()
        .ok()?;
    while bytes.len() < end + length {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    Some(String::from_utf8_lossy(&bytes[end..end + length]).into_owned())
}

/// 模型替身：标题请求与普通输入立即回复；带 HOLD 的输入在 `release` 放行一次后回复。
struct Mock {
    url: String,
    holding: Arc<AtomicUsize>,
    release: Arc<Semaphore>,
}

async fn mock_model() -> Mock {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let holding = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Semaphore::new(0));
    let (count, gate) = (holding.clone(), release.clone());
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let (count, gate) = (count.clone(), gate.clone());
            tokio::spawn(async move {
                let Some(body) = read_body(&mut socket).await else {
                    return;
                };
                let reply = if body.contains(TITLE) {
                    sse("{\"title\":\"Queue\"}")
                } else {
                    if pending(&body).contains(HOLD) {
                        count.fetch_add(1, Ordering::SeqCst);
                        gate.acquire().await.unwrap().forget();
                        count.fetch_sub(1, Ordering::SeqCst);
                    }
                    sse("done")
                };
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    Mock {
        url,
        holding,
        release,
    }
}

struct Client {
    lines: tokio::io::Lines<BufReader<ReadHalf<tokio::io::DuplexStream>>>,
    writer: WriteHalf<tokio::io::DuplexStream>,
    next_id: u64,
    /// 读回复时顺带收下的 sessions-index 摘要（session.upserted），按到达顺序。
    summaries: Vec<Value>,
}

impl Client {
    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(LIMIT, self.lines.next_line())
            .await
            .expect("no message from the runtime")
            .unwrap()
            .expect("runtime closed its output");
        let message: Value = serde_json::from_str(&line).unwrap();
        let payload = &message["params"]["frame"]["payload"];
        for delta in payload["deltas"].as_array().into_iter().flatten() {
            if delta["op"] == "session.upserted" {
                self.summaries.push(delta["session"].clone());
            }
        }
        message
    }

    async fn until(&mut self, accept: impl Fn(&Value) -> bool) -> Value {
        loop {
            let message = self.next().await;
            if accept(&message) {
                return message;
            }
        }
    }

    /// 一个请求的完整回复（`result` 或 `error`）。
    async fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let line = format!(
            "{}\n",
            json!({"id": id, "method": method, "params": params})
        );
        self.writer.write_all(line.as_bytes()).await.unwrap();
        self.until(|m| m["id"] == id && m.get("method").is_none())
            .await
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let reply = self.call(method, params).await;
        assert!(reply.get("error").is_none(), "{method} failed: {reply}");
        reply["result"].clone()
    }

    async fn command(
        &mut self,
        session: Option<&str>,
        id: &str,
        kind: &str,
        payload: Value,
    ) -> Value {
        self.request(
            "v4/command",
            json!({"commandId": id, "clientId": "queue-test", "sessionId": session,
                "type": kind, "issuedAt": 1000, "payload": payload}),
        )
        .await
    }

    async fn queue(&mut self, session: &str) -> Value {
        self.request("v4/conversation/inputQueue", json!({"sessionId": session}))
            .await
    }

    /// 等到这个会话第 `from` 条之后的摘要满足条件（已收到的也算）；返回它的位置与内容。
    async fn summary_from(
        &mut self,
        from: usize,
        session: &str,
        accept: impl Fn(&Value) -> bool,
    ) -> (usize, Value) {
        tokio::time::timeout(LIMIT, async {
            let mut seen = from;
            loop {
                let found = self.summaries[seen..]
                    .iter()
                    .position(|s| s["sessionId"] == session && accept(s));
                if let Some(at) = found {
                    return (seen + at + 1, self.summaries[seen + at].clone());
                }
                seen = self.summaries.len();
                self.next().await;
            }
        })
        .await
        .expect("no matching sessions-index summary")
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    workspace: PathBuf,
    mock: Mock,
    client: Client,
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = tokio::fs::canonicalize(dir.path()).await.unwrap();
    let workspace = root.join("workspace");
    tokio::fs::create_dir_all(&workspace).await.unwrap();
    let mock = mock_model().await;
    let model = root.join("model.json");
    tokio::fs::write(
        &model,
        json!({"providerId": "fixture", "modelId": "core-model", "reasoningLevel": "none",
            "reasoningParameters": {"reasoning_effort": "none"}, "baseUrl": mock.url,
            "requestTimeoutSeconds": 60})
        .to_string(),
    )
    .await
    .unwrap();
    let text = |path: &Path| path.to_string_lossy().into_owned();
    let runtime = embed::spawn(AppServerOptions {
        runtime: RuntimeOptions {
            home: root.clone(),
            env: vec![
                ("HOME".into(), text(&root)),
                ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
                (
                    "ZCODE_SESSION_DB_PATH".into(),
                    text(&root.join("sessions.sqlite")),
                ),
            ],
            argv: vec!["zcode-cli-rust".into()],
            cwd: workspace.clone(),
            workspace_identity: None,
            base_dir: root.clone(),
            data_dir: PathBuf::from("data"),
        },
        config: Some(model),
        desktop: false,
        prepare_storage: false,
    });
    let (reader, writer) = tokio::io::split(runtime.io);
    let mut client = Client {
        lines: BufReader::new(reader).lines(),
        writer,
        next_id: 0,
        summaries: vec![],
    };
    client
        .until(|m| m["method"] == "startup/storageState" && m["params"]["phase"] == "ready")
        .await;
    let topic = format!("sessions-index/{}", workspace.display());
    client
        .request(
            "v4/conversation/subscribe",
            json!({"topic": topic, "connectionId": "queue-test", "clientMode": "desktop-continuous"}),
        )
        .await;
    Fixture {
        _dir: dir,
        workspace,
        mock,
        client,
    }
}

async fn until(what: &str, check: impl Fn() -> bool) {
    tokio::time::timeout(LIMIT, async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

async fn create(client: &mut Client, workspace: &Path) -> String {
    let ack = client
        .command(
            None,
            "create",
            "createSession",
            json!({"workspaceId": workspace}),
        )
        .await;
    ack["result"]["sessionId"].as_str().unwrap().to_owned()
}

/// 运行中的输入、带 externalId 的队列项与摘要的 activeInputId；run 结束后 runtime 开跑队首，
/// 摘要依次是 running(run-1) → 终态（无 activeInputId）→ running(q-2) → 终态。
#[tokio::test]
async fn the_queue_and_the_running_input_are_visible_over_the_protocol() {
    let Fixture {
        _dir,
        workspace,
        mock,
        mut client,
    } = fixture().await;
    let session = create(&mut client, &workspace).await;
    let idle = client.queue(&session).await;
    assert_eq!(idle["queue"], json!({"items": [], "autoDrain": true}));
    assert!(idle.get("activeInput").is_none(), "{idle}");
    assert_eq!(idle["running"], false);

    let first = format!("{HOLD} first");
    let ack = client
        .command(
            Some(&session),
            "run-1",
            "sendText",
            json!({"text": first, "externalId": "ext-1"}),
        )
        .await;
    assert_eq!(ack["result"]["delivery"], "startNow", "{ack}");
    until("the held model request", || {
        mock.holding.load(Ordering::SeqCst) == 1
    })
    .await;
    let running = client.queue(&session).await;
    assert_eq!(running["running"], true);
    let active = &running["activeInput"];
    assert_eq!(active["inputId"], "run-1");
    assert_eq!(active["externalId"], "ext-1");
    assert_eq!(active["text"], first.as_str());
    assert!(active["startedAt"].is_number(), "{running}");

    let ack = client
        .command(
            Some(&session),
            "q-2",
            "sendText",
            json!({"text": "second", "requestedDelivery": "queue", "externalId": "ext-2"}),
        )
        .await;
    assert_eq!(ack["result"]["delivery"], "queue", "{ack}");
    let queued = client.queue(&session).await;
    let item = &queued["queue"]["items"][0];
    assert_eq!(item["externalId"], "ext-2");
    assert_eq!(item["sourceCommandId"], "q-2");
    assert_eq!(item["queueItemId"], "queue_q-2");
    assert_eq!(item["dispatch"]["state"], "queued");
    assert!(queued["revision"].as_u64().unwrap() > running["revision"].as_u64().unwrap());

    // 摘要的顺序：running(run-1) → 终态（无 activeInputId）→ running(q-2) → 终态。
    let running = |input: &'static str| {
        move |s: &Value| s["phase"] == "running" && s["activeInputId"] == input
    };
    let ended = |s: &Value| s["phase"] == "completedSuccess" && s.get("activeInputId").is_none();
    let (at, _) = client.summary_from(0, &session, running("run-1")).await;
    mock.release.add_permits(1);
    let (at, _) = client.summary_from(at, &session, ended).await;
    let (at, _) = client.summary_from(at, &session, running("q-2")).await;
    client.summary_from(at, &session, ended).await;
    let after = client.queue(&session).await;
    assert!(after.get("activeInput").is_none(), "{after}");
    assert_eq!(after["running"], false);
    assert_eq!(after["queue"]["items"], json!([]));
}

/// legacy `session/send` 开跑的 run 持有 legacy 锁：缺省时再发报忙（不变），带
/// `requestedDelivery: "queue"` 时带附件入队，`externalId` 记在队列项上；取值不合法时报参数错误。
#[tokio::test]
async fn legacy_send_queues_under_the_legacy_lock_when_asked() {
    let Fixture {
        _dir,
        workspace,
        mock,
        mut client,
    } = fixture().await;
    let session = create(&mut client, &workspace).await;
    let send = |input: &str, content: &str| json!({"sessionId": session, "inputId": input, "content": content});
    client
        .request("session/send", send("legacy-1", &format!("{HOLD} first")))
        .await;
    until("the held model request", || {
        mock.holding.load(Ordering::SeqCst) == 1
    })
    .await;

    let busy = client.call("session/send", send("legacy-x", "plain")).await;
    assert_eq!(busy["error"]["code"], -32010, "{busy}");
    let mut invalid = send("legacy-y", "plain");
    invalid["requestedDelivery"] = "guide".into();
    let invalid = client.call("session/send", invalid).await;
    assert_eq!(invalid["error"]["code"], -32602, "{invalid}");
    assert!(
        invalid["error"]["message"]
            .as_str()
            .unwrap()
            .contains("requestedDelivery"),
        "{invalid}"
    );

    let mut queued = send("legacy-2", "with a file");
    queued["requestedDelivery"] = "queue".into();
    queued["externalId"] = "ext-2".into();
    queued["attachments"] = json!([{"kind": "file", "filename": "notes.txt",
        "mimeType": "text/plain", "textContent": "hello"}]);
    let result = client.request("session/send", queued).await;
    assert_eq!(result["accepted"], true, "{result}");
    let state = client.queue(&session).await;
    let item = &state["queue"]["items"][0];
    assert_eq!(item["sourceCommandId"], "legacy-2", "{state}");
    assert_eq!(item["externalId"], "ext-2");
    assert_eq!(item["attachments"][0]["fileName"], "notes.txt");
    assert_eq!(state["activeInput"]["inputId"], "legacy-1");

    mock.release.add_permits(1);
    let (at, _) = client
        .summary_from(0, &session, |s| {
            s["phase"] == "running" && s["activeInputId"] == "legacy-2"
        })
        .await;
    client
        .summary_from(at, &session, |s| {
            s["phase"] == "completedSuccess" && s.get("activeInputId").is_none()
        })
        .await;
}
