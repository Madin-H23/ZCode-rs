//! Bot 读会话的 V4 适配对真实 Rust CLI 的集成测试（docs/specs/rust-host-bots.md 第 4、8 节）：
//! Bot 用自己的连接订阅 `conversation/<id>`，投影器把真实 CLI 的帧翻译成 Bot 事件——正文 chunk、
//! 工具调用与完成、轮次收口；第二轮只投影新增的内容。模型是本地 mock 的 OpenAI 兼容端点，HOME 与
//! 数据目录都是临时目录，不读写开发者的 `~/.zcode`。
//!
//! 缺省用进程内的嵌入式 runtime（docs/specs/rust-u3-single-process.md），不需要预先构建 CLI；设置
//! `ZCODE_TEST_CLI_RUST_BINARY` 时改用该可执行文件走子进程链路，作为差分对照。
#![expect(
    clippy::disallowed_methods,
    reason = "集成测试从进程环境取被测二进制与 PATH"
)]
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use zcode_host_services::agent::{AgentRuntime, AgentRuntimeConfig, WorkspaceTarget};
use zcode_host_services::bots::stream::{Projector, StreamEvent};
use zcode_host_services::bots::subscribe_conversation;
use zcode_host_services::providers::ProviderRuntime;
use zcode_host_services::settings::SettingStore;

const FIRST_REPLY: &str = "Hello from the mock model";
const TOOL_REPLY: &str = "The tool said hi";

/// runtime 的选择：缺省嵌入式（Host 的缺省链路）；`ZCODE_TEST_CLI_RUST_BINARY` 指向可执行文件时走子进程链路。
fn runtime_vars() -> BTreeMap<String, String> {
    match std::env::var_os("ZCODE_TEST_CLI_RUST_BINARY").map(PathBuf::from) {
        Some(cli) => {
            assert!(
                cli.is_file(),
                "ZCODE_TEST_CLI_RUST_BINARY is not a file: {}",
                cli.display()
            );
            // 缺陷修复：这里原来递归调用 runtime_vars() 自己，设置该变量时栈溢出，子进程链路的差分
            // 对照从未真正跑过；改为与 agent_cli.rs 相同的显式命令与 runtime 种类。
            BTreeMap::from([
                (
                    "ZCODE_AGENT_SERVER_COMMAND".to_owned(),
                    cli.to_string_lossy().into_owned(),
                ),
                (
                    "ZCODE_AGENT_SERVER_RUNTIME".to_owned(),
                    "zcode-cli-rust".to_owned(),
                ),
            ])
        }
        // 不设置任何选择变量：走 Host 的缺省链路（嵌入式）。
        None => BTreeMap::new(),
    }
}

fn chunk(delta: Value, finish: Value) -> Value {
    json!({
        "id": "mock", "object": "chat.completion.chunk", "created": 0, "model": "my-model",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    })
}

/// 请求里没有工具结果且用户要求用工具时回一个 Bash 工具调用；其余回文本（两段 chunk）。
fn respond(body: &str) -> String {
    let wants_tool = body.contains("use a tool") && !body.contains("\"role\":\"tool\"");
    let mut stop = chunk(
        json!({}),
        json!(if wants_tool { "tool_calls" } else { "stop" }),
    );
    stop["usage"] = json!({"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2});
    let events: Vec<Value> = if wants_tool {
        vec![
            chunk(
                json!({"role": "assistant", "tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                    "function": {"name": "Bash", "arguments": json!({"command": "echo hi", "description": "say hi"}).to_string()}}]}),
                Value::Null,
            ),
            stop,
        ]
    } else {
        let text = if body.contains("\"role\":\"tool\"") {
            TOOL_REPLY
        } else {
            FIRST_REPLY
        };
        let (head, tail) = text.split_at(5);
        vec![
            chunk(json!({"role": "assistant", "content": head}), Value::Null),
            chunk(json!({"content": tail}), Value::Null),
            stop,
        ]
    };
    let mut body = String::new();
    for event in events {
        body.push_str(&format!("data: {event}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

async fn serve_mock_model(listener: TcpListener) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(async move {
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 8192];
            let body_start = loop {
                let Ok(read) = stream.read(&mut chunk).await else {
                    return;
                };
                if read == 0 {
                    return;
                }
                buffer.extend_from_slice(&chunk[..read]);
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
                let Ok(read) = stream.read(&mut chunk).await else {
                    return;
                };
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
            let body = respond(&String::from_utf8_lossy(&buffer[body_start..]));
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        });
    }
}

/// 订阅、等到初始快照作为基线，发送一轮输入，收集投影事件直到轮次收口。
#[expect(
    clippy::print_stderr,
    reason = "设置 ZCODE_TEST_DUMP_FRAMES 时输出帧便于排查"
)]
async fn run_turn(
    runtime: &AgentRuntime,
    target: &WorkspaceTarget,
    session_id: &str,
    selection: &Value,
    text: &str,
) -> Vec<StreamEvent> {
    let mut frames: BoxStream<'static, Value> =
        subscribe_conversation(runtime.clone(), target.clone(), session_id.to_owned())
            .await
            .expect("subscribe conversation");
    let mut projector = Projector::new();
    let first = tokio::time::timeout(Duration::from_secs(30), frames.next())
        .await
        .expect("initial snapshot")
        .expect("frame");
    assert!(
        projector.apply(&first).is_empty(),
        "the baseline produces no events"
    );
    assert!(projector.is_initialized());
    let process = runtime.get_ready_client(target).await.unwrap();
    let envelope = json!({
        "commandId": format!("0190a000-0000-7000-8000-{:012}", text.len()),
        "clientId": "host-bots-test",
        "sessionId": session_id,
        "type": "sendText",
        "payload": {"text": text, "heldQueueDisposition": "keepQueueAndSend",
            "modelSelection": selection},
        "issuedAt": 1,
    });
    let ack = process
        .request_value("v4/command", &envelope)
        .await
        .unwrap();
    assert_eq!(ack["status"], "accepted", "ack: {ack}");
    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(frame) = frames.next().await {
            if std::env::var_os("ZCODE_TEST_DUMP_FRAMES").is_some() {
                eprintln!("FRAME {frame}");
            }
            events.extend(projector.apply(&frame));
            if projector.is_finished() {
                return;
            }
        }
    })
    .await
    .expect("turn never finished");
    events
}

fn message_text(events: &[StreamEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::MessageChunk(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn bots_read_real_cli_conversations_through_v4_frames() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let workspace_path = workspace.path().to_string_lossy().into_owned();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve_mock_model(listener));

    let config_dir = home.path().join(".zcode/v2");
    std::fs::create_dir_all(&config_dir).unwrap();
    let personal = config_dir.join("provider_config.json");
    std::fs::write(
        &personal,
        json!({"schemaVersion": 1, "config": {
            "providerOrder": ["new-provider"],
            "providerConfigRules": {"providerRules": [{
                "providerId": "new-provider", "providerName": "Mock",
                "config": {
                    "group": "standard-personal",
                    "access": {"type": "api-key", "apiKey": "sk-test"},
                    "api": {"type": "openai-chat-completions", "baseUrl": format!("http://127.0.0.1:{port}/v1")},
                    "personalModelIds": ["my-model"], "modelOrder": ["my-model"],
                },
            }]},
            "modelConfigRules": {
                "providerModelRules": [{"providerId": "new-provider", "modelId": "my-model", "config": {"enabled": true}}],
                "manualProviderModelRules": [],
            },
        }})
        .to_string(),
    )
    .unwrap();
    let builtin = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../config/provider/zcode-builtin.json");
    let home_text = home.path().to_string_lossy().into_owned();
    let providers = ProviderRuntime::new(
        &zcode_host_services::testing::spawner(),
        Some(builtin),
        personal,
    );
    // 与 Bot 相同：草稿态的提交模型取目标 Host 的首选选择（带思考档位）。
    let (revision, resolved) = providers.snapshot().await;
    let selection = resolved.model_selection_view(revision, None)["preferredSelection"].clone();
    assert_eq!(selection["modelId"], "my-model", "{selection}");
    let runtime = AgentRuntime::new(
        AgentRuntimeConfig {
            base_env: BTreeMap::from([
                ("HOME".to_owned(), home_text.clone()),
                ("USERPROFILE".to_owned(), home_text.clone()),
                ("ZCODE_DATA_BASE_DIR".to_owned(), home_text.clone()),
                ("PATH".to_owned(), std::env::var("PATH").unwrap_or_default()),
                ("NO_PROXY".to_owned(), "127.0.0.1,localhost".to_owned()),
            ]),
            command_vars: runtime_vars(),
            fallback_cwd: None,
        },
        SettingStore::new(&zcode_host_services::testing::spawner(), home.path()),
        providers,
    );
    let target = WorkspaceTarget::local(workspace_path.clone());
    // 与 Bot 建会话相同：yolo、首发前不落盘（deferred），带模型。
    let process = runtime.get_ready_client(&target).await.unwrap();
    let snapshot = process
        .request_value(
            "session/create",
            &json!({"workspace": target.workspace_ref(), "mode": "yolo",
                    "model": selection, "persistence": "deferred"}),
        )
        .await
        .unwrap();
    let session_id = snapshot["session"]["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();

    let events = run_turn(&runtime, &target, &session_id, &selection, "say hello").await;
    assert_eq!(message_text(&events), FIRST_REPLY, "events: {events:?}");
    assert_eq!(events.last(), Some(&StreamEvent::Complete));

    // 第二轮：初始快照里的上一轮只是基线，不会重放；工具调用与完成都被投影出来。
    let events = run_turn(
        &runtime,
        &target,
        &session_id,
        &selection,
        "please use a tool",
    )
    .await;
    let tool_call = events.iter().find_map(|event| match event {
        StreamEvent::ToolCall(tool) => Some(tool.clone()),
        _ => None,
    });
    let tool_call = tool_call.unwrap_or_else(|| panic!("no tool call in {events:?}"));
    assert_eq!(tool_call.tool_name, "Bash");
    assert_eq!(tool_call.input["command"], "echo hi");
    assert!(
        events.iter().any(
            |event| matches!(event, StreamEvent::ToolUpdate(tool) if tool.status == "completed")
        ),
        "events: {events:?}"
    );
    assert_eq!(message_text(&events), TOOL_REPLY, "events: {events:?}");
    assert_eq!(events.last(), Some(&StreamEvent::Complete));
    runtime.dispose_all().await;
}
