//! 已接受的输入在 runtime 实例重建后的结算（docs/specs/rust-u3-stage-b.md B10、3.4.2“重启”）：
//! 会话上一条输入正在运行、一条排在它后面时，调试构建的故障注入（`ZCODE_RUNTIME_FAULT_MARKER`）让
//! 实例的 Engine 循环 panic，Host 在下一次请求时重启实例。之后经 `queryConversationCommandsV4` 查两条
//! 输入：开跑的那条在 transcript 里（`accepted`），排队的那条结算为
//! `failed fault.command.inputDiscardedOnRestart`，同一 commandId 重提也得到它；会话照常可用。
//!
//! 模型是本地 mock 的 OpenAI 兼容端点，不需要任何真实凭据。两种链路都覆盖，且不受
//! `ZCODE_TEST_CLI_RUST_BINARY` 影响（故障注入只编进调试构建）：进程内的嵌入式 runtime，以及以
//! `zcode-cli-rust` 为名运行的合并二进制（`CARGO_BIN_EXE_zcode-host`，调试构建）走子进程链路。
#![cfg(feature = "embedded-runtime")]
#![expect(
    clippy::disallowed_methods,
    reason = "集成测试从进程环境取 PATH，并为 mock 模型起任务"
)]
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use zcode_host_control::ClientMode;
use zcode_host_kit::{CallContext, Reply, RpcValue, ServiceChannel};
use zcode_host_services::agent::api::AgentApi;
use zcode_host_services::agent::notifications::AgentEvents;
use zcode_host_services::agent::service::AgentChannel;
use zcode_host_services::agent::{AgentRuntime, AgentRuntimeConfig, WorkspaceTarget};
use zcode_host_services::providers::ProviderRuntime;
use zcode_host_services::settings::SettingStore;
use zcode_host_services::tasks::agent_calls;

const LIMIT: Duration = Duration::from_secs(60);
const FAULT: &str = "fault-marker-b10-7c41";
/// 带这个标记的输入，模型请求一直挂起，直到 runtime 关闭连接（run 被取消或实例结束）。
const HOLD: &str = "[hold]";
const TITLE: &str = "Generate a concise title";
const REPLY: &str = "done";

/// 进程内的嵌入式 runtime：不设置任何选择变量。
#[tokio::test(flavor = "multi_thread")]
async fn accepted_inputs_are_settled_after_an_embedded_runtime_panic() {
    accepted_inputs_after_a_panic(BTreeMap::new()).await;
}

/// 子进程链路：合并二进制以 `zcode-cli-rust` 为名经它的 CLI 入口运行（Unix 用符号链接，其他平台用
/// 副本）。Engine 循环 panic 后 CLI 进程以错误退出，Host 走子进程退出的路径。
#[tokio::test(flavor = "multi_thread")]
async fn accepted_inputs_are_settled_after_a_child_process_runtime_panic() {
    let bin = tempfile::tempdir().unwrap();
    let cli = bin
        .path()
        .join(format!("zcode-cli-rust{}", std::env::consts::EXE_SUFFIX));
    #[cfg(unix)]
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_zcode-host"), &cli).unwrap();
    #[cfg(not(unix))]
    std::fs::copy(env!("CARGO_BIN_EXE_zcode-host"), &cli).unwrap();
    accepted_inputs_after_a_panic(BTreeMap::from([
        (
            "ZCODE_AGENT_SERVER_COMMAND".to_owned(),
            cli.to_string_lossy().into_owned(),
        ),
        (
            "ZCODE_AGENT_SERVER_RUNTIME".to_owned(),
            "zcode-cli-rust".to_owned(),
        ),
    ]))
    .await;
}

async fn accepted_inputs_after_a_panic(command_vars: BTreeMap<String, String>) {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().to_string_lossy().into_owned();
    let target = WorkspaceTarget::local(path.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let holding = serve_mock_model(listener);
    let personal = provider_config(home.path(), port);
    let home_text = home.path().to_string_lossy().into_owned();
    let runtime = AgentRuntime::new(
        AgentRuntimeConfig {
            base_env: BTreeMap::from([
                ("HOME".to_owned(), home_text.clone()),
                ("USERPROFILE".to_owned(), home_text.clone()),
                ("ZCODE_DATA_BASE_DIR".to_owned(), home_text),
                ("PATH".to_owned(), std::env::var("PATH").unwrap_or_default()),
                ("NO_PROXY".to_owned(), "127.0.0.1,localhost".to_owned()),
                ("ZCODE_RUNTIME_FAULT_MARKER".to_owned(), FAULT.to_owned()),
            ]),
            command_vars,
            fallback_cwd: None,
        },
        SettingStore::new(home.path()),
        ProviderRuntime::new(
            Some(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../../../config/provider/zcode-builtin.json"),
            ),
            personal,
        ),
    );
    let events = AgentEvents::start(runtime.extensions(), None);
    let api = AgentApi::new(runtime.clone(), home.path(), events, None);
    let members = zcode_host_services::agent::members::agent_service(&api).build(None);
    let channel = AgentChannel::new(api, Some(members));
    let context = CallContext {
        connection: 1,
        attachment_id: "test".into(),
        client_mode: ClientMode::DesktopContinuous,
        window_id: None,
    };
    let h = Harness {
        channel,
        context,
        runtime,
        target,
        path,
        holding,
    };
    h.call("helloConversationV4", Value::Null).await;
    h.call(
        "initializeConversationV4",
        json!({"kind": "clientHello", "protocolVersion": 3, "clientId": "client-test", "clientKind": "desktop", "appVersion": "unknown"}),
    )
    .await;

    let created = h.command(create_session(&h.path, "say hello")).await;
    assert_eq!(created["status"], "accepted", "{created}");
    let session = created["result"]["sessionId"].as_str().unwrap().to_owned();
    h.idle(&session).await;

    // 一条输入开跑（模型请求挂起），一条排在它后面。
    let (running, queued) = (command_id(2), command_id(3));
    let ack = h
        .command(send_text(
            &h.path,
            &session,
            &running,
            &format!("{HOLD} first"),
        ))
        .await;
    assert_eq!(ack["status"], "accepted", "{ack}");
    h.held(1).await;
    let ack = h
        .command(send_text(&h.path, &session, &queued, "second"))
        .await;
    assert_eq!(
        (&ack["status"], &ack["result"]["delivery"]),
        (&json!("accepted"), &json!("queue")),
        "{ack}"
    );
    let state = h.input_queue(&session).await;
    assert_eq!(state["activeInput"]["inputId"], running.as_str(), "{state}");
    let items = state["queue"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{state}");
    assert_eq!(items[0]["sourceCommandId"], queued.as_str(), "{state}");
    assert_eq!(h.runtime.existing(&h.target).await.unwrap().generation, 1);

    // 带标记的请求让 Engine 循环 panic：这次调用失败，实例结束，挂起的模型请求随之断开。
    h.channel
        .call(
            &h.context,
            "sendConversationCommandV4",
            vec![RpcValue::Json(create_session(&h.path, FAULT))],
        )
        .await
        .unwrap_err();
    tokio::time::timeout(LIMIT, async {
        while h.runtime.existing(&h.target).await.is_some() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the panicked runtime was not released");
    h.held(0).await;

    // 下一次请求（查询命令状态）让 Host 重启实例，代次加一。
    let results = h.query(&session, &[&running, &queued]).await;
    assert_eq!(h.runtime.existing(&h.target).await.unwrap().generation, 2);
    // 开跑的输入在 transcript 里。
    assert_eq!(
        results[0]["result"],
        json!({"commandId": running, "status": "accepted", "revisionAtDecision": 0}),
        "{results:?}"
    );
    // 已接受但没开跑的输入明确结算为“重启时丢弃”，不是 unknown，也不会被重放。
    let discarded = json!({
        "commandId": queued,
        "status": "failed",
        "reasonCode": "fault.command.inputDiscardedOnRestart",
        "message": "Input was discarded when the CLI restarted; confirm before resending.",
        "revisionAtDecision": 0,
        "result": {"type": "inputDisposition", "delivery": "queue"},
    });
    assert_eq!(results[1]["result"], discarded, "{results:?}");
    let state = h.input_queue(&session).await;
    assert!(
        state["queue"]["items"].as_array().unwrap().is_empty() && state["activeInput"].is_null(),
        "{state}"
    );
    // 同一 commandId 重提：得到同一个结算，不会作为新输入再执行。
    let resent = h
        .command(send_text(&h.path, &session, &queued, "second"))
        .await;
    assert_eq!(resent, discarded, "{resent}");
    // 开跑过的输入重提得到 duplicate，同样不会再执行一次。
    let resent = h
        .command(send_text(
            &h.path,
            &session,
            &running,
            &format!("{HOLD} first"),
        ))
        .await;
    assert_eq!(resent["status"], "duplicate", "{resent}");

    // 会话照常可用：新的输入开跑并完成。
    let next = command_id(4);
    let ack = h
        .command(send_text(&h.path, &session, &next, "say hello again"))
        .await;
    assert_eq!(ack["status"], "accepted", "{ack}");
    h.idle(&session).await;
    let results = h.query(&session, &[&next]).await;
    assert_eq!(results[0]["result"]["status"], "accepted", "{results:?}");
    // transcript：开跑的输入留在原位（被中断，没有回复），排队的输入从未执行，新的输入得到回复。
    let read = agent_calls::read_session(&h.runtime, &h.target, &session)
        .await
        .unwrap();
    let transcript: Vec<(&str, String)> = read["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| {
            let text = message["parts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|part| part["text"].as_str())
                .collect();
            (message["info"]["role"].as_str().unwrap_or_default(), text)
        })
        .collect();
    let line = |role, text: &str| (role, text.to_owned());
    assert_eq!(
        transcript,
        [
            line("user", "say hello"),
            line("assistant", REPLY),
            line("user", &format!("{HOLD} first")),
            line("user", "say hello again"),
            line("assistant", REPLY),
        ],
        "{read}"
    );
    assert_eq!(h.runtime.existing(&h.target).await.unwrap().generation, 2);

    h.channel.connection_closed(&h.context);
    h.runtime.dispose_all().await;
}

struct Harness {
    channel: Arc<AgentChannel>,
    context: CallContext,
    runtime: AgentRuntime,
    target: WorkspaceTarget,
    path: String,
    holding: Arc<AtomicUsize>,
}

impl Harness {
    async fn call(&self, method: &str, value: Value) -> Value {
        let args = if value.is_null() {
            Vec::new()
        } else {
            vec![RpcValue::Json(value)]
        };
        let reply = tokio::time::timeout(LIMIT, self.channel.call(&self.context, method, args))
            .await
            .unwrap_or_else(|_| panic!("{method} timed out"))
            .unwrap_or_else(|error| panic!("{method} failed: {error:?}"));
        match reply {
            Reply::Undefined => Value::Null,
            Reply::Value(value) => value,
            Reply::RawJson(raw) => serde_json::from_str(&raw).unwrap(),
            Reply::Bytes(_) => panic!("unexpected bytes"),
            Reply::Encoded(body) => zcode_host_rpc::Reader::new(&body)
                .value()
                .unwrap()
                .into_json(),
        }
    }

    /// `sendConversationCommandV4`，返回 ACK。
    async fn command(&self, params: Value) -> Value {
        self.call("sendConversationCommandV4", params).await
    }

    /// `queryConversationCommandsV4` 的 `results`，与 `commands` 同序。
    async fn query(&self, session: &str, commands: &[&str]) -> Vec<Value> {
        let keys: Vec<Value> = commands
            .iter()
            .map(|command| json!({"sessionId": session, "commandId": command}))
            .collect();
        let answer = self
            .call(
                "queryConversationCommandsV4",
                json!({"workspacePath": self.path, "commands": keys}),
            )
            .await;
        let results = answer["results"].as_array().cloned().unwrap_or_default();
        assert_eq!(results.len(), commands.len(), "{answer}");
        for (result, command) in results.iter().zip(commands) {
            assert_eq!(result["key"]["commandId"], *command, "{answer}");
        }
        results
    }

    async fn input_queue(&self, session: &str) -> Value {
        agent_calls::read(
            &self.runtime,
            &self.target,
            "v4/conversation/inputQueue",
            json!({"sessionId": session}),
        )
        .await
        .unwrap()
    }

    async fn idle(&self, session: &str) {
        tokio::time::timeout(LIMIT, async {
            loop {
                let state = self.input_queue(session).await;
                if state["running"] == false {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the session did not become idle");
    }

    async fn held(&self, count: usize) {
        tokio::time::timeout(LIMIT, async {
            while self.holding.load(Ordering::SeqCst) != count {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("expected {count} held model requests"));
    }
}

fn command_id(n: u32) -> String {
    format!("0190a000-0000-7000-8000-{n:012}")
}

fn selection() -> Value {
    json!({"providerId": "new-provider", "modelId": "my-model"})
}

fn create_session(workspace: &str, text: &str) -> Value {
    json!({"workspacePath": workspace, "envelope": {
        "commandId": command_id(if text == FAULT { 99 } else { 1 }),
        "clientId": "client-test",
        "sessionId": null,
        "type": "createSession",
        "payload": {
            "workspaceId": workspace,
            "firstInput": {"text": text, "modelSelection": selection(), "mode": "build", "planEnabled": false},
        },
        "issuedAt": 1,
    }})
}

fn send_text(workspace: &str, session: &str, command: &str, text: &str) -> Value {
    // 运行中的输入按 queue 排队（不取会话的 followupMode，可能是 guide）。
    json!({"workspacePath": workspace, "envelope": {
        "commandId": command,
        "clientId": "client-test",
        "sessionId": session,
        "type": "sendText",
        "payload": {"text": text, "modelSelection": selection(), "requestedDelivery": "queue"},
        "issuedAt": 1,
    }})
}

/// 个人 provider 配置：一个指向本地 mock 的 OpenAI 兼容 provider。返回配置文件路径。
fn provider_config(home: &Path, port: u16) -> PathBuf {
    let config_dir = home.join(".zcode/v2");
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
    personal
}

/// 这次请求要回答的用户输入：最后一条不是系统提醒（`<…>` 包裹）的 user 消息。
fn held(body: &str) -> bool {
    let request: Value = serde_json::from_str(body).unwrap_or_default();
    let text = |content: &Value| match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect(),
        _ => String::new(),
    };
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .rev()
        .filter(|m| m["role"] == "user")
        .map(|m| text(&m["content"]))
        .find(|text| !text.trim_start().starts_with('<'))
        .is_some_and(|text| text.contains(HOLD))
}

async fn read_body(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let start = loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8_lossy(&buffer[..start]).to_lowercase();
    let length = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while buffer.len() < start + length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    Some(String::from_utf8_lossy(&buffer[start..]).into_owned())
}

/// OpenAI 兼容的 mock 模型：带 HOLD 的输入挂起到连接被关闭为止（计数 +1，关闭后 -1）；其余请求
/// （含标题生成）立即回 `REPLY`。
fn serve_mock_model(listener: TcpListener) -> Arc<AtomicUsize> {
    let holding = Arc::new(AtomicUsize::new(0));
    let count = holding.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let count = count.clone();
            tokio::spawn(async move {
                let Some(body) = read_body(&mut stream).await else {
                    return;
                };
                if !body.contains(TITLE) && held(&body) {
                    count.fetch_add(1, Ordering::SeqCst);
                    let mut rest = [0u8; 64];
                    while matches!(stream.read(&mut rest).await, Ok(n) if n > 0) {}
                    count.fetch_sub(1, Ordering::SeqCst);
                    return;
                }
                let chunk = |delta: Value, finish: Value| {
                    json!({"id": "mock", "object": "chat.completion.chunk", "created": 0,
                        "model": "my-model", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
                };
                let mut stop = chunk(json!({}), json!("stop"));
                stop["usage"] =
                    json!({"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2});
                let body = format!(
                    "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    chunk(json!({"role": "assistant", "content": REPLY}), Value::Null),
                    stop
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    holding
}
