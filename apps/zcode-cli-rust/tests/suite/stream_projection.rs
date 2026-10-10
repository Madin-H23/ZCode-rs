//! 流式回复推给界面的 V4 投影与 Node 一致（docs/specs/rust-m8-delivery.md §4、
//! docs/specs/rust-history-actions.md「行级 actions 的时机」），经 App 协议驱动嵌入的 runtime：
//! - 一轮只有开始、结束两次 control 变化，`activeWorks[].startedAt` 是轮次开始时间；
//! - 文本分片之间没有 `state.updated`，同一刷新窗口里相邻的分片合并成一个 `row.delta`；
//! - 回复行在流式中不带 `canRetry`，这一轮结束后才带上。
//!
//! 桌面（desktop-continuous）与手机（web-remote-replayable）两个订阅都检查。
#![expect(clippy::disallowed_methods, reason = "读取测试进程的 PATH")]
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use zcode_cli_rust::{embed, runtime::RuntimeOptions, serve::AppServerOptions};

use super::common::read_request;

const LIMIT: Duration = Duration::from_secs(20);
const TITLE: &str = "Generate a concise title for this coding session.";
/// 回复的分片数；分片之间隔几毫秒，让每个分片落在不同的毫秒上。
const CHUNKS: usize = 40;

fn sse(delta: &Value) -> String {
    format!("data: {}\n\n", json!({"choices": [{"delta": delta}]}))
}

/// Chat Completions 假模型：标题请求回标题；其他请求逐个写出 CHUNKS 个文本分片后结束。
async fn mock_model() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let Some(body) = read_request(&mut socket).await else {
                    return;
                };
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
                let _ = socket.write_all(head.as_bytes()).await;
                if body.contains(TITLE) {
                    let title = sse(&json!({"content": "{\"title\":\"Stream\"}"}));
                    let _ = socket.write_all(title.as_bytes()).await;
                } else {
                    for i in 0..CHUNKS {
                        let chunk = sse(&json!({"content": format!("c{i} ")}));
                        let _ = socket.write_all(chunk.as_bytes()).await;
                        let _ = socket.flush().await;
                        tokio::time::sleep(Duration::from_millis(3)).await;
                    }
                }
                let end = format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    json!({"choices": [{"delta": {}, "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 10, "completion_tokens": 4}})
                );
                let _ = socket.write_all(end.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    url
}

struct Client {
    lines: tokio::io::Lines<BufReader<ReadHalf<tokio::io::DuplexStream>>>,
    writer: WriteHalf<tokio::io::DuplexStream>,
    next_id: u64,
    /// 读回复时顺带收下的 conversation 帧（`params`），按到达顺序。
    frames: Vec<Value>,
}

impl Client {
    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(LIMIT, self.lines.next_line())
            .await
            .expect("no message from the runtime")
            .unwrap()
            .expect("runtime closed its output");
        let message: Value = serde_json::from_str(&line).unwrap();
        if message["method"] == "v4/conversation/frame" {
            assert_eq!(message["params"]["kind"], "complete", "{message}");
            self.frames.push(message["params"].clone());
        }
        message
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let line = format!(
            "{}\n",
            json!({"id": id, "method": method, "params": params})
        );
        self.writer.write_all(line.as_bytes()).await.unwrap();
        loop {
            let message = self.next().await;
            if message["id"] == id && message.get("method").is_none() {
                assert!(message.get("error").is_none(), "{method} failed: {message}");
                return message["result"].clone();
            }
        }
    }

    async fn command(&mut self, session: Option<&str>, id: &str, kind: &str, payload: Value) {
        let ack = self
            .request(
                "v4/command",
                json!({"commandId": id, "clientId": "stream-test", "sessionId": session,
                    "type": kind, "issuedAt": 1000, "payload": payload}),
            )
            .await;
        assert_eq!(ack["status"], "accepted", "{ack}");
    }

    async fn subscribe(&mut self, session: &str, connection: &str, mode: &str) -> String {
        let reply = self
            .request(
                "v4/conversation/subscribe",
                json!({"topic": format!("conversation/{session}"), "connectionId": connection,
                    "clientMode": mode}),
            )
            .await;
        reply["ack"]["subscriptionId"].as_str().unwrap().to_owned()
    }
}

fn frames_of<'a>(frames: &'a [Value], subscription: &str) -> Vec<&'a Value> {
    frames
        .iter()
        .filter(|f| f["subscriptionId"] == subscription)
        .map(|f| &f["frame"])
        .collect()
}

fn ended(frame: &Value) -> bool {
    frame["payload"]["deltas"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|d| d["patch"]["control"]["phase"] == "completedSuccess")
}

/// 一个订阅从发送到这一轮结束所收到的帧，逐帧应用后检查界面看到的状态。
#[derive(Debug, Default)]
struct Observed {
    /// 带 `control` 的 `state.updated`（帧内已合并）。
    controls: Vec<Value>,
    /// 流式文本帧（含 `row.delta` 的帧）的个数、op 数与字节数。
    text_frames: usize,
    text_ops: usize,
    text_bytes: usize,
    /// 一帧里同一 (rowId, path) 出现多个 `row.delta`：相邻分片没有合并。
    split_deltas: usize,
    /// 应用某帧后，assistantText 行在 running 或 streaming 时带 `canRetry`。
    early_retry: Vec<Value>,
    rows: BTreeMap<u64, Value>,
}

fn observe(frames: &[&Value]) -> Observed {
    let mut seen = Observed::default();
    let mut running = false;
    for frame in frames {
        let payload = &frame["payload"];
        if payload["kind"] == "snapshot" {
            seen.rows = payload["snapshot"]["rows"]["window"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| (r["rowId"].as_u64().unwrap(), r.clone()))
                .collect();
            continue;
        }
        let deltas = payload["deltas"].as_array().unwrap();
        let mut paths = Vec::new();
        for delta in deltas {
            match delta["op"].as_str().unwrap() {
                "row.appended" | "row.upserted" => {
                    let row = &delta["row"];
                    seen.rows
                        .insert(row["rowId"].as_u64().unwrap(), row.clone());
                }
                "row.delta" => {
                    let key = (delta["rowId"].clone(), delta["path"].clone());
                    if paths.contains(&key) {
                        seen.split_deltas += 1;
                    }
                    paths.push(key);
                    let row = seen
                        .rows
                        .get_mut(&delta["rowId"].as_u64().unwrap())
                        .unwrap();
                    let Value::String(text) = &mut row[delta["path"].as_str().unwrap()] else {
                        panic!("text path: {delta}");
                    };
                    text.push_str(delta["append"].as_str().unwrap());
                }
                "state.updated" => {
                    if let Some(control) = delta["patch"].get("control") {
                        running = control["phase"] == "running";
                        seen.controls.push(control.clone());
                    }
                }
                _ => {}
            }
        }
        if !paths.is_empty() {
            seen.text_frames += 1;
            seen.text_ops += deltas.len();
            seen.text_bytes += payload.to_string().len();
        }
        for row in seen.rows.values() {
            if row["kind"] == "assistantText"
                && row["actions"]["canRetry"] == true
                && (running || row["state"] != "complete")
            {
                seen.early_retry.push(row.clone());
            }
        }
    }
    seen
}

#[tokio::test]
async fn a_streaming_reply_changes_control_twice_and_offers_retry_only_after_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let root = tokio::fs::canonicalize(dir.path()).await.unwrap();
    let workspace = root.join("workspace");
    tokio::fs::create_dir_all(&workspace).await.unwrap();
    let url = mock_model().await;
    let model = root.join("model.json");
    tokio::fs::write(
        &model,
        json!({"providerId": "fixture", "modelId": "core-model", "reasoningLevel": "none",
            "reasoningParameters": {"reasoning_effort": "none"}, "baseUrl": url,
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
            data_dir: "data".into(),
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
        frames: vec![],
    };
    loop {
        let message = client.next().await;
        if message["method"] == "startup/storageState" && message["params"]["phase"] == "ready" {
            break;
        }
    }
    let created = client
        .request(
            "v4/command",
            json!({"commandId": "create", "clientId": "stream-test", "sessionId": null,
                "type": "createSession", "issuedAt": 1000, "payload": {"workspaceId": workspace}}),
        )
        .await;
    let session = created["result"]["sessionId"].as_str().unwrap().to_owned();
    let desktop = client
        .subscribe(&session, "desktop", "desktop-continuous")
        .await;
    let mobile = client
        .subscribe(&session, "mobile", "web-remote-replayable")
        .await;
    client
        .command(
            Some(&session),
            "send-1",
            "sendText",
            json!({"text": "stream"}),
        )
        .await;
    while ![&desktop, &mobile]
        .iter()
        .all(|s| frames_of(&client.frames, s).into_iter().any(ended))
    {
        client.next().await;
    }

    let reply: String = (0..CHUNKS).map(|i| format!("c{i} ")).collect();
    for subscription in [&desktop, &mobile] {
        let frames = frames_of(&client.frames, subscription);
        let end = frames.iter().position(|f| ended(f)).unwrap();
        let seen = observe(&frames[..=end]);
        eprintln!(
            "{subscription}: {} control updates, {} text frames, {} ops, {} bytes",
            seen.controls.len(),
            seen.text_frames,
            seen.text_ops,
            seen.text_bytes
        );
        // Node：轮次开始与结束各一次 control（product-projection.ts:2086 onTurnStarted、:2216 onTurnComplete）。
        assert_eq!(seen.controls.len(), 2, "{:#?}", seen.controls);
        let header = seen
            .rows
            .values()
            .find(|r| r["kind"] == "turnHeader")
            .unwrap();
        assert_eq!(
            seen.controls[0]["activeWorks"],
            json!([{"kind": "primaryTurn",
                "foregroundExecutionId": seen.controls[0]["activeWorks"][0]["foregroundExecutionId"],
                "startedAt": header["startedAt"]}])
        );
        assert_eq!(seen.controls[1]["activeWorks"], json!([]));
        assert_eq!(
            seen.split_deltas, 0,
            "adjacent text deltas were not coalesced"
        );
        assert_eq!(seen.early_retry, Vec::<Value>::new());
        let answer = seen
            .rows
            .values()
            .find(|r| r["kind"] == "assistantText")
            .unwrap();
        assert_eq!(answer["text"], reply.as_str());
        assert_eq!(answer["state"], "complete");
        assert_eq!(
            answer["actions"],
            json!({"canRetry": true, "canFork": true})
        );
    }
}
