//! bots 测试：对拍 fixture（TS 生成）+ 127.0.0.1 上的飞书 mock（开放平台 HTTP 与长连接 WS）。
//! 不连任何真实飞书服务，不读写真实 HOME。
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use futures_util::{FutureExt, SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use zcode_host_kit::EventHub;

use super::credentials::{CredentialCipher, CredentialStore};
use super::feishu::api::{Endpoints, FeishuApi};
use super::feishu::frame::{Frame, METHOD_CONTROL, METHOD_DATA};
use super::feishu::{card_markdown, parse_callback, split_text, strip_mentions};
use super::model::{
    import_legacy_config, import_legacy_state, message, normalize_allowed_workspaces,
    normalize_bot, parse_command, parse_config_file, parse_state_file,
};
use super::service::{Bots, BotsDeps};
use super::store::BotsRepo;
use super::{FinalReply, TaskPort};
use crate::agent::WorkspaceTarget;
use crate::automation::driver::{SendText, TerminalOutcome};
use crate::off_peak::gateway::{read_request, write_json};
use crate::settings::SettingStore;

fn parity() -> Value {
    serde_json::from_str(include_str!("fixtures/parity.json")).unwrap()
}

#[test]
fn command_parser_matches_ts() {
    for case in parity()["commands"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        assert_eq!(parse_command(text), case["command"], "{text:?}");
    }
}

#[test]
fn normalization_and_legacy_import_match_ts() {
    let fixture = parity();
    for case in fixture["normalize"].as_array().unwrap() {
        assert_eq!(
            normalize_bot(&case["input"]),
            case["output"],
            "{}",
            case["input"]
        );
    }
    for case in fixture["allowedWorkspaces"].as_array().unwrap() {
        let input = super::model::string_list(&case["input"]);
        assert_eq!(json!(normalize_allowed_workspaces(&input)), case["output"]);
    }
    for case in fixture["legacyConfig"].as_array().unwrap() {
        let parsed = parse_config_file(&import_legacy_config(&case["input"])).unwrap();
        assert_eq!(parsed, case["output"]);
        // 输出键序与 zod 相同（schema 顺序）。
        assert_eq!(parsed.to_string(), case["output"].to_string());
    }
    for case in fixture["legacyState"].as_array().unwrap() {
        let parsed = parse_state_file(&import_legacy_state(&case["input"])).unwrap();
        assert_eq!(parsed.to_string(), case["output"].to_string());
    }
    let strict = json!({"version": 3, "bots": [{"id": "a", "name": "", "provider": "feishu", "enabled": true,
        "allowedWorkspaces": ["*"], "allowedCommands": {"status": true, "new": true, "workspace": true,
        "model": true, "thoughtLevel": true, "reply": true}, "currentOptions": {}, "replyMode": "streaming_card",
        "extra": 1}]});
    assert!(
        parse_config_file(&strict)
            .unwrap_err()
            .contains("unrecognized key")
    );
}

#[test]
fn messages_come_from_the_ts_table() {
    assert_eq!(
        message(None, "unknownCommand", &[("command", "x")]),
        "未知命令：**/x**"
    );
    assert_eq!(
        message(Some("en-US"), "taskFailed", &[("message", "boom")]),
        "Task failed: boom"
    );
    assert_eq!(message(Some("ja-JP"), "received", &[]), "已收到。");
}

#[test]
fn frames_encode_like_protobufjs() {
    let ping = Frame {
        service: 7,
        method: METHOD_CONTROL,
        headers: vec![("type".into(), "ping".into())],
        ..Frame::default()
    };
    let mut expected = vec![
        0x08, 0x00, 0x10, 0x00, 0x18, 0x07, 0x20, 0x00, 0x2a, 0x0c, 0x0a, 0x04,
    ];
    expected.extend_from_slice(b"type");
    expected.extend_from_slice(&[0x12, 0x04]);
    expected.extend_from_slice(b"ping");
    assert_eq!(ping.encode(), expected);
    let data = Frame {
        seq_id: 300,
        log_id: u64::MAX,
        service: -1,
        method: METHOD_DATA,
        headers: vec![("type".into(), "event".into()), ("sum".into(), "1".into())],
        payload: Some(b"{}".to_vec()),
        log_id_new: Some("n".into()),
        ..Frame::default()
    };
    assert_eq!(Frame::decode(&data.encode()).unwrap(), data);
}

#[test]
fn feishu_messages_parse_like_the_provider() {
    let payload = json!({
        "botId": "b1", "zcodeProvider": "lark",
        "sender": {"sender_id": {"open_id": "ou_1"}},
        "message": {"message_id": "om_1", "chat_id": "oc_1", "chat_type": "group", "message_type": "post",
            "content": json!({"zh_cn": {"title": "T", "content": [[{"tag": "text", "text": "hi "},
                {"tag": "at", "user_id": "x"}, {"tag": "a", "text": "link", "href": "https://e.x"}]]}}).to_string()},
    });
    let inbound = parse_callback(&payload).unwrap();
    assert_eq!(inbound.text, "T\nhi link https://e.x");
    assert_eq!(inbound.actor.provider, "lark");
    assert_eq!(inbound.actor.chat_type, "group");
    assert_eq!(inbound.actor.reply_target(), "oc_1");
    let card = json!({"botId": "b1", "operator": {"open_id": "ou_2"},
        "action": {"value": {"command": "/status"}}, "context": {"open_chat_id": "oc_9"}, "event_id": "ev"});
    let inbound = parse_callback(&card).unwrap();
    assert_eq!(
        (inbound.text.as_str(), inbound.actor.chat_type.as_str()),
        ("/status", "private")
    );
    assert_eq!(inbound.actor.provider_message_id.as_deref(), Some("ev"));
    assert_eq!(strip_mentions("<at user_id=\"1\">Bot</at> hi @all"), "hi");
    assert_eq!(
        card_markdown("a\nb\n\n```\nx\ny\n```\n---\nc"),
        "a  \nb\n\n```\nx\ny\n```\n\n---\n\nc"
    );
    assert_eq!(split_text(&"字".repeat(1901)).len(), 2);
}

#[tokio::test]
async fn credentials_interoperate_with_node() {
    let fixture = &parity()["credential"];
    let cipher = CredentialCipher::new(fixture["secret"].as_str(), std::path::Path::new("/unused"));
    let encrypted = fixture["encrypted"].as_str().unwrap();
    assert_eq!(cipher.decrypt(encrypted).unwrap(), fixture["plain"]);
    let again = cipher.encrypt("v").unwrap();
    assert!(again.starts_with("enc:v1:"));
    assert_eq!(cipher.decrypt(&again).unwrap(), "v");
    assert_eq!(cipher.decrypt("plain-legacy").unwrap(), "plain-legacy");
    assert!(
        cipher
            .decrypt("enc:v1:a.b")
            .unwrap_err()
            .to_string()
            .contains("密文格式非法")
    );

    let dir = tempfile::tempdir().unwrap();
    let store = CredentialStore::new(dir.path(), CredentialCipher::new(Some("s"), dir.path()));
    store.save("bot:a:credential", "secret-1").await.unwrap();
    store.save("other", "x").await.unwrap();
    assert_eq!(
        store.load("bot:a:credential").await.unwrap().as_deref(),
        Some("secret-1")
    );
    store.delete("bot:a:credential").await.unwrap();
    assert_eq!(store.load("bot:a:credential").await.unwrap(), None);
    let file = dir.path().join("credentials.json");
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert!(saved["other"].as_str().unwrap().starts_with("enc:v1:"));
    std::fs::write(&file, "{broken").unwrap();
    assert!(
        store
            .load("other")
            .await
            .unwrap_err()
            .to_string()
            .contains("corrupt")
    );
    assert!(std::fs::read_dir(dir.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".corrupt-")
    }));
}

#[tokio::test]
async fn repo_imports_legacy_files_once() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = &parity()["legacyConfig"][0];
    std::fs::write(
        dir.path().join("bot-config.json"),
        legacy["input"].to_string(),
    )
    .unwrap();
    let repo = BotsRepo::new(dir.path().to_path_buf());
    assert_eq!(repo.read_config().await.unwrap(), legacy["output"]);
    let written = std::fs::read_to_string(dir.path().join("bot-config.v3.json")).unwrap();
    assert_eq!(
        written,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&legacy["output"]).unwrap()
        )
    );
    let state = repo.read_state().await.unwrap();
    assert_eq!(state, json!({"version": 3, "bots": {}}));
    assert!(dir.path().join("bot-state.v3.json").exists());
}

#[tokio::test]
async fn runtime_lock_is_exclusive_until_released() {
    let dir = tempfile::tempdir().unwrap();
    let first = super::runtime_lock::acquire(dir.path(), "feishu-websocket", "cli_x", "a")
        .await
        .unwrap()
        .unwrap();
    assert!(
        super::runtime_lock::acquire(dir.path(), "feishu-websocket", "cli_x", "b")
            .await
            .unwrap()
            .is_none()
    );
    first.release().await;
    assert!(
        super::runtime_lock::acquire(dir.path(), "feishu-websocket", "cli_x", "b")
            .await
            .unwrap()
            .is_some()
    );
}

// ── 飞书 mock ──

#[derive(Default)]
struct Mock {
    /// 发给飞书的消息（im/v1/messages 的请求体）。
    sent: Mutex<Vec<Value>>,
    ws_url: Mutex<String>,
    poll_count: Mutex<u32>,
}

impl Mock {
    fn cards(&self) -> Vec<String> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .map(|body| {
                let card: Value = serde_json::from_str(body["content"].as_str().unwrap()).unwrap();
                card["body"]["elements"][0]["content"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }
}

async fn mock_open(mock: Arc<Mock>) -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                continue;
            };
            let mock = mock.clone();
            tokio::spawn(async move {
                let Ok(Some(request)) = read_request(&mut stream).await else {
                    return;
                };
                let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
                let form = String::from_utf8_lossy(&request.body).to_string();
                let path = request
                    .path
                    .split('?')
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                let response = match path.as_str() {
                    "/open-apis/auth/v3/tenant_access_token/internal" => {
                        if body["app_secret"] == "app-secret" {
                            json!({"code": 0, "tenant_access_token": "t-1", "expire": 7200})
                        } else {
                            json!({"code": 10014, "msg": "app secret invalid"})
                        }
                    }
                    "/open-apis/application/v6/applications/cli_0123456789abcdef" => {
                        json!({"code": 0, "data": {"app": {"app_name": " Mock Bot "}}})
                    }
                    "/open-apis/im/v1/messages" => {
                        assert_eq!(request.headers["authorization"], "Bearer t-1");
                        mock.sent.lock().unwrap().push(body);
                        json!({"code": 0, "data": {"message_id": "om_out"}})
                    }
                    "/callback/ws/endpoint" => {
                        let url = mock.ws_url.lock().unwrap().clone();
                        json!({"code": 0, "data": {"URL": url, "ClientConfig": {
                            "PingInterval": 120, "ReconnectCount": 1, "ReconnectInterval": 1, "ReconnectNonce": 0}}})
                    }
                    "/oauth/v1/app/registration" if form.contains("action=init") => {
                        json!({"supported_auth_methods": ["client_secret"]})
                    }
                    "/oauth/v1/app/registration" if form.contains("action=begin") => json!({
                        "device_code": "dev-1", "user_code": "U-1", "interval": 3, "expire_in": 60,
                        "verification_uri_complete": "https://accounts.example/verify?code=1&from=web",
                    }),
                    "/oauth/v1/app/registration" => {
                        let mut count = mock.poll_count.lock().unwrap();
                        *count += 1;
                        match *count {
                            1 => json!({"error": "authorization_pending"}),
                            2 => json!({"error": "slow_down"}),
                            _ => json!({"client_id": "cli_0123456789abcdef", "client_secret": "s",
                                "app": {"name": "Scanned"}, "user_info": {"open_id": "ou_owner"}}),
                        }
                    }
                    _ => json!({"code": 404}),
                };
                let _ = write_json(&mut stream, 200, &response, &[]).await;
            });
        }
    });
    origin
}

/// 一次长连接：把测试发来的帧推给客户端，并把客户端回的帧转给测试。
async fn mock_ws() -> (
    String,
    mpsc::UnboundedSender<Frame>,
    mpsc::UnboundedReceiver<Frame>,
) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let url = format!(
        "ws://127.0.0.1:{}/ws?device_id=d1&service_id=7",
        listener.local_addr().unwrap().port()
    );
    let (to_client, mut outgoing) = mpsc::unbounded_channel::<Frame>();
    let (from_client_tx, from_client) = mpsc::unbounded_channel::<Frame>();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        loop {
            tokio::select! {
                frame = outgoing.recv() => {
                    let Some(frame) = frame else { break };
                    socket.send(Message::Binary(frame.encode().into())).await.unwrap();
                }
                message = socket.next() => match message {
                    Some(Ok(Message::Binary(bytes))) => {
                        let _ = from_client_tx.send(Frame::decode(&bytes).unwrap());
                    }
                    Some(Ok(_)) => {}
                    _ => break,
                }
            }
        }
    });
    (url, to_client, from_client)
}

fn event_frame(message_id: &str, text: &str) -> Frame {
    let payload = json!({
        "schema": "2.0",
        "header": {"event_id": format!("ev-{message_id}"), "event_type": "im.message.receive_v1"},
        "event": {
            "sender": {"sender_id": {"open_id": "ou_user"}},
            "message": {"message_id": message_id, "chat_id": "oc_chat", "chat_type": "p2p",
                "message_type": "text", "content": json!({"text": text}).to_string()},
        },
    });
    Frame {
        service: 7,
        method: METHOD_DATA,
        headers: vec![
            ("type".into(), "event".into()),
            ("message_id".into(), message_id.into()),
            ("sum".into(), "1".into()),
            ("seq".into(), "0".into()),
        ],
        payload: Some(payload.to_string().into_bytes()),
        ..Frame::default()
    }
}

struct FakeTasks {
    terminal: EventHub<TerminalOutcome>,
    created: Mutex<Vec<(String, Value, Option<String>)>>,
    sent: Mutex<Vec<(String, SendText)>>,
}

impl TaskPort for FakeTasks {
    fn create(
        &self,
        target: WorkspaceTarget,
        selection: Value,
        mode: Option<String>,
    ) -> BoxFuture<'static, Result<String, String>> {
        let mut created = self.created.lock().unwrap();
        created.push((target.workspace_path, selection, mode));
        let id = format!("task-{}", created.len());
        async move { Ok(id) }.boxed()
    }
    fn resume(
        &self,
        _: WorkspaceTarget,
        _: String,
    ) -> BoxFuture<'static, Result<Option<Value>, String>> {
        async { Ok(Some(json!({"providerId": "zai", "modelId": "GLM-5"}))) }.boxed()
    }
    fn send(
        &self,
        target: WorkspaceTarget,
        task_id: String,
        input: SendText,
    ) -> BoxFuture<'static, Result<(), String>> {
        self.sent
            .lock()
            .unwrap()
            .push((task_id.clone(), input.clone()));
        let terminal = self.terminal.clone();
        async move {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                terminal.emit(TerminalOutcome {
                    workspace_key: target.key(),
                    session_id: task_id,
                    outcome: "succeeded",
                    input_id: Some(input.input_id),
                });
            });
            Ok(())
        }
        .boxed()
    }
    fn stop(&self, _: WorkspaceTarget, _: String) -> BoxFuture<'static, Result<(), String>> {
        async { Ok(()) }.boxed()
    }
    fn list(&self, _: String) -> BoxFuture<'static, Vec<Value>> {
        async { Vec::new() }.boxed()
    }
    fn deleted_ids(&self, _: String) -> BoxFuture<'static, Vec<String>> {
        async { Vec::new() }.boxed()
    }
    fn terminal_outcomes(&self) -> BoxStream<'static, TerminalOutcome> {
        self.terminal.subscribe().boxed()
    }
    fn final_reply(&self, _: WorkspaceTarget, _: String) -> BoxFuture<'static, FinalReply> {
        async {
            FinalReply {
                text: Some("Hi from the agent".into()),
                error: None,
            }
        }
        .boxed()
    }
    fn model_view(&self, input: Option<Value>) -> BoxFuture<'static, Value> {
        async move {
            match input {
                Some(input) => json!({"effectiveSelection": input["selection"]}),
                None => json!({
                    "preferredSelection": {"providerId": "zai", "modelId": "GLM-5", "options": {"reasoningLevel": "high"}},
                    "providers": [{"providerId": "zai", "providerName": "Z.ai", "models": [{"modelId": "GLM-5"}]}],
                }),
            }
        }
        .boxed()
    }
}

async fn eventually<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    for _ in 0..200 {
        if let Some(value) = check() {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

fn setup(dir: &std::path::Path, origin: &str, tasks: Arc<FakeTasks>) -> Arc<Bots> {
    let config_dir = dir.join(".zcode").join("v2");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("setting.json"),
        json!({"lastWorkspaceSession": [{"kind": "local", "workspacePath": "/tmp/ws-bot"}]})
            .to_string(),
    )
    .unwrap();
    let endpoints = Endpoints {
        feishu_open: origin.to_owned(),
        lark_open: origin.to_owned(),
        feishu_accounts: origin.to_owned(),
        lark_accounts: origin.to_owned(),
    };
    let bots = Bots::new(BotsDeps {
        repo: BotsRepo::new(config_dir.clone()),
        credentials: CredentialStore::new(&config_dir, CredentialCipher::new(Some("test"), dir)),
        api: Arc::new(FeishuApi::new(endpoints).unwrap()),
        settings: SettingStore::new(dir),
        tasks,
        background: false,
    });
    *bots.name_retry_ms.lock().unwrap() = vec![0];
    bots
}

#[tokio::test]
async fn feishu_registration_polls_until_success() {
    let mock = Arc::new(Mock::default());
    let origin = mock_open(mock.clone()).await;
    let api = FeishuApi::new(Endpoints {
        feishu_open: origin.clone(),
        lark_open: origin.clone(),
        feishu_accounts: origin.clone(),
        lark_accounts: origin,
    })
    .unwrap();
    let begin = api.begin_registration(Some("lark")).await.unwrap();
    assert_eq!(begin["deviceCode"], "dev-1");
    assert_eq!(begin["domain"], "lark");
    assert_eq!(begin["pollDomain"], "feishu");
    assert_eq!(
        begin["qrUrl"],
        "https://accounts.example/verify?code=1&from=sdk&source=node-sdk%2Fzcode&tp=sdk"
    );
    let params = json!({"deviceCode": "dev-1", "domain": "feishu"});
    assert_eq!(api.poll_registration(&params).await.unwrap()["interval"], 5);
    assert_eq!(
        api.poll_registration(&params).await.unwrap()["interval"],
        10
    );
    let done = api.poll_registration(&params).await.unwrap();
    assert_eq!(
        done,
        json!({"status": "success", "appId": "cli_0123456789abcdef", "appSecret": "s",
               "domain": "feishu", "appName": "Scanned", "openId": "ou_owner"})
    );
}

#[tokio::test]
async fn management_members_round_trip_through_the_files() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Arc::new(Mock::default());
    let origin = mock_open(mock.clone()).await;
    let tasks = Arc::new(FakeTasks {
        terminal: EventHub::new(),
        created: Mutex::default(),
        sent: Mutex::default(),
    });
    let bots = setup(dir.path(), &origin, tasks);
    let saved = bots
        .save_bot(&json!({
            "bot": {"id": " feishu-1 ", "name": "", "provider": "feishu", "enabled": true,
                "feishuAppId": "cli_0123456789abcdef", "allowedWorkspaces": [],
                "allowedCommands": {"status": true, "new": true, "workspace": true, "model": true,
                    "thoughtLevel": true, "reply": true},
                "currentOptions": {}, "replyMode": "assistant_changes"},
            "credentialValue": " app-secret ",
        }))
        .await
        .unwrap();
    assert_eq!(saved["id"], "feishu-1");
    assert_eq!(saved["name"], "Mock Bot");
    assert_eq!(saved["credentialRef"], "bot:feishu-1:credential");
    assert_eq!(saved["replyMode"], "streaming_card");
    assert_eq!(saved["allowedWorkspaces"], json!(["*"]));
    let test = bots.test_bot("feishu-1").await.unwrap();
    assert_eq!(
        test,
        json!({"ok": true, "message": "Feishu app credentials are valid.", "provider": "feishu"})
    );
    assert_eq!(
        bots.test_bot("nope").await.unwrap()["message"],
        "Bot not found."
    );
    let status = bots.get_status().await.unwrap();
    assert_eq!(status["botsCount"], 1);
    assert_eq!(status["botRuntime"][0]["message"], "Bot is configured.");
    let code = bots
        .create_bind_code(&json!({"botId": "feishu-1"}))
        .await
        .unwrap();
    assert_eq!(code["code"].as_str().unwrap().len(), 6);
    assert!(
        bots.create_bind_code(&json!({"botId": "x"}))
            .await
            .unwrap_err()
            .message
            .contains("Bot not found: x")
    );
    let duplicate = bots
        .save_bot(
            &json!({"bot": {"id": "feishu-2", "name": "B", "provider": "feishu", "enabled": true,
            "credentialRef": "bot:feishu-1:credential", "allowedWorkspaces": ["*"],
            "allowedCommands": {}, "currentOptions": {}}}),
        )
        .await
        .unwrap_err();
    assert_eq!(
        duplicate.message,
        "Enabled polling bots cannot share the same credential."
    );
    let removed = bots.remove_bot_secret("feishu-1").await.unwrap();
    assert!(removed.get("credentialRef").is_none() && removed.get("feishuAppId").is_none());
    let credentials =
        std::fs::read_to_string(dir.path().join(".zcode/v2/credentials.json")).unwrap();
    assert!(!credentials.contains("bot:feishu-1:credential"));
    bots.delete_bot("feishu-1").await.unwrap();
    assert_eq!(bots.get_status().await.unwrap()["botsCount"], 0);
}

#[tokio::test]
async fn feishu_long_connection_binds_then_runs_a_message() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Arc::new(Mock::default());
    let origin = mock_open(mock.clone()).await;
    let (url, to_client, mut from_client) = mock_ws().await;
    *mock.ws_url.lock().unwrap() = url;
    let tasks = Arc::new(FakeTasks {
        terminal: EventHub::new(),
        created: Mutex::default(),
        sent: Mutex::default(),
    });
    let bots = setup(dir.path(), &origin, tasks.clone());
    bots.save_bot(&json!({
        "bot": {"id": "feishu-1", "name": "Bot", "provider": "feishu", "enabled": true,
            "feishuAppId": "cli_0123456789abcdef", "allowedWorkspaces": ["*"],
            "allowedCommands": {"status": true, "new": true, "workspace": true, "model": true,
                "thoughtLevel": true, "reply": false},
            "currentOptions": {}, "replyMode": "streaming_card"},
        "credentialValue": "app-secret",
    }))
    .await
    .unwrap();
    bots.refresh().await.unwrap();
    eventually("connected status", || {
        let status = bots.runtime_status("feishu-1")?;
        (status.get("message") == Some(&json!("Feishu WebSocket is running."))).then_some(())
    })
    .await;

    // 未绑定：任何命令都回 botDisabled（找不到绑定这个用户的 Bot）。
    to_client.send(event_frame("m0", "/help")).unwrap();
    let ack = loop {
        let frame = from_client.recv().await.unwrap();
        if frame.method == METHOD_DATA {
            break frame;
        }
    };
    assert_eq!(ack.header("message_id"), Some("m0"));
    assert!(ack.header("biz_rt").is_some());
    assert_eq!(ack.payload.as_deref(), Some(br#"{"code":200}"#.as_slice()));
    eventually("botDisabled reply", || {
        (mock.cards().len() == 1).then_some(())
    })
    .await;
    assert_eq!(mock.cards()[0], "当前 bot 未启用。");

    let code = bots
        .create_bind_code(&json!({"botId": "feishu-1"}))
        .await
        .unwrap();
    let code = code["code"].as_str().unwrap().to_lowercase();
    to_client
        .send(event_frame("m1", &format!("/bind {code}")))
        .unwrap();
    eventually("bind reply", || (mock.cards().len() == 2).then_some(())).await;
    let bind = &mock.cards()[1];
    assert!(bind.starts_with("绑定成功。"), "{bind}");
    assert!(!bind.contains("/回复"), "reply command is disabled: {bind}");
    let config = bots.repo.read_config().await.unwrap();
    assert_eq!(config["bots"][0]["providerUserId"], "ou_user");

    to_client.send(event_frame("m2", "/status")).unwrap();
    eventually("status reply", || (mock.cards().len() == 3).then_some(())).await;
    assert_eq!(
        mock.cards()[2],
        "工作区: ws-bot  \n模型: Z.ai/GLM-5\n\n---\n\n任务: 草稿  \n状态: 草稿"
    );

    // 同一条消息重投只处理一次。
    to_client.send(event_frame("m3", "hello agent")).unwrap();
    to_client.send(event_frame("m3", "hello agent")).unwrap();
    eventually("terminal reply", || (mock.cards().len() == 4).then_some(())).await;
    assert_eq!(mock.cards()[3], "Hi from the agent");
    let created = tasks.created.lock().unwrap().clone();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].0, "/tmp/ws-bot");
    assert_eq!(created[0].2.as_deref(), Some("yolo"));
    let sent = tasks.sent.lock().unwrap().clone();
    assert_eq!(sent[0].1.text, "hello agent");
    assert_eq!(
        sent[0].1.bot_delivery_target,
        Some(
            json!({"provider": "feishu", "botId": "feishu-1", "providerUserId": "ou_user", "chatType": "private"})
        )
    );
    let state = bots.repo.read_state().await.unwrap();
    assert_eq!(state["bots"]["feishu-1"]["mode"], "task");
    assert_eq!(state["bots"]["feishu-1"]["activeTaskId"], "task-1");

    // 未移植的选择类命令回未知命令。
    to_client.send(event_frame("m4", "/model")).unwrap();
    eventually("unknown reply", || (mock.cards().len() == 5).then_some(())).await;
    assert_eq!(mock.cards()[4], "未知命令：**/model**");

    bots.dispose().await;
    assert_eq!(
        bots.runtime_status("feishu-1").unwrap()["message"],
        "Feishu WebSocket is stopped."
    );
}

#[tokio::test]
async fn automation_runs_are_delivered_to_the_bound_chat() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Arc::new(Mock::default());
    let origin = mock_open(mock.clone()).await;
    let tasks = Arc::new(FakeTasks {
        terminal: EventHub::new(),
        created: Mutex::default(),
        sent: Mutex::default(),
    });
    let bots = setup(dir.path(), &origin, tasks.clone());
    bots.save_bot(&json!({
        "bot": {"id": "feishu-1", "name": "Bot", "provider": "feishu", "enabled": true,
            "feishuAppId": "cli_0123456789abcdef", "providerUserId": "ou_user", "allowedWorkspaces": ["*"],
            "allowedCommands": {}, "currentOptions": {}},
        "credentialValue": "app-secret",
    }))
    .await
    .unwrap();
    bots.watch_automation_run(&json!({
        "target": {"provider": "feishu", "botId": "feishu-1", "providerUserId": "oc_group", "chatType": "group"},
        "taskId": "run-session",
        "workspacePath": "/tmp/ws-bot",
    }))
    .await;
    tasks.terminal.emit(TerminalOutcome {
        workspace_key: "/tmp/ws-bot".into(),
        session_id: "run-session".into(),
        outcome: "succeeded",
        input_id: Some("run-1".into()),
    });
    eventually("automation reply", || {
        (mock.cards().len() == 1).then_some(())
    })
    .await;
    assert_eq!(mock.sent.lock().unwrap()[0]["receive_id"], "oc_group");
    assert_eq!(mock.cards()[0], "Hi from the agent");
    // 通道不一致：跳过。
    bots.watch_automation_run(&json!({
        "target": {"provider": "lark", "botId": "feishu-1", "providerUserId": "ou_user", "chatType": "private"},
        "taskId": "other", "workspacePath": "/tmp/ws-bot",
    }))
    .await;
    assert_eq!(mock.cards().len(), 1);
}
