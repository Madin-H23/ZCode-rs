//! 用真实的 Rust CLI 走一遍核心对话链路：握手 → createSession（带首条输入）→ 订阅会话 →
//! 收到模型流式回复。模型是本地 mock 的 OpenAI 兼容端点，不需要任何真实凭据。
//!
//! 两种链路都缺省覆盖（docs/specs/rust-host-cli-boundary.md §5）：
//!
//! - 进程内的嵌入式 runtime（docs/specs/rust-u3-single-process.md），不需要预先构建 CLI；设置
//!   `ZCODE_TEST_CLI_RUST_BINARY` 时改用该可执行文件走子进程链路，作为与独立 CLI 的差分对照；
//! - 子进程链路：以 `zcode-cli-rust` 为名的合并二进制（`CARGO_BIN_EXE_zcode-host`，所以测试放在
//!   二进制的 crate 里）经它的 CLI 入口运行。
#![cfg(feature = "embedded-runtime")]
#![expect(
    clippy::disallowed_methods,
    reason = "集成测试从进程环境取被测二进制与 PATH"
)]
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use zcode_host_control::ClientMode;
use zcode_host_kit::{CallContext, Reply, RpcValue, ServiceChannel};
use zcode_host_services::agent::api::AgentApi;
use zcode_host_services::agent::notifications::AgentEvents;
use zcode_host_services::agent::service::AgentChannel;
use zcode_host_services::agent::sessions_index::{IndexEvent, SessionsIndexObserver};
use zcode_host_services::agent::{AgentRuntime, AgentRuntimeConfig};
use zcode_host_services::agent::{WorkspaceTarget, session};
use zcode_host_services::providers::ProviderRuntime;
use zcode_host_services::settings::SettingStore;

const MOCK_REPLY: &str = "Hello from the mock model";

/// runtime 的选择：缺省嵌入式（Host 的缺省链路）；`ZCODE_TEST_CLI_RUST_BINARY` 指向可执行文件时走子进程链路。
fn runtime_vars() -> BTreeMap<String, String> {
    match std::env::var_os("ZCODE_TEST_CLI_RUST_BINARY").map(PathBuf::from) {
        Some(cli) => {
            assert!(
                cli.is_file(),
                "ZCODE_TEST_CLI_RUST_BINARY is not a file: {}",
                cli.display()
            );
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
        Reply::Encoded(body) => zcode_host_rpc::Reader::new(&body)
            .value()
            .unwrap()
            .into_json(),
    }
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

fn builtin_providers() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../config/provider/zcode-builtin.json")
}

fn base_env(home: &Path) -> BTreeMap<String, String> {
    let home = home.to_string_lossy().into_owned();
    BTreeMap::from([
        ("HOME".to_owned(), home.clone()),
        ("USERPROFILE".to_owned(), home.clone()),
        ("ZCODE_DATA_BASE_DIR".to_owned(), home),
        ("PATH".to_owned(), std::env::var("PATH").unwrap_or_default()),
        ("NO_PROXY".to_owned(), "127.0.0.1,localhost".to_owned()),
    ])
}

fn create_session(workspace: &str, command: u32, text: &str) -> Value {
    json!({"workspacePath": workspace, "envelope": {
        "commandId": format!("0190a000-0000-7000-8000-{command:012}"),
        "clientId": "client-test",
        "sessionId": null,
        "type": "createSession",
        "payload": {
            "workspaceId": workspace,
            "firstInput": {
                "text": text,
                "modelSelection": {"providerId": "new-provider", "modelId": "my-model"},
                "mode": "build",
                "planEnabled": false,
            },
        },
        "issuedAt": 1,
    }})
}

#[tokio::test(flavor = "multi_thread")]
async fn core_chat_flow_against_the_runtime() {
    core_chat_flow(runtime_vars()).await;
}

/// 子进程链路，runtime 是合并二进制的 CLI 入口：以 `zcode-cli-rust` 为名运行它（Unix 用符号链接，
/// 其他平台用副本），Host 按 Rust CLI 的规则给它加 `--cwd` 与 `--surface desktop`。
#[tokio::test(flavor = "multi_thread")]
async fn core_chat_flow_over_the_child_process_link() {
    let bin = tempfile::tempdir().unwrap();
    let cli = bin
        .path()
        .join(format!("zcode-cli-rust{}", std::env::consts::EXE_SUFFIX));
    #[cfg(unix)]
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_zcode-host"), &cli).unwrap();
    #[cfg(not(unix))]
    std::fs::copy(env!("CARGO_BIN_EXE_zcode-host"), &cli).unwrap();
    core_chat_flow(BTreeMap::from([
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

async fn core_chat_flow(command_vars: BTreeMap<String, String>) {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let workspace_path = workspace.path().to_string_lossy().into_owned();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve_mock_model(listener));

    // 真实 workspace 的项目配置：一个 inline 插件与一个停用的 MCP server。控制面 lane 的
    // cwd 是 plugin-workspace，只能按请求里的 workspace 读到它们。
    let plugin_dir = home.path().join("plugins-src/host-demo");
    std::fs::create_dir_all(plugin_dir.join(".zcode-plugin")).unwrap();
    std::fs::write(
        plugin_dir.join(".zcode-plugin/plugin.json"),
        json!({"name": "host-demo", "version": "1.0.0", "description": "Seeded plugin"})
            .to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(workspace.path().join(".zcode")).unwrap();
    std::fs::write(
        workspace.path().join(".zcode/config.json"),
        json!({
            "plugins": {"dirs": [plugin_dir.to_string_lossy()]},
            "mcp": {"servers": {"seeded": {"type": "stdio", "command": "never-started", "enabled": false}}},
        })
        .to_string(),
    )
    .unwrap();

    let personal = provider_config(home.path(), port);
    let builtin = builtin_providers();
    let base_env = base_env(home.path());
    let providers = ProviderRuntime::new(Some(builtin), personal);
    let runtime = AgentRuntime::new(
        AgentRuntimeConfig {
            base_env,
            command_vars,
            fallback_cwd: None,
        },
        SettingStore::new(&zcode_host_services::testing::spawner(), home.path()),
        providers,
    );
    let events = AgentEvents::start(runtime.extensions(), None);
    let api = AgentApi::new(runtime.clone(), home.path(), events, None);
    let members = zcode_host_services::agent::members::agent_service(&api).build(None);
    let channel = AgentChannel::new(api.clone(), Some(members));
    let observer = SessionsIndexObserver::start(runtime.clone());
    let mut index_events = Box::pin(observer.events().subscribe());
    let context = CallContext {
        connection: 1,
        attachment_id: "test".into(),
        client_mode: ClientMode::DesktopContinuous,
        window_id: None,
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

    if std::env::var_os("ZCODE_TEST_DUMP_SESSION_READ").is_some() {
        let process = runtime
            .existing(&WorkspaceTarget::local(workspace_path.clone()))
            .await
            .unwrap();
        let snapshot = process
            .request_value(
                "session/read",
                &json!({"sessionId": session_id, "messageLimit": 20}),
            )
            .await
            .unwrap();
        // 测试体移出了 #[test] 函数，clippy 不再放行 eprintln!：直接写 stderr。
        use std::io::Write;
        let _ = writeln!(
            std::io::stderr().lock(),
            "SESSION_READ {}",
            serde_json::to_string_pretty(&snapshot).unwrap()
        );
    }

    // 进程已在运行：sessions-index（existing-only）现在可以订阅。
    let index = call(
        &channel,
        &context,
        "subscribeSessionsIndexV4",
        json!({"workspacePath": workspace_path, "runtimePolicy": "existing-only"}),
    )
    .await;
    assert!(index["ack"]["subscriptionId"].is_string());

    // ── agent 补齐（docs/specs/rust-host-agent.md §6）──
    let workspace_params = json!({"workspacePath": workspace_path});
    let config = call(
        &channel,
        &context,
        "subscribeWorkspaceConfigV4",
        json!({"workspacePath": workspace_path, "runtimePolicy": "existing-only"}),
    )
    .await;
    assert!(config["ack"]["subscriptionId"].is_string(), "{config}");
    let read = call(
        &channel,
        &context,
        "readSession",
        json!({"workspacePath": workspace_path, "sessionId": session_id}),
    )
    .await;
    assert_eq!(read["session"]["sessionId"], session_id.as_str());
    let listed = call(&channel, &context, "listSessions", workspace_params.clone()).await;
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|session| session["sessionId"] == session_id.as_str()),
        "{listed}"
    );
    let identity = call(
        &channel,
        &context,
        "getWorkspaceRuntimeIdentity",
        workspace_params.clone(),
    )
    .await;
    assert_eq!(identity["generation"], 1);
    let storage = call(
        &channel,
        &context,
        "getStorageStartupState",
        workspace_params.clone(),
    )
    .await;
    assert_eq!(storage["state"]["phase"], "ready", "{storage}");
    let skills = call(
        &channel,
        &context,
        "getSkillReferenceCatalog",
        workspace_params.clone(),
    )
    .await;
    assert!(skills.is_object(), "{skills}");
    let usage = call(
        &channel,
        &context,
        "getTaskTokenUsage",
        json!({"workspacePath": workspace_path, "sessionId": session_id}),
    )
    .await;
    assert!(usage.is_object(), "{usage}");
    // 插件与 MCP 走两个控制面 lane（工作目录在临时 home 下），真实 workspace 只作为请求参数，
    // 两个 lane 的 CLI 同时存活（同一 cwd，无工作区锁）。
    let plugins = call(
        &channel,
        &context,
        "listPlugins",
        json!({"workspacePath": workspace_path}),
    )
    .await;
    let seeded = plugins["plugins"]
        .as_array()
        .unwrap_or_else(|| panic!("{plugins}"))
        .iter()
        .find(|plugin| plugin["id"] == "host-demo@inline")
        .unwrap_or_else(|| panic!("seeded plugin missing: {plugins}"));
    assert_eq!(seeded["rootSource"], "workspace", "{seeded}");
    let mcp = call(
        &channel,
        &context,
        "listMcpServerStatuses",
        json!({"workspacePath": workspace_path}),
    )
    .await;
    assert_eq!(mcp["statuses"]["seeded"]["status"], "disabled", "{mcp}");
    // status 模式读同一 workspace 最近一次 connect 的结果。
    let status = call(
        &channel,
        &context,
        "listMcpServerStatuses",
        json!({"workspacePath": workspace_path, "mode": "status"}),
    )
    .await;
    assert_eq!(
        status["statuses"]["seeded"]["status"], "disabled",
        "{status}"
    );
    let plugin_workspace = home
        .path()
        .join(".zcode/plugin-workspace")
        .to_string_lossy()
        .into_owned();
    let lane_target = WorkspaceTarget::local(plugin_workspace);
    let plugin_process = api.plugin.existing(&lane_target).await.unwrap();
    assert!(plugin_process.runtime_identity().get("lane").is_none());
    let mcp_process = api.mcp.existing(&lane_target).await.unwrap();
    assert_eq!(mcp_process.runtime_identity()["lane"], "mcp-status");
    assert!(!plugin_process.client.is_closed());
    let children = call(
        &channel,
        &context,
        "collectLocalRuntimeChildProcesses",
        Value::Null,
    )
    .await;
    let children = children.as_array().unwrap();
    assert_eq!(children.len(), 3, "{children:?}");
    // chat 在真实 workspace；plugin-management（不带 lane 字段，同 Node）与 mcp-status 都在
    // plugin-workspace，两者同时存活。
    let at = |path: &str, lane: Option<&str>| {
        children.iter().any(|entry| {
            entry["workspacePath"] == path && entry.get("lane").and_then(Value::as_str) == lane
        })
    };
    assert!(at(&workspace_path, None), "{children:?}");
    assert!(at(&lane_target.workspace_path, None), "{children:?}");
    assert!(
        at(&lane_target.workspace_path, Some("mcp-status")),
        "{children:?}"
    );
    // legacy 会话：草稿创建后条件关闭。
    let draft = call(
        &channel,
        &context,
        "createSession",
        json!({"workspacePath": workspace_path, "persistence": "deferred"}),
    )
    .await;
    let draft_id = draft["session"]["sessionId"].as_str().unwrap().to_owned();
    let closed = call(
        &channel,
        &context,
        "closeSession",
        json!({"workspacePath": workspace_path, "sessionId": draft_id, "expectedPersistence": "deferred"}),
    )
    .await;
    assert_eq!(closed, json!(true));

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

/// 一个嵌入式 runtime 实例崩溃（调试构建的故障注入让它的 Engine 循环 panic）只影响它自己的
/// workspace：另一个 workspace 的实例原样存活；崩溃的 workspace 在下一次请求时重启，代次加一
/// （docs/specs/rust-u3-single-process.md §3）。
#[tokio::test(flavor = "multi_thread")]
async fn an_embedded_runtime_panic_only_restarts_its_own_workspace() {
    if std::env::var_os("ZCODE_TEST_CLI_RUST_BINARY").is_some() {
        // 子进程链路的崩溃由进程退出路径覆盖；故障注入只在进程内的调试构建里生效。
        return;
    }
    const FAULT: &str = "fault-marker-91be";
    let home = tempfile::tempdir().unwrap();
    let (first, second) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let path = |dir: &tempfile::TempDir| dir.path().to_string_lossy().into_owned();
    let (first_path, second_path) = (path(&first), path(&second));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve_mock_model(listener));
    let personal = provider_config(home.path(), port);
    let mut env = base_env(home.path());
    env.insert("ZCODE_RUNTIME_FAULT_MARKER".to_owned(), FAULT.to_owned());
    let runtime = AgentRuntime::new(
        AgentRuntimeConfig {
            base_env: env,
            command_vars: runtime_vars(),
            fallback_cwd: None,
        },
        SettingStore::new(&zcode_host_services::testing::spawner(), home.path()),
        ProviderRuntime::new(Some(builtin_providers()), personal),
    );
    let events = AgentEvents::start(runtime.extensions(), None);
    let api = AgentApi::new(runtime.clone(), home.path(), events, None);
    let members = zcode_host_services::agent::members::agent_service(&api).build(None);
    let channel = AgentChannel::new(api.clone(), Some(members));
    let context = CallContext {
        connection: 1,
        attachment_id: "test".into(),
        client_mode: ClientMode::DesktopContinuous,
        window_id: None,
    };
    call(&channel, &context, "helloConversationV4", Value::Null).await;
    call(
        &channel,
        &context,
        "initializeConversationV4",
        json!({"kind": "clientHello", "protocolVersion": 3, "clientId": "client-test", "clientKind": "desktop", "appVersion": "unknown"}),
    )
    .await;
    let send = |workspace: &str, command: u32, text: &str| {
        let arguments = args(create_session(workspace, command, text));
        let (channel, context) = (&channel, &context);
        async move {
            tokio::time::timeout(
                Duration::from_secs(60),
                channel.call(context, "sendConversationCommandV4", arguments),
            )
            .await
            .expect("sendConversationCommandV4 timed out")
        }
    };
    for (workspace, command) in [(&first_path, 1), (&second_path, 2)] {
        send(workspace, command, "say hello").await.unwrap();
    }
    let (first_target, second_target) = (
        WorkspaceTarget::local(first_path.clone()),
        WorkspaceTarget::local(second_path.clone()),
    );
    let before = runtime.existing(&second_target).await.unwrap();
    assert_eq!(runtime.existing(&first_target).await.unwrap().generation, 1);

    // 带标记的请求让第一个 workspace 的 Engine 循环 panic：这次调用失败，实例结束。
    send(&first_path, 3, FAULT).await.unwrap_err();
    tokio::time::timeout(Duration::from_secs(20), async {
        while runtime.existing(&first_target).await.is_some() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the panicked runtime was not released");

    // 另一个 workspace 的实例没有重启，仍能处理请求。
    send(&second_path, 4, "say hello again").await.unwrap();
    let after = runtime.existing(&second_target).await.unwrap();
    assert_eq!(after.generation, before.generation);
    assert!(std::sync::Arc::ptr_eq(&before, &after));

    // 崩溃的 workspace 在下一次请求时重启。
    send(&first_path, 5, "say hello after the crash")
        .await
        .unwrap();
    assert_eq!(runtime.existing(&first_target).await.unwrap().generation, 2);
    channel.connection_closed(&context);
    runtime.dispose_all().await;
}

/// 嵌入式 runtime 没有 stderr：实例自己结束时（启动失败、返回错误、Engine panic），结束原因只在
/// 任务的结果里，输出管道随之关闭只是结果。Host 日志必须记下 runtime 给出的原因
/// （docs/specs/rust-u3-single-process.md §2.2“结束原因”），不能只记 `stdout_closed`。
#[tokio::test(flavor = "multi_thread")]
async fn an_embedded_runtime_that_ends_on_its_own_logs_why() {
    if std::env::var_os("ZCODE_TEST_CLI_RUST_BINARY").is_some() {
        // 子进程链路的错误在 stderr 尾巴里，由同一行日志带出。
        return;
    }
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
    impl zcode_host_kit::log::LogSink for Capture {
        fn log(&self, _level: zcode_host_control::LogLevel, message: String) {
            self.0.lock().unwrap().push(message);
        }
    }
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    zcode_host_kit::log::set_sink(Box::new(Capture(lines.clone())));
    let home = tempfile::tempdir().unwrap();
    // workspace 目录不存在且没有回退目录：runtime 在准备阶段以错误结束。
    let missing = home.path().join("no-such-workspace");
    let runtime = AgentRuntime::new(
        AgentRuntimeConfig {
            base_env: base_env(home.path()),
            command_vars: runtime_vars(),
            fallback_cwd: None,
        },
        SettingStore::new(&zcode_host_services::testing::spawner(), home.path()),
        ProviderRuntime::new(
            Some(builtin_providers()),
            home.path().join("provider_config.json"),
        ),
    );
    let target = WorkspaceTarget::local(missing.to_string_lossy().into_owned());
    runtime
        .acquire(
            &target,
            zcode_host_services::agent::RuntimePolicy::StartIfNeeded,
        )
        .await
        .unwrap();
    let label = format!("agent[{}#1]", target.key());
    let closed = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let found = lines
                .lock()
                .unwrap()
                .iter()
                .find(|line| line.starts_with(&format!("{label} closed:")))
                .cloned();
            if let Some(line) = found {
                return line;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the runtime instance did not end");
    assert!(
        closed.contains("runtime_exit: Workspace unavailable"),
        "{closed}"
    );
    runtime.dispose_all().await;
}

/// web-remote-replayable（手机远控的恢复链路）在 runtime 之上同样成立：replayable 连接建会话、
/// 收到流式回复；断开后另一条 replayable 连接重新订阅，快照里有完整的回复（恢复靠 runtime 持有的
/// 会话事实，不靠连接）。与 `core_chat_flow_against_the_runtime` 的 desktop-continuous 互为对照
/// （docs/specs/rust-u3-single-process.md §3）。
#[tokio::test(flavor = "multi_thread")]
async fn a_replayable_attachment_streams_and_recovers_against_the_runtime() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let workspace_path = workspace.path().to_string_lossy().into_owned();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve_mock_model(listener));
    let personal = provider_config(home.path(), port);
    let runtime = AgentRuntime::new(
        AgentRuntimeConfig {
            base_env: base_env(home.path()),
            command_vars: runtime_vars(),
            fallback_cwd: None,
        },
        SettingStore::new(&zcode_host_services::testing::spawner(), home.path()),
        ProviderRuntime::new(Some(builtin_providers()), personal),
    );
    let events = AgentEvents::start(runtime.extensions(), None);
    let api = AgentApi::new(runtime.clone(), home.path(), events, None);
    let members = zcode_host_services::agent::members::agent_service(&api).build(None);
    let channel = AgentChannel::new(api.clone(), Some(members));
    let target = json!({"workspacePath": workspace_path});
    let connect = |connection: u64| {
        let channel = &channel;
        async move {
            let context = CallContext {
                connection,
                attachment_id: format!("remote-{connection}"),
                client_mode: ClientMode::WebRemoteReplayable,
                window_id: None,
            };
            let hello = call(channel, &context, "helloConversationV4", Value::Null).await;
            assert_eq!(hello["deliveryProfile"], "replayable");
            assert_eq!(hello["capabilities"]["localTerminal"], false);
            call(
                channel,
                &context,
                "initializeConversationV4",
                json!({"kind": "clientHello", "protocolVersion": 3, "clientId": "client-test", "clientKind": "mobileRemote", "appVersion": "unknown"}),
            )
            .await;
            context
        }
    };
    // 订阅一个会话，等到快照或增量里出现 mock 的回复。
    let read_reply = |context: CallContext, session_id: String| {
        let (channel, target, workspace_path) = (&channel, target.clone(), workspace_path.clone());
        async move {
            let mut frames = channel
                .listen(
                    &context,
                    "onDynamicConversationFrame",
                    RpcValue::Json(target),
                )
                .unwrap();
            let subscribed = call(
                channel,
                &context,
                "subscribeConversationV4",
                json!({"workspacePath": workspace_path, "sessionId": session_id}),
            )
            .await;
            let subscription = subscribed["ack"]["subscriptionId"]
                .as_str()
                .unwrap()
                .to_owned();
            tokio::time::timeout(Duration::from_secs(60), async {
                while let Some(Reply::RawJson(raw)) = frames.next().await {
                    let frame: Value = serde_json::from_str(&raw).unwrap();
                    assert_eq!(frame["subscriptionId"], subscription.as_str());
                    if raw.contains(MOCK_REPLY) {
                        return;
                    }
                }
                panic!("the frame stream ended");
            })
            .await
            .expect("the reply never reached the replayable connection");
            context
        }
    };

    let first = connect(1).await;
    let ack = tokio::time::timeout(
        Duration::from_secs(60),
        call(
            &channel,
            &first,
            "sendConversationCommandV4",
            create_session(&workspace_path, 1, "say hello"),
        ),
    )
    .await
    .expect("createSession timed out");
    assert_eq!(ack["status"], "accepted", "ack: {ack}");
    let session_id = ack["result"]["sessionId"].as_str().unwrap().to_owned();
    let first = read_reply(first, session_id.clone()).await;
    let before = runtime
        .existing(&WorkspaceTarget::local(workspace_path.clone()))
        .await
        .unwrap();

    // 手机断线重连：新的 replayable 连接从快照恢复，runtime 实例不变。
    channel.connection_closed(&first);
    let second = read_reply(connect(2).await, session_id).await;
    let after = runtime
        .existing(&WorkspaceTarget::local(workspace_path.clone()))
        .await
        .unwrap();
    assert!(std::sync::Arc::ptr_eq(&before, &after));
    channel.connection_closed(&second);
    runtime.dispose_all().await;
}
