//! 用真实的 Rust CLI 走一遍核心对话链路：握手 → createSession（带首条输入）→ 订阅会话 →
//! 收到模型流式回复。模型是本地 mock 的 OpenAI 兼容端点，不需要任何真实凭据。
//!
//! CLI 可执行文件取 `ZCODE_TEST_CLI_RUST_BINARY`，否则用工作区内的 release 构建；都不存在时跳过。
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use zcode_host_control::ClientMode;
use zcode_host_kit::{CallContext, Reply, RpcValue, ServiceChannel};
use zcode_host_services::agent::service::AgentChannel;
use zcode_host_services::agent::sessions_index::{IndexEvent, SessionsIndexObserver};
use zcode_host_services::agent::{AgentRuntime, AgentRuntimeConfig};
use zcode_host_services::agent::{WorkspaceTarget, session};
use zcode_host_services::providers::ProviderRuntime;
use zcode_host_services::settings::SettingStore;

const MOCK_REPLY: &str = "Hello from the mock model";

fn cli_binary() -> Option<PathBuf> {
    let path = std::env::var_os("ZCODE_TEST_CLI_RUST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../../zcode-cli-rust/target/release/zcode-cli-rust")
        });
    path.is_file().then_some(path)
}

/// 每个请求都回同一段 SSE 流（含标题生成等附带请求）。
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
            let chunk = |delta: Value, finish: Value| {
                json!({
                    "id": "mock", "object": "chat.completion.chunk", "created": 0, "model": "my-model",
                    "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
                })
            };
            let mut stop = chunk(json!({}), json!("stop"));
            stop["usage"] = json!({"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2});
            let body = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                chunk(
                    json!({"role": "assistant", "content": MOCK_REPLY}),
                    Value::Null
                ),
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
}

fn args(value: Value) -> Vec<RpcValue> {
    vec![RpcValue::Json(value)]
}

async fn call(channel: &AgentChannel, context: &CallContext, method: &str, value: Value) -> Value {
    let reply = channel
        .call(
            context,
            method,
            if value.is_null() {
                Vec::new()
            } else {
                args(value)
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{method} failed: {error:?}"));
    match reply {
        Reply::Undefined => Value::Null,
        Reply::Value(value) => value,
        Reply::RawJson(raw) => serde_json::from_str(&raw).unwrap(),
        Reply::Bytes(_) => panic!("unexpected bytes"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn core_chat_flow_against_rust_cli() {
    let Some(cli) = cli_binary() else {
        eprintln!("skipped: Rust CLI release binary not found");
        return;
    };
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
    let base_env = BTreeMap::from([
        ("HOME".to_owned(), home_text.clone()),
        ("USERPROFILE".to_owned(), home_text.clone()),
        ("ZCODE_DATA_BASE_DIR".to_owned(), home_text.clone()),
        ("PATH".to_owned(), std::env::var("PATH").unwrap_or_default()),
        ("NO_PROXY".to_owned(), "127.0.0.1,localhost".to_owned()),
    ]);
    let command_vars = BTreeMap::from([
        (
            "ZCODE_AGENT_SERVER_COMMAND".to_owned(),
            cli.to_string_lossy().into_owned(),
        ),
        (
            "ZCODE_AGENT_SERVER_RUNTIME".to_owned(),
            "zcode-cli-rust".to_owned(),
        ),
    ]);
    let providers = ProviderRuntime::new(Some(builtin), personal);
    let runtime = AgentRuntime::new(
        AgentRuntimeConfig {
            base_env,
            command_vars,
            search_roots: Vec::new(),
            fallback_cwd: None,
        },
        SettingStore::new(home.path()),
        providers,
    );
    let channel = AgentChannel::new(runtime.clone(), None);
    let observer = SessionsIndexObserver::start(runtime.clone());
    let mut index_events = Box::pin(observer.events().subscribe());
    let context = CallContext {
        connection: 1,
        attachment_id: "test".into(),
        client_mode: ClientMode::DesktopContinuous,
    };
    let target = json!({"workspacePath": workspace_path});
    let mut lifecycle = Box::pin(runtime.lifecycle().subscribe());

    // 握手前的 V4 调用必须被拒绝。
    let early = channel
        .call(&context, "subscribeSessionsIndexV4", args(target.clone()))
        .await
        .unwrap_err();
    assert_eq!(early.message, "fault.connection.handshakeRequired");

    let hello = call(&channel, &context, "helloConversationV4", Value::Null).await;
    assert_eq!(hello["deliveryProfile"], "continuous");
    assert!(
        hello["connectionId"]
            .as_str()
            .unwrap()
            .starts_with("host-rpc-")
    );
    call(
        &channel,
        &context,
        "initializeConversationV4",
        json!({"kind": "clientHello", "protocolVersion": 3, "clientId": "client-test", "clientKind": "desktop", "appVersion": "unknown"}),
    )
    .await;

    // sessions-index 只用已有进程：此时还没有 CLI。
    let dormant = channel
        .call(
            &context,
            "subscribeSessionsIndexV4",
            args(json!({"workspacePath": workspace_path, "runtimePolicy": "existing-only"})),
        )
        .await
        .unwrap_err();
    assert_eq!(dormant.code_str(), Some("ZCODE_AGENT_RUNTIME_UNAVAILABLE"));

    let mut frames = channel
        .listen(
            &context,
            "onDynamicConversationFrame",
            RpcValue::Json(target.clone()),
        )
        .unwrap();

    let envelope = json!({
        "commandId": "0190a000-0000-7000-8000-000000000001",
        "clientId": "client-test",
        "sessionId": null,
        "type": "createSession",
        "payload": {
            "workspaceId": workspace_path,
            "firstInput": {
                "text": "say hello",
                "modelSelection": {"providerId": "new-provider", "modelId": "my-model"},
                "mode": "build",
                "planEnabled": false,
            },
        },
        "issuedAt": 1,
    });
    let ack = tokio::time::timeout(
        Duration::from_secs(60),
        call(
            &channel,
            &context,
            "sendConversationCommandV4",
            json!({"workspacePath": workspace_path, "envelope": envelope}),
        ),
    )
    .await
    .expect("createSession timed out");
    assert_eq!(ack["status"], "accepted", "ack: {ack}");
    let session_id = ack["result"]["sessionId"].as_str().unwrap().to_owned();

    let available = lifecycle.next().await.unwrap();
    assert_eq!(available["state"], "available");
    assert_eq!(available["runtimeIdentity"]["generation"], 1);

    let subscribed = call(
        &channel,
        &context,
        "subscribeConversationV4",
        json!({"workspacePath": workspace_path, "sessionId": session_id}),
    )
    .await;
    let subscription_id = subscribed["ack"]["subscriptionId"]
        .as_str()
        .unwrap()
        .to_owned();

    // 初始快照与后续增量都应送达；等到 mock 回复的文本出现。
    let reply_seen = tokio::time::timeout(Duration::from_secs(60), async {
        let mut count = 0;
        while let Some(Reply::RawJson(raw)) = frames.next().await {
            count += 1;
            let frame: Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(frame["subscriptionId"], subscription_id.as_str());
            assert_eq!(frame["topic"], format!("conversation/{session_id}"));
            if raw.contains(MOCK_REPLY) {
                return count;
            }
        }
        0
    })
    .await
    .expect("assistant reply not streamed");
    assert!(reply_seen > 0);

    // Host 自己的 sessions-index 观察者：进程可用后自动订阅，拿到含新会话的摘要。
    let observed = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(event) = index_events.next().await {
            let sessions: Vec<Value> = match event.as_ref() {
                IndexEvent::Snapshot { sessions, .. } => sessions.clone(),
                IndexEvent::Upserted { session, .. } => vec![session.clone()],
                _ => continue,
            };
            if sessions
                .iter()
                .any(|summary| summary["sessionId"] == session_id.as_str())
            {
                return true;
            }
        }
        false
    })
    .await
    .expect("observer never saw the session");
    assert!(observed);
    assert!(observer.sessions(&workspace_path).contains_key(&session_id));

    let presentation = session::read_workspace_presentation(
        &runtime,
        &WorkspaceTarget::local(workspace_path.clone()),
    )
    .await
    .unwrap();
    assert_eq!(
        presentation["workspace"]["workspaceKey"],
        workspace_path.as_str()
    );
    assert!(presentation["slashCommands"].is_array());
    assert!(presentation["executionCapabilities"]["permissionModes"].is_array());

    // 进程已在运行：sessions-index（existing-only）现在可以订阅。
    let index = call(
        &channel,
        &context,
        "subscribeSessionsIndexV4",
        json!({"workspacePath": workspace_path, "runtimePolicy": "existing-only"}),
    )
    .await;
    assert!(index["ack"]["subscriptionId"].is_string());

    // 不属于本连接的订阅不能 resync。
    let foreign = channel
        .call(
            &context,
            "resyncConversationV4",
            args(json!({"workspacePath": workspace_path, "subscriptionId": "nope", "base": null})),
        )
        .await
        .unwrap_err();
    assert_eq!(foreign.message, "fault.subscription.notOwned");

    channel.connection_closed(&context);
    runtime.dispose_all().await;
    let unavailable = tokio::time::timeout(Duration::from_secs(5), lifecycle.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unavailable["state"], "unavailable");
}
