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
//!
//! `runtime_link_comparison`（`#[ignore]`）是 U3 的对照测量（docs/reports/rust-u3-embedded-runtime.md）：
//! N（`ZCODE_PERF_WORKSPACES`，缺省 4）个 workspace 同时流式时的控制 RPC 延迟、Host 进程（子进程链路另加
//! 各 CLI 进程）的 RSS 与 CPU，以及 workspace runtime 的冷启动就绪时间；结果以一行 `runtime_link_perf
//! {json}` 打印。用发布构建运行：
//! `cargo test --release -p zcode-host --test control_latency runtime_link_comparison -- --ignored --nocapture`。
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

/// runtime 的选择：缺省嵌入式（Host 的缺省链路）；`ZCODE_TEST_CLI_RUST_BINARY` 指向可执行文件时走子进程链路。
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
        // 不设置任何选择变量：走 Host 的缺省链路（嵌入式）。
        None => Vec::new(),
    }
}

fn sse(delta: Value, finish: Value) -> String {
    let chunk = json!({
        "id": "mock", "object": "chat.completion.chunk", "created": 0, "model": "mock-model",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    });
    format!("data: {chunk}\n\n")
}

/// 主对话请求（带 tools）每 20 ms 出一段，共 `chunks` 段；标题生成等附带请求立即结束。
async fn serve_mock_model(listener: TcpListener, chunks: usize) {
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
                for index in 0..chunks {
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

/// 一个已就绪的 Host：控制通道与 desktop-continuous 的 RPC 连接（握手已完成）。
struct Host {
    control: Control,
    rpc: Rpc,
}

async fn start_host(home: &Path) -> Host {
    let builtin = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../config/provider/zcode-builtin.json");
    let mut child = zcode_proc::command(env!("CARGO_BIN_EXE_zcode-host"))
        .args(["--allowed-origin", "file://"])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("TMPDIR", std::env::temp_dir())
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("ZCODE_DESKTOP_HOME_DIR", home)
        .env("ZCODE_DATA_BASE_DIR", home)
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
    Host { control, rpc }
}

/// 在 workspace 里建会话并订阅；返回帧监听的 id。
async fn start_stream(rpc: &mut Rpc, index: usize, path: &str) -> i64 {
    let listen = rpc
        .listen(
            "zcode-agent",
            "onDynamicConversationFrame",
            json!({"workspacePath": path}),
        )
        .await;
    let envelope = json!({
        "commandId": format!("0190a000-0000-7000-8000-{index:012}"),
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
    listen
}

/// 等所有流都开始出帧（每路至少 5 帧）。
async fn wait_streaming(rpc: &mut Rpc, streams: &[(i64, String)]) {
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
}

/// 流式进行中交替采样两类控制 RPC；返回两组延迟与窗口内每路新到的帧数。
async fn sample_control_rpcs(
    rpc: &mut Rpc,
    streams: &[(i64, String)],
) -> (Vec<Duration>, Vec<Duration>, Vec<usize>, Duration) {
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
        let target = &streams[index % streams.len()].1;
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
    let during = streams
        .iter()
        .zip(&before)
        .map(|((listen, _), before)| rpc.events[listen] - before)
        .collect();
    (local, agent, during, window)
}

#[tokio::test(flavor = "multi_thread")]
async fn control_rpc_p95_while_four_workspaces_stream() {
    let home = tempfile::tempdir().unwrap();
    let workspaces: Vec<tempfile::TempDir> = (0..WORKSPACES)
        .map(|_| tempfile::tempdir().unwrap())
        .collect();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve_mock_model(listener, STREAM_CHUNKS));
    write_provider_config(home.path(), port);
    let Host {
        control: _control,
        mut rpc,
    } = start_host(home.path()).await;

    let mut streams = Vec::new();
    for (index, workspace) in workspaces.iter().enumerate() {
        let path = workspace.path().to_string_lossy().into_owned();
        streams.push((start_stream(&mut rpc, index, &path).await, path));
    }
    // 等 4 路都开始出帧，再在流式进行中采样。
    wait_streaming(&mut rpc, &streams).await;
    let (local, agent, during, window) = sample_control_rpcs(&mut rpc, &streams).await;
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

/// 一个进程在某一时刻的 RSS（KiB）与累计 CPU 时间。
#[derive(Clone, Copy)]
struct Usage {
    pid: u32,
    rss_kib: u64,
    cpu: Duration,
}

/// `ps` 的 `time` 列：`[[时:]分:]秒.百分秒`。
fn parse_cpu_time(text: &str) -> Duration {
    let mut seconds = 0.0;
    for part in text.split(':') {
        seconds = seconds * 60.0 + part.parse::<f64>().unwrap_or(0.0);
    }
    Duration::from_secs_f64(seconds)
}

/// Host 进程与它的 runtime 子进程（CLI；嵌入式链路没有）各自的用量。
async fn usage_of(host: u32) -> (Usage, Vec<Usage>) {
    let output = zcode_proc::command("ps")
        .args(["-axo", "pid=,ppid=,rss=,time=,command="])
        .output()
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    let mut host_usage = Usage {
        pid: host,
        rss_kib: 0,
        cpu: Duration::ZERO,
    };
    let mut children = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(rss), Some(time)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let command = fields.collect::<Vec<_>>().join(" ");
        let usage = Usage {
            pid: pid.parse().unwrap_or(0),
            rss_kib: rss.parse().unwrap_or(0),
            cpu: parse_cpu_time(time),
        };
        if pid.parse::<u32>().ok() == Some(host) {
            host_usage = usage;
        } else if ppid.parse::<u32>().ok() == Some(host) && command.contains("zcode-cli-rust") {
            children.push(usage);
        }
    }
    (host_usage, children)
}

/// macOS `footprint`（活动监视器的内存值，不含分配器已释放的页；`ps` 的 RSS 含这些页）；其他平台为 None。
async fn footprint_kib(pid: u32) -> Option<u64> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let output = zcode_proc::command("footprint")
        .args(["-p", &pid.to_string()])
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let rest = text.split("Footprint:").nth(1)?;
    let mut parts = rest.split_whitespace();
    let value: f64 = parts.next()?.parse().ok()?;
    let scale = match parts.next()? {
        "B" => 1.0 / 1024.0,
        "KB" => 1.0,
        "MB" => 1024.0,
        "GB" => 1024.0 * 1024.0,
        _ => return None,
    };
    Some((value * scale).round() as u64)
}

fn millis(duration: Duration) -> f64 {
    (duration.as_secs_f64() * 1e5).round() / 1e2
}

fn latency_json(mut samples: Vec<Duration>) -> Value {
    samples.sort();
    json!({
        "p50": millis(percentile(&samples, 0.5)),
        "p95": millis(percentile(&samples, 0.95)),
        "p99": millis(percentile(&samples, 0.99)),
        "max": millis(*samples.last().unwrap()),
    })
}

/// U3 对照测量（见文件头）：嵌入式链路与子进程链路各跑一次，比较打印出来的 JSON。
#[tokio::test(flavor = "multi_thread")]
#[ignore = "measurement; run in release with --ignored --nocapture"]
async fn runtime_link_comparison() {
    let workspaces_count: usize = std::env::var("ZCODE_PERF_WORKSPACES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(4);
    // 流式要覆盖冷启动、采样窗口与负载下的冷启动：约 40 s。
    const CHUNKS: usize = 2000;
    let home = tempfile::tempdir().unwrap();
    let workspaces: Vec<tempfile::TempDir> = (0..=workspaces_count)
        .map(|_| tempfile::tempdir().unwrap())
        .collect();
    let paths: Vec<String> = workspaces
        .iter()
        .map(|dir| dir.path().to_string_lossy().into_owned())
        .collect();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve_mock_model(listener, CHUNKS));
    write_provider_config(home.path(), port);
    let Host { control, mut rpc } = start_host(home.path()).await;
    let host_pid = control._child.id().unwrap();
    let (idle, _) = usage_of(host_pid).await;

    // 冷启动就绪：`zcode-agent.initialize` 拉起 runtime 并等到它可用（存储就绪、偏好推送完成）。
    let mut cold = Vec::new();
    for path in &paths[..workspaces_count] {
        let started = Instant::now();
        let result = rpc
            .call(
                "zcode-agent",
                "initialize",
                json!([{"workspacePath": path}]),
            )
            .await;
        cold.push(started.elapsed());
        assert_eq!(result["available"], true, "{result}");
    }
    let (ready, ready_children) = usage_of(host_pid).await;

    let mut streams = Vec::new();
    for (index, path) in paths[..workspaces_count].iter().enumerate() {
        streams.push((start_stream(&mut rpc, index, path).await, path.clone()));
    }
    wait_streaming(&mut rpc, &streams).await;

    // 采样窗口：控制 RPC 与进程用量（每 200 ms 一次 RSS；CPU 取窗口首尾的累计时间之差）。
    let (start_host_usage, start_children) = usage_of(host_pid).await;
    let window_started = Instant::now();
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    let sampling = tokio::spawn(async move {
        let mut samples = Vec::new();
        loop {
            let (host, children) = usage_of(host_pid).await;
            samples.push((
                host.rss_kib,
                children.iter().map(|c| c.rss_kib).sum::<u64>(),
            ));
            tokio::select! {
                _ = stopped.changed() => return samples,
                () = tokio::time::sleep(Duration::from_millis(200)) => {}
            }
        }
    });
    let (local, agent, during, _) = sample_control_rpcs(&mut rpc, &streams).await;
    let window = window_started.elapsed();
    let (end_host_usage, end_children) = usage_of(host_pid).await;
    let host_footprint = footprint_kib(host_pid).await;
    let mut children_footprint = Some(0);
    for child in &end_children {
        children_footprint = match (children_footprint, footprint_kib(child.pid).await) {
            (Some(sum), Some(value)) => Some(sum + value),
            _ => None,
        };
    }
    stop.send_replace(true);
    let rss = sampling.await.unwrap();
    assert!(during.iter().all(|count| *count >= 10), "{during:?}");
    let cpu_percent = |start: Duration, end: Duration| {
        (end.saturating_sub(start).as_secs_f64() / window.as_secs_f64() * 1000.0).round() / 10.0
    };
    let children_cpu = |usages: &[Usage]| usages.iter().map(|usage| usage.cpu).sum::<Duration>();

    // 负载下的冷启动：N 路仍在流式时再拉起一个 workspace 的 runtime。
    let started = Instant::now();
    let result = rpc
        .call(
            "zcode-agent",
            "initialize",
            json!([{"workspacePath": paths[workspaces_count]}]),
        )
        .await;
    let cold_under_load = started.elapsed();
    assert_eq!(result["available"], true, "{result}");
    let still_streaming = streams
        .iter()
        .all(|(listen, _)| rpc.events[listen] < CHUNKS);
    assert!(still_streaming, "streams ended before the last measurement");

    let link = if std::env::var_os("ZCODE_TEST_CLI_RUST_BINARY").is_some() {
        "child"
    } else {
        "embedded"
    };
    // 机器负载（其他进程同时编译时数字偏高；报告里注明）。
    let load = zcode_proc::command("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .await
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_default();
    let report = json!({
        "link": link,
        "loadAvg": load,
        "workspaces": workspaces_count,
        "coldReadyMs": cold.iter().map(|d| millis(*d)).collect::<Vec<_>>(),
        "coldReadyUnderLoadMs": millis(cold_under_load),
        "settingGetMs": latency_json(local),
        "agentQueryMs": latency_json(agent),
        "framesPerWorkspaceInWindow": during,
        "windowS": (window.as_secs_f64() * 10.0).round() / 10.0,
        "hostRssKiB": {
            "idle": idle.rss_kib,
            "afterColdStarts": ready.rss_kib,
            "streamingMean": rss.iter().map(|(host, _)| host).sum::<u64>() / rss.len() as u64,
            "streamingMax": rss.iter().map(|(host, _)| *host).max().unwrap_or(0),
        },
        "childrenRssKiB": {
            "afterColdStarts": ready_children.iter().map(|c| c.rss_kib).sum::<u64>(),
            "streamingMean": rss.iter().map(|(_, children)| children).sum::<u64>() / rss.len() as u64,
            "streamingMax": rss.iter().map(|(_, children)| *children).max().unwrap_or(0),
        },
        "footprintKiBEndOfWindow": {
            "host": host_footprint,
            "children": children_footprint,
        },
        "cpuPercent": {
            "host": cpu_percent(start_host_usage.cpu, end_host_usage.cpu),
            "children": cpu_percent(children_cpu(&start_children), children_cpu(&end_children)),
        },
        "childProcesses": end_children.len(),
    });
    eprintln!("runtime_link_perf {report}");
}
