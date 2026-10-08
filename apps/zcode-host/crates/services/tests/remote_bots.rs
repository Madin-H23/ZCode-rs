//! Bot 远端运行时端到端（docs/specs/rust-host-remote.md §2.9）：拉起本仓库构建的真实
//! `packages/server/dist/remote/zcode-server.cjs`（临时 HOME，远端 Agent 是本仓库的 Rust CLI），经控制
//! 消息连上远端 workspace 并做 Provider Provisioning；本地 Bots 的 Telegram 长轮询从 127.0.0.1 上的
//! Bot API mock 收到一条消息，上下文是这个远端 workspace：Bot 经 `RemoteBotRuntime` 问“Main”连接状态，
//! 在远端建会话、订阅远端 V4 会话帧、经 zcode-task wrapper 发送（带 realtime lease 与镜像），远端 CLI
//! 调 127.0.0.1 上的 mock 模型，流式回复回到 Telegram mock 的 sendMessage。
//!
//! 所有出站 HTTP 只到 127.0.0.1（其余指向已关闭的端口），HOME、数据目录与凭据都在临时目录。
//! 需要：node、`pnpm --filter @zcode/server build:remote` 的产物、Rust CLI（`ZCODE_TEST_CLI_RUST_BINARY`
//! 或工作区内的 release 构建）；缺任一项时跳过并说明原因。
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use futures_util::{FutureExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use zcode_host_control::{ControlRouter, ControlSink, HostToMain};
use zcode_host_services::account::credential::CredentialStore;
use zcode_host_services::agent::WorkspaceTarget;
use zcode_host_services::automation::driver::{SendText, TerminalOutcome};
use zcode_host_services::bots::providers::{Endpoints, Providers};
use zcode_host_services::bots::service::{Bots, BotsDeps};
use zcode_host_services::bots::store::BotsRepo;
use zcode_host_services::bots::{FinalReply, TaskPort};
use zcode_host_services::main_bridge::MainBridge;
use zcode_host_services::remote::provisioning::ProvisioningSource;
use zcode_host_services::remote::transport::ProcessTransport;
use zcode_host_services::remote::{RemoteConfig, install_with};
use zcode_host_services::settings::SettingStore;
use zcode_host_services::tasks::realtime::TaskRealtimeBridge;

const REPLY: &str = "Hello from the remote mock model";
const PROMPT: &str = "hello remote agent";

struct Control(mpsc::UnboundedSender<Value>);

impl ControlSink for Control {
    fn send(&self, message: HostToMain) {
        let _ = self.0.send(serde_json::to_value(&message).unwrap());
    }
}

fn find_node() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("node"))
            .find(|candidate| candidate.is_file())
    })
}

fn server_bundle() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../packages/server/dist/remote/zcode-server.cjs")
}

fn cli_binary() -> Option<PathBuf> {
    let path = std::env::var_os("ZCODE_TEST_CLI_RUST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../../zcode-cli-rust/target/release/zcode-cli-rust")
        });
    path.is_file().then_some(path)
}

/// 一个 HTTP 请求：方法、路径与 JSON body。
#[derive(Debug, Clone)]
struct Request {
    path: String,
    body: Value,
}

async fn read_request(stream: &mut TcpStream) -> Option<(String, String, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let body_start = loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..body_start]).to_string();
    let length = head
        .to_lowercase()
        .lines()
        .find_map(|line| line.strip_prefix("content-length:").map(str::to_owned))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while buffer.len() < body_start + length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let mut line = head.lines().next().unwrap_or_default().split(' ');
    let method = line.next().unwrap_or_default().to_owned();
    let path = line
        .next()
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default()
        .to_owned();
    Some((method, path, buffer[body_start..].to_vec()))
}

async fn write_response(stream: &mut TcpStream, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn chunk(delta: Value, finish: Value) -> Value {
    json!({
        "id": "mock", "object": "chat.completion.chunk", "created": 0, "model": "my-model",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    })
}

/// OpenAI 兼容的流式模型：回复分两段文本。
async fn serve_model(listener: TcpListener, hits: Arc<Mutex<usize>>) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let hits = hits.clone();
        tokio::spawn(async move {
            if read_request(&mut stream).await.is_none() {
                return;
            }
            *hits.lock().unwrap() += 1;
            let mut stop = chunk(json!({}), json!("stop"));
            stop["usage"] = json!({"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2});
            let (head, tail) = REPLY.split_at(6);
            let mut body = String::new();
            for event in [
                chunk(json!({"role": "assistant", "content": head}), Value::Null),
                chunk(json!({"content": tail}), Value::Null),
                stop,
            ] {
                body.push_str(&format!("data: {event}\n\n"));
            }
            body.push_str("data: [DONE]\n\n");
            write_response(&mut stream, "text/event-stream", &body).await;
        });
    }
}

/// Telegram Bot API mock：getUpdates 先投递一条消息（offset 提交后不再投递），其余成员应答成功。
async fn serve_telegram(listener: TcpListener, requests: Arc<Mutex<Vec<Request>>>) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let requests = requests.clone();
        tokio::spawn(async move {
            let Some((_, path, body)) = read_request(&mut stream).await else {
                return;
            };
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            requests.lock().unwrap().push(Request {
                path: path.clone(),
                body: body.clone(),
            });
            let method = path.rsplit('/').next().unwrap_or_default();
            let answer = match method {
                "getMe" => {
                    json!({"ok": true, "result": {"first_name": "TG Bot", "username": "tgbot"}})
                }
                "getUpdates" => {
                    if body["offset"].as_i64().unwrap_or(0) <= 10 {
                        json!({"ok": true, "result": [{"update_id": 10, "message": {
                            "message_id": 10, "text": PROMPT,
                            "chat": {"id": 42, "type": "private"}, "from": {"id": 42, "first_name": "Ann"}}}]})
                    } else {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        json!({"ok": true, "result": []})
                    }
                }
                "sendMessage" => {
                    json!({"ok": true, "result": {"message_id": 100, "chat": {"id": 42}}})
                }
                _ => json!({"ok": true, "result": true}),
            };
            write_response(&mut stream, "application/json", &answer.to_string()).await;
        });
    }
}

/// 本地 workspace 的任务能力：这个测试里的上下文都是远端，走到这里就是路由错误。
struct LocalUnreachable;

fn local_error<T: Send + 'static>() -> BoxFuture<'static, Result<T, String>> {
    async { Err("local task port must not be used for a remote workspace".to_owned()) }.boxed()
}

impl TaskPort for LocalUnreachable {
    fn create(
        &self,
        _: WorkspaceTarget,
        _: Value,
        _: Option<String>,
    ) -> BoxFuture<'static, Result<String, String>> {
        local_error()
    }
    fn resume(
        &self,
        _: WorkspaceTarget,
        _: String,
    ) -> BoxFuture<'static, Result<Option<Value>, String>> {
        local_error()
    }
    fn send(
        &self,
        _: WorkspaceTarget,
        _: String,
        _: SendText,
    ) -> BoxFuture<'static, Result<(), String>> {
        local_error()
    }
    fn send_with_attachments(
        &self,
        _: WorkspaceTarget,
        _: String,
        _: SendText,
        _: Vec<Value>,
    ) -> BoxFuture<'static, Result<(), String>> {
        local_error()
    }
    fn stop(&self, _: WorkspaceTarget, _: String) -> BoxFuture<'static, Result<(), String>> {
        local_error()
    }
    fn list(&self, _: String) -> BoxFuture<'static, Vec<Value>> {
        async { Vec::new() }.boxed()
    }
    fn deleted_ids(&self, _: String) -> BoxFuture<'static, Vec<String>> {
        async { Vec::new() }.boxed()
    }
    fn terminal_outcomes(&self) -> BoxStream<'static, TerminalOutcome> {
        futures_util::stream::empty().boxed()
    }
    fn final_reply(&self, _: WorkspaceTarget, _: String) -> BoxFuture<'static, FinalReply> {
        async { FinalReply::default() }.boxed()
    }
    fn model_view(&self, _: Option<Value>) -> BoxFuture<'static, Value> {
        async { Value::Null }.boxed()
    }
    fn read_session(
        &self,
        _: WorkspaceTarget,
        _: String,
    ) -> BoxFuture<'static, Result<Value, String>> {
        local_error()
    }
    fn config_options(
        &self,
        _: WorkspaceTarget,
        _: String,
    ) -> BoxFuture<'static, Result<Value, String>> {
        local_error()
    }
    fn set_model(
        &self,
        _: WorkspaceTarget,
        _: String,
        _: Value,
    ) -> BoxFuture<'static, Result<(), String>> {
        local_error()
    }
    fn set_thought_level(
        &self,
        _: WorkspaceTarget,
        _: String,
        _: String,
    ) -> BoxFuture<'static, Result<(), String>> {
        local_error()
    }
    fn resolve_interaction(
        &self,
        _: WorkspaceTarget,
        _: String,
        _: String,
        _: Value,
    ) -> BoxFuture<'static, Result<(), String>> {
        local_error()
    }
    fn subscribe(
        &self,
        _: WorkspaceTarget,
        _: String,
    ) -> BoxFuture<'static, Result<BoxStream<'static, Value>, String>> {
        local_error()
    }
}

async fn wait_for<T>(what: &str, limit: Duration, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn telegram_message_runs_on_a_remote_workspace_agent() {
    let bundle = server_bundle();
    let (Some(node), true, Some(cli)) = (find_node(), bundle.is_file(), cli_binary()) else {
        eprintln!(
            "skipping: needs node on PATH, {} (pnpm --filter @zcode/server build:remote) and the Rust CLI release binary",
            bundle.display()
        );
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let remote_home = temp.path().join("remote-home");
    let local_home = temp.path().join("local-home");
    let local_config = local_home.join(".zcode/v2");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&remote_home).unwrap();
    std::fs::create_dir_all(&local_config).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    let workspace_path = workspace.canonicalize().unwrap().display().to_string();
    let identity = format!("remote:docker:local-test:{workspace_path}");

    let model_hits = Arc::new(Mutex::new(0));
    let model = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let model_port = model.local_addr().unwrap().port();
    tokio::spawn(serve_model(model, model_hits.clone()));
    let telegram_requests = Arc::new(Mutex::new(Vec::<Request>::new()));
    let telegram = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let telegram_origin = format!("http://127.0.0.1:{}", telegram.local_addr().unwrap().port());
    tokio::spawn(serve_telegram(telegram, telegram_requests.clone()));

    // 本机：个人 Provider（mock 模型，经 Provisioning 同步到远端）与远端 workspace 的连接记录。
    std::fs::write(
        local_config.join("provider_config.json"),
        json!({"schemaVersion": 1, "config": {
            "providerOrder": ["new-provider"],
            "providerConfigRules": {"providerRules": [{
                "providerId": "new-provider", "providerName": "Mock",
                "config": {
                    "group": "standard-personal",
                    "access": {"type": "api-key", "apiKey": "sk-test"},
                    "api": {"type": "openai-chat-completions", "baseUrl": format!("http://127.0.0.1:{model_port}/v1")},
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
    std::fs::write(
        local_config.join("setting.json"),
        json!({"lastWorkspaceSession": [{"kind": "remote", "workspacePath": workspace_path,
            "workspaceIdentity": identity, "target": {"kind": "docker", "container": "local-test"},
            "lastOpenedAt": 1, "lastConnectionStatus": "connected"}]})
        .to_string(),
    )
    .unwrap();

    let dead = "http://127.0.0.1:9";
    let env: BTreeMap<String, String> = [
        ("HOME", remote_home.display().to_string()),
        (
            "PATH",
            format!("/usr/bin:/bin:{}", node.parent().unwrap().display()),
        ),
        ("NODE_USE_ENV_PROXY", "1".into()),
        ("HTTP_PROXY", dead.into()),
        ("HTTPS_PROXY", dead.into()),
        ("NO_PROXY", "127.0.0.1,localhost".into()),
        ("no_proxy", "127.0.0.1,localhost".into()),
        ("ZCODE_BASE_URL", dead.into()),
        ("ZCODE_ENDPOINT_ORIGIN", dead.into()),
        ("ZAI_OAUTH_ORIGIN", dead.into()),
        ("ZAI_BUSINESS_BASE_URL", dead.into()),
        ("ZCODE_TELEMETRY_REPORT_ENDPOINT", String::new()),
        (
            "ZCODE_SERVICE_AUTHORITY_MODE",
            "desktop-attached-remote".into(),
        ),
        (
            "ZCODE_SERVER_RUNTIME_ROOT",
            remote_home.join(".zcode/server").display().to_string(),
        ),
        ("ZCODE_AGENT_SERVER_COMMAND", cli.display().to_string()),
        ("ZCODE_AGENT_SERVER_RUNTIME", "zcode-cli-rust".into()),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value))
    .collect();

    // “Main”：回答 Bot 的连接状态查询与 realtime lease，其余消息记下来。
    let (sink, mut messages) = mpsc::unbounded_channel();
    let control: Arc<dyn ControlSink> = Arc::new(Control(sink));
    let mut router = ControlRouter::default();
    let realtime =
        TaskRealtimeBridge::for_host_init(Some("host-1"), None, MainBridge::new(control.clone()))
            .unwrap();
    realtime.register(&mut router);
    let credentials = CredentialStore::new(&local_config, "test-secret");
    let settings = SettingStore::new(&local_home);
    let remote = install_with(
        RemoteConfig {
            transport: Arc::new(ProcessTransport {
                program: node,
                args: vec![bundle.display().to_string()],
                env,
                cwd: remote_home.clone(),
            }),
            settings: settings.clone(),
            control,
            provisioning: ProvisioningSource {
                personal_config_file: local_config.join("provider_config.json"),
                credentials: credentials.clone(),
            },
            media_range_enabled: true,
            wsl_idle_ttl: Duration::from_secs(60),
            realtime: Some(realtime),
        },
        &mut router,
    );
    let router = Arc::new(router);
    let log = Arc::new(Mutex::new(Vec::<Value>::new()));
    {
        let (router, log) = (router.clone(), log.clone());
        tokio::spawn(async move {
            while let Some(message) = messages.recv().await {
                match message["type"].as_str() {
                    Some("bot-remote-workspace-connection-status-request") => {
                        let connected = log
                            .lock()
                            .unwrap()
                            .iter()
                            .any(|logged| logged["type"] == "remote-workspace-connected");
                        router.dispatch(
                            "bot-remote-workspace-connection-status-result",
                            json!({"type": "bot-remote-workspace-connection-status-result",
                                   "requestId": message["requestId"], "ok": true, "connected": connected}),
                        );
                    }
                    Some("task-run-lease-acquire") => {
                        router.dispatch(
                            "task-run-lease-result",
                            json!({"type": "task-run-lease-result", "result": {
                                "leaseRequestId": message["request"]["leaseRequestId"], "acquired": true}}),
                        );
                    }
                    _ => {}
                }
                log.lock().unwrap().push(message);
            }
        });
    }
    let logged = |kind: &str| -> Vec<Value> {
        log.lock()
            .unwrap()
            .iter()
            .filter(|message| message["type"] == kind)
            .cloned()
            .collect()
    };

    // 连上远端 workspace，并做 environment-online 的 Provisioning（远端拿到 mock 模型）。
    assert!(router.dispatch(
        "connect-remote-workspace",
        json!({"type": "connect-remote-workspace", "requestId": "r1",
               "target": {"kind": "docker", "container": "local-test"},
               "remoteAssets": {}, "workspacePath": workspace_path}),
    ));
    let connected = wait_for("remote connected", Duration::from_secs(30), || {
        logged("remote-workspace-connected").pop()
    })
    .await;
    assert_eq!(connected["descriptor"]["workspaceIdentity"], identity);
    let session_id = connected["descriptor"]["remoteSessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(router.dispatch(
        "provider-provisioning-execute",
        json!({"type": "provider-provisioning-execute", "requestId": "p1",
               "environmentKey": "docker:local-test", "remoteSessionId": session_id,
               "trigger": "environment-online"}),
    ));
    let provisioned = wait_for("provisioning", Duration::from_secs(30), || {
        logged("provider-provisioning-execution-result").pop()
    })
    .await;
    assert!(
        matches!(
            provisioned["status"].as_str(),
            Some("applied" | "already-applied")
        ),
        "{provisioned}"
    );

    // 本地 Bots（桌面 Host 里的那份）：远端运行时来自远端区域，Telegram 指向 mock。
    let bots = Bots::new(BotsDeps {
        repo: BotsRepo::new(local_config.clone()),
        credentials,
        providers: Arc::new(Providers::new(Endpoints::local(&telegram_origin)).unwrap()),
        settings,
        tasks: Arc::new(LocalUnreachable),
        remote: remote.bots_runtime(),
        attachments_dir: local_config.join("bot-attachments"),
        background: true,
    });
    remote.bind_bots(bots.clone());
    bots.save_bot(&json!({
        "bot": {"id": "tg-1", "name": "tg", "provider": "telegram", "enabled": true,
                "providerUserId": "42", "allowedWorkspaces": ["*"],
                "allowedCommands": {"status": true, "new": true, "workspace": true, "model": true,
                                    "thoughtLevel": true, "reply": true},
                "currentOptions": {}},
        "credentialValue": "tg-token",
    }))
    .await
    .unwrap();

    // Telegram 投递的消息落到远端 Agent，流式回复回到 Telegram。
    let sent = |requests: &Arc<Mutex<Vec<Request>>>| -> Vec<String> {
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.path.ends_with("/sendMessage"))
            .map(|request| request.body["text"].as_str().unwrap_or_default().to_owned())
            .collect()
    };
    let reply = wait_for(
        "bot reply from the remote agent",
        Duration::from_secs(120),
        || {
            sent(&telegram_requests)
                .into_iter()
                .find(|text| text.contains(REPLY))
        },
    )
    .await;
    assert!(reply.contains(REPLY), "{reply}");
    assert!(
        *model_hits.lock().unwrap() >= 1,
        "the remote CLI called the mock model"
    );
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(local_config.join("bot-state.v3.json")).unwrap(),
    )
    .unwrap();
    let context = &state["bots"]["tg-1"];
    assert_eq!(context["workspaceIdentity"], identity, "{state}");
    assert_eq!(context["mode"], "task");
    let task_id = context["activeTaskId"].as_str().unwrap().to_owned();

    // 远端 sendPrompt 期间持有 realtime lease，并把用户消息镜像给 Main。
    let lease = wait_for("lease release", Duration::from_secs(10), || {
        logged("task-run-lease-release").pop()
    })
    .await;
    assert_eq!(lease["target"]["taskId"], task_id);
    assert_eq!(lease["target"]["workspaceKey"], identity);
    let user_message = logged("task-stream-op-publish")
        .into_iter()
        .find(|message| message["op"]["kind"] == "user_message")
        .expect("user message mirrored");
    assert_eq!(user_message["op"]["content"], PROMPT);
    assert!(
        logged("session-route-announce")
            .iter()
            .any(|message| message["route"]["sessionId"] == task_id.as_str())
    );

    bots.shutdown().await;
    remote.shutdown().await;
}
