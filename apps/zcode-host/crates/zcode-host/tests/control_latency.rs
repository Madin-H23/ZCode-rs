//! R2 性能验收（docs/specs/rust-host.md 第 5 节 R2；rust-host-resilience.md §6）：
//! N = 4 个 workspace 同时流式时，控制 RPC 的 p95 ≤ 20 ms。
//!
//! 真实 zcode-host 二进制 + 真实 Rust CLI + 本地 mock 的 OpenAI 兼容模型，不经过 UI。控制 RPC 与 4 路
//! 流式帧走同一条 WebSocket（Renderer 只有一条连接，队头阻塞也计入）。测两类调用：
//! - `setting.get`：只在 Host 内处理；
//! - `zcode-agent.queryConversationCommandsV4{clock:true}`：经 Host 到正在流式的 CLI 再返回。
//!
//! 缺省用进程内的嵌入式 runtime；设置 `ZCODE_TEST_CLI_RUST_BINARY` 时走子进程链路作差分对照。
//! 数字用 `cargo test --release -p zcode-host --test control_latency -- --nocapture` 查看。
#![expect(
    clippy::disallowed_methods,
    reason = "集成测试从进程环境取被测二进制与 PATH"
)]
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use zcode_host_rpc::{FrameReader, Reader, codec, frame};

/// 调度器对执行中闲时任务数的上报：与请求无关、随时可能到达，读“下一条控制消息”时跳过。
const UNSOLICITED_REPORT: &str = "off-peak-active-count";
const WORKSPACES: usize = 4;
const STREAM_CHUNKS: usize = 600;
const CHUNK_GAP: Duration = Duration::from_millis(20);
const SAMPLES: usize = 150;
const P95_BUDGET: Duration = Duration::from_millis(20);

/// runtime 的选择：缺省嵌入式；`ZCODE_TEST_CLI_RUST_BINARY` 指向可执行文件时走子进程链路。
fn runtime_env() -> Vec<(&'static str, String)> {
    match std::env::var_os("ZCODE_TEST_CLI_RUST_BINARY").map(PathBuf::from) {
        Some(cli) => {
            assert!(
                cli.is_file(),
                "ZCODE_TEST_CLI_RUST_BINARY is not a file: {}",
                cli.display()
            );
            vec![
                (
                    "ZCODE_AGENT_SERVER_COMMAND",
                    cli.to_string_lossy().into_owned(),
                ),
                ("ZCODE_AGENT_SERVER_RUNTIME", "zcode-cli-rust".to_owned()),
            ]
        }
        None => vec![("ZCODE_AGENT_SERVER_RUNTIME", "embedded".to_owned())],
    }
}

fn sse(delta: Value, finish: Value) -> String {
    let chunk = json!({
        "id": "mock", "object": "chat.completion.chunk", "created": 0, "model": "mock-model",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    });
    format!("data: {chunk}\n\n")
}

/// 主对话请求（带 tools）持续流式约 12 s；标题生成等附带请求立即结束。
async fn serve_mock_model(listener: TcpListener) {
    while let Ok((mut stream, _)) = listener.accept().await {
        tokio::spawn(async move {
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 16384];
            let body_start = loop {
                match stream.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                }
                if let Some(index) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8_lossy(&buffer[..body_start]).to_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while buffer.len() < body_start + length {
                match stream.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                }
            }
            let main = String::from_utf8_lossy(&buffer[body_start..]).contains("\"tools\"");
            let head =
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
            if stream.write_all(head.as_bytes()).await.is_err() {
                return;
            }
            let _ = stream
                .write_all(sse(json!({"role": "assistant", "content": ""}), Value::Null).as_bytes())
                .await;
            if main {
                for index in 0..STREAM_CHUNKS {
                    let text = format!("token-{index} lorem ipsum dolor sit amet. ");
                    if stream
                        .write_all(sse(json!({"content": text}), Value::Null).as_bytes())
                        .await
                        .is_err()
                    {
                        return;
                    }
                    tokio::time::sleep(CHUNK_GAP).await;
                }
            } else {
                let _ = stream
                    .write_all(
                        sse(json!({"content": "{\"title\":\"Load\"}"}), Value::Null).as_bytes(),
                    )
                    .await;
            }
            let mut stop = sse(json!({}), json!("stop"));
            stop.push_str("data: [DONE]\n\n");
            let _ = stream.write_all(stop.as_bytes()).await;
            let _ = stream.shutdown().await;
        });
    }
}

struct Control {
    _child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
}

impl Control {
    async fn send(&mut self, message: Value) {
        self.stdin
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
    }

    async fn recv(&mut self) -> Value {
        loop {
            let line = tokio::time::timeout(Duration::from_secs(30), self.stdout.next_line())
                .await
                .expect("control message")
                .unwrap()
                .expect("control stdout open");
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["type"] == UNSOLICITED_REPORT {
                continue;
            }
            if message["type"] != "log" {
                return message;
            }
        }
    }
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 顺序发请求、读到对应应答为止；途中的事件帧按 listen id 计数。
struct Rpc {
    socket: Socket,
    frames: FrameReader,
    next_id: i32,
    events: HashMap<i64, usize>,
}

impl Rpc {
    async fn write(&mut self, header: Value, body: Option<Value>) {
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

    async fn next(&mut self) -> (Value, Value) {
        loop {
            if let Some(message) = self.frames.next_message() {
                let mut reader = Reader::new(&message);
                let header = reader.value().unwrap().into_json();
                return (header, reader.value().unwrap().into_json());
            }
            match tokio::time::timeout(Duration::from_secs(60), self.socket.next()).await {
                Ok(Some(Ok(Message::Binary(data)))) => self.frames.push(&data),
                Ok(Some(Ok(_))) => {}
                other => panic!("socket ended: {other:?}"),
            }
        }
    }

    async fn call(&mut self, channel: &str, method: &str, args: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.write(json!([100, id, channel, method]), Some(args))
            .await;
        loop {
            let (header, body) = self.next().await;
            match header[0].as_i64() {
                Some(204) => *self.events.entry(header[1].as_i64().unwrap()).or_default() += 1,
                Some(201) if header[1] == id => return body,
                Some(202 | 203) if header[1] == id => panic!("{channel}.{method} failed: {body}"),
                _ => {}
            }
        }
    }

    async fn listen(&mut self, channel: &str, event: &str, arg: Value) -> i64 {
        self.next_id += 1;
        let id = self.next_id;
        self.write(json!([102, id, channel, event]), Some(arg))
            .await;
        id as i64
    }
}

fn percentile(sorted: &[Duration], fraction: f64) -> Duration {
    let index = ((sorted.len() as f64 - 1.0) * fraction).round() as usize;
    sorted[index]
}

#[expect(clippy::print_stderr, reason = "输出延迟统计供性能对比")]
fn summary(name: &str, mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    let p95 = percentile(&samples, 0.95);
    eprintln!(
        "control_latency {name}: n={} p50={:.2}ms p95={:.2}ms p99={:.2}ms max={:.2}ms",
        samples.len(),
        percentile(&samples, 0.5).as_secs_f64() * 1e3,
        p95.as_secs_f64() * 1e3,
        percentile(&samples, 0.99).as_secs_f64() * 1e3,
        samples.last().unwrap().as_secs_f64() * 1e3,
    );
    p95
}

fn write_provider_config(home: &Path, port: u16) {
    let config_dir = home.join(".zcode/v2");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("provider_config.json"),
        json!({"schemaVersion": 1, "config": {
            "providerOrder": ["mock-provider"],
            "providerConfigRules": {"providerRules": [{
                "providerId": "mock-provider", "providerName": "Mock",
                "config": {
                    "group": "standard-personal",
                    "access": {"type": "api-key", "apiKey": "sk-test"},
                    "api": {"type": "openai-chat-completions", "baseUrl": format!("http://127.0.0.1:{port}/v1")},
                    "personalModelIds": ["mock-model"], "modelOrder": ["mock-model"],
                },
            }]},
            "modelConfigRules": {
                "providerModelRules": [{"providerId": "mock-provider", "modelId": "mock-model", "config": {"enabled": true}}],
                "manualProviderModelRules": [],
            },
        }})
        .to_string(),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn control_rpc_p95_while_four_workspaces_stream() {
    let home = tempfile::tempdir().unwrap();
    let workspaces: Vec<tempfile::TempDir> = (0..WORKSPACES)
        .map(|_| tempfile::tempdir().unwrap())
        .collect();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve_mock_model(listener));
    write_provider_config(home.path(), port);
    let builtin = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../config/provider/zcode-builtin.json");

    let mut child = zcode_proc::command(env!("CARGO_BIN_EXE_zcode-host"))
        .args(["--allowed-origin", "file://"])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("TMPDIR", std::env::temp_dir())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("ZCODE_DESKTOP_HOME_DIR", home.path())
        .env("ZCODE_DATA_BASE_DIR", home.path())
        .env("NO_PROXY", "127.0.0.1,localhost")
        .envs(runtime_env())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut control = Control {
        stdin: child.stdin.take().unwrap(),
        stdout: BufReader::new(child.stdout.take().unwrap()).lines(),
        _child: child,
    };
    assert_eq!(control.recv().await["type"], "host-endpoint-ready");
    control
        .send(json!({"type": "init-local", "databaseStartupId": "perf",
            "zcodeBuiltinProviderConfigFilePath": builtin}))
        .await;
    loop {
        let message = control.recv().await;
        if message["type"] == "database-startup-state" && message["state"]["phase"] == "ready" {
            break;
        }
    }
    control
        .send(
            json!({"type": "issue-attachment-endpoint", "requestId": "r1", "attachmentId": "a1",
            "clientMode": "desktop-continuous", "scope": {"kind": "local"}}),
        )
        .await;
    let issued = control.recv().await;
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
    let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let mut rpc = Rpc {
        socket,
        frames: FrameReader::default(),
        next_id: 0,
        events: HashMap::new(),
    };
    assert_eq!(rpc.next().await.0, json!([200]));

    rpc.call("zcode-agent", "helloConversationV4", json!([]))
        .await;
    rpc.call(
        "zcode-agent",
        "initializeConversationV4",
        json!([{"kind": "clientHello", "protocolVersion": 3, "clientId": "perf-client",
            "clientKind": "desktop", "appVersion": "unknown"}]),
    )
    .await;

    let mut streams = Vec::new();
    for (index, workspace) in workspaces.iter().enumerate() {
        let path = workspace.path().to_string_lossy().into_owned();
        let listen = rpc
            .listen(
                "zcode-agent",
                "onDynamicConversationFrame",
                json!({"workspacePath": path}),
            )
            .await;
        let envelope = json!({
            "commandId": format!("0190a000-0000-7000-8000-00000000000{index}"),
            "clientId": "perf-client", "sessionId": null, "type": "createSession",
            "payload": {"workspaceId": path, "firstInput": {
                "text": "stream a long answer",
                "modelSelection": {"providerId": "mock-provider", "modelId": "mock-model"},
                "mode": "build", "planEnabled": false,
            }},
            "issuedAt": 1,
        });
        let ack = rpc
            .call(
                "zcode-agent",
                "sendConversationCommandV4",
                json!([{"workspacePath": path, "envelope": envelope}]),
            )
            .await;
        assert_eq!(ack["status"], "accepted", "{ack}");
        let session = ack["result"]["sessionId"].as_str().unwrap().to_owned();
        rpc.call(
            "zcode-agent",
            "subscribeConversationV4",
            json!([{"workspacePath": path, "sessionId": session}]),
        )
        .await;
        streams.push((listen, path));
    }

    // 等 4 路都开始出帧，再在流式进行中采样。
    let warm = Instant::now();
    while streams
        .iter()
        .any(|(listen, _)| rpc.events.get(listen).copied().unwrap_or(0) < 5)
    {
        assert!(
            warm.elapsed() < Duration::from_secs(60),
            "streams did not start"
        );
        rpc.call("setting", "get", json!([])).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let before: Vec<usize> = streams
        .iter()
        .map(|(listen, _)| rpc.events[listen])
        .collect();

    let (mut local, mut agent) = (Vec::new(), Vec::new());
    let started = Instant::now();
    for index in 0..SAMPLES {
        let sent = Instant::now();
        rpc.call("setting", "get", json!([])).await;
        local.push(sent.elapsed());
        let sent = Instant::now();
        let target = &streams[index % WORKSPACES].1;
        rpc.call(
            "zcode-agent",
            "queryConversationCommandsV4",
            json!([{"workspacePath": target, "clock": true, "commands": []}]),
        )
        .await;
        agent.push(sent.elapsed());
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let window = started.elapsed();
    let during: Vec<usize> = streams
        .iter()
        .zip(&before)
        .map(|((listen, _), before)| rpc.events[listen] - before)
        .collect();
    eprintln!(
        "control_latency: {WORKSPACES} workspaces streaming; frames during the {:.1}s window per workspace: {during:?}",
        window.as_secs_f64()
    );
    // 采样窗口内 4 路都必须仍在出帧，否则测的不是“同时流式”。
    assert!(during.iter().all(|count| *count >= 10), "{during:?}");
    let local_p95 = summary("setting.get", local);
    let agent_p95 = summary("zcode-agent.queryConversationCommandsV4(clock)", agent);
    assert!(local_p95 <= P95_BUDGET, "setting.get p95 {local_p95:?}");
    assert!(
        agent_p95 <= P95_BUDGET,
        "queryConversationCommandsV4 p95 {agent_p95:?}"
    );
}
