//! bots 测试：对拍 fixture（TS 生成）+ 127.0.0.1 上的飞书 mock（开放平台 HTTP 与长连接 WS）。
//! 不连任何真实飞书服务，不读写真实 HOME。
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::feishu::api::{Endpoints, FeishuApi};
use super::feishu::cards::{self, StreamBlock, StreamState, StreamStatus};
use super::feishu::frame::{Frame, METHOD_CONTROL, METHOD_DATA};
use super::feishu::{card_markdown, parse_callback, split_text, strip_mentions};
use super::model::{
    import_legacy_config, import_legacy_state, message, normalize_allowed_workspaces,
    normalize_bot, parse_command, parse_config_file, parse_state_file,
};
use super::store::BotsRepo;
use super::test_support::{Answer, FakeTasks, MockServer, bots_with, eventually};
use crate::account::credential::CredentialStore;
use crate::automation::driver::TerminalOutcome;

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

/// Bot 应用凭据经 account 的共享存储读写（加解密与 Node 的互通由 account 区域的测试覆盖）。
#[tokio::test]
async fn bot_credentials_use_the_shared_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = CredentialStore::new(dir.path(), "s");
    store.save("bot:a:credential", "secret-1").await.unwrap();
    assert_eq!(
        store.load("bot:a:credential").await.unwrap().as_deref(),
        Some("secret-1")
    );
    let saved: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("credentials.json")).unwrap(),
    )
    .unwrap();
    assert!(
        saved["bot:a:credential"]
            .as_str()
            .unwrap()
            .starts_with("enc:v1:")
    );
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

/// 飞书开放平台 mock：token、应用名、发卡片（记录）、PATCH 卡片（记录）、长连接接入点、扫码注册。
#[derive(Default)]
struct Feishu {
    ws_url: Mutex<String>,
    poll_count: Mutex<u32>,
}

fn card_text(card: &Value) -> String {
    card["body"]["elements"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|element| element["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 发出的卡片（POST 创建）与原地更新（PATCH）的正文。
fn sent_cards(server: &MockServer) -> Vec<(String, Value)> {
    server
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| {
            request.path.starts_with("/open-apis/im/v1/messages")
                && !request.path.contains("/reactions")
        })
        .map(|request| {
            let card: Value =
                serde_json::from_str(request.body["content"].as_str().unwrap_or("null")).unwrap();
            (request.method.clone(), card)
        })
        .collect()
}

async fn feishu_server(state: Arc<Feishu>) -> MockServer {
    let server = MockServer::start().await;
    let counter = Arc::new(Mutex::new(0u32));
    server.on(move |request| {
        let body = &request.body;
        let form = String::from_utf8_lossy(&request.raw).to_string();
        Some(match request.path.as_str() {
            "/open-apis/auth/v3/tenant_access_token/internal" => Answer::json(200, if body["app_secret"] == "app-secret" {
                json!({"code": 0, "tenant_access_token": "t-1", "expire": 7200})
            } else {
                json!({"code": 10014, "msg": "app secret invalid"})
            }),
            "/open-apis/application/v6/applications/cli_0123456789abcdef" => {
                Answer::json(200, json!({"code": 0, "data": {"app": {"app_name": " Mock Bot "}}}))
            }
            "/open-apis/im/v1/messages" => {
                assert_eq!(request.headers["authorization"], "Bearer t-1");
                let mut count = counter.lock().unwrap();
                *count += 1;
                Answer::json(200, json!({"code": 0, "data": {"message_id": format!("om_{count}")}}))
            }
            path if path.starts_with("/open-apis/im/v1/messages/") && path.ends_with("/reactions") => {
                Answer::json(200, json!({"code": 0, "data": {"reaction_id": "r1"}}))
            }
            path if path.starts_with("/open-apis/im/v1/messages/") => Answer::json(200, json!({"code": 0})),
            path if path.starts_with("/open-apis/contact/v3/users/") => {
                Answer::json(200, json!({"code": 0, "data": {"user": {"name": "Ann"}}}))
            }
            "/callback/ws/endpoint" => {
                let url = state.ws_url.lock().unwrap().clone();
                Answer::json(200, json!({"code": 0, "data": {"URL": url, "ClientConfig": {
                    "PingInterval": 120, "ReconnectCount": 1, "ReconnectInterval": 1, "ReconnectNonce": 0}}}))
            }
            "/oauth/v1/app/registration" if form.contains("action=init") => {
                Answer::json(200, json!({"supported_auth_methods": ["client_secret"]}))
            }
            "/oauth/v1/app/registration" if form.contains("action=begin") => Answer::json(200, json!({
                "device_code": "dev-1", "user_code": "U-1", "interval": 3, "expire_in": 60,
                "verification_uri_complete": "https://accounts.example/verify?code=1&from=web",
            })),
            "/oauth/v1/app/registration" => {
                let mut count = state.poll_count.lock().unwrap();
                *count += 1;
                Answer::json(200, match *count {
                    1 => json!({"error": "authorization_pending"}),
                    2 => json!({"error": "slow_down"}),
                    _ => json!({"client_id": "cli_0123456789abcdef", "client_secret": "s",
                        "app": {"name": "Scanned"}, "user_info": {"open_id": "ou_owner"}}),
                })
            }
            _ => return None,
        })
    });
    server
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

fn data_frame(message_id: &str, payload: Value) -> Frame {
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

fn event_frame(message_id: &str, text: &str) -> Frame {
    data_frame(
        message_id,
        json!({
            "schema": "2.0",
            "header": {"event_id": format!("ev-{message_id}"), "event_type": "im.message.receive_v1"},
            "event": {
                "sender": {"sender_id": {"open_id": "ou_user"}},
                "message": {"message_id": message_id, "chat_id": "oc_chat", "chat_type": "p2p",
                    "message_type": "text", "content": json!({"text": text}).to_string()},
            },
        }),
    )
}

fn card_action_frame(id: &str, command: &str) -> Frame {
    data_frame(
        id,
        json!({
            "schema": "2.0",
            "header": {"event_id": format!("ev-{id}"), "event_type": "card.action.trigger"},
            "event": {"operator": {"open_id": "ou_user"}, "token": "c-token",
                "action": {"value": {"command": command, "zcodeCardText": "Pick"}},
                "context": {"open_chat_id": "oc_chat", "open_message_id": "om_x"}},
        }),
    )
}

#[tokio::test]
async fn feishu_registration_polls_until_success() {
    let server = feishu_server(Arc::new(Feishu::default())).await;
    let origin = server.origin.clone();
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
    let server = feishu_server(Arc::new(Feishu::default())).await;
    let bots = bots_with(dir.path(), &server.origin, FakeTasks::new());
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
    // discord / wecom 没有适配器。
    bots.save_bot(
        &json!({"bot": {"id": "d1", "name": "D", "provider": "discord", "enabled": true,
        "allowedWorkspaces": ["*"], "allowedCommands": {}, "currentOptions": {}}}),
    )
    .await
    .unwrap();
    assert_eq!(
        bots.test_bot("d1").await.unwrap(),
        json!({"ok": false, "message": "discord is reserved for a future version.", "provider": "discord"})
    );
}

async fn wait_ack(from_client: &mut mpsc::UnboundedReceiver<Frame>, message_id: &str) -> Frame {
    loop {
        let frame = from_client.recv().await.unwrap();
        if frame.method == METHOD_DATA && frame.header("message_id") == Some(message_id) {
            return frame;
        }
    }
}

#[tokio::test]
async fn feishu_long_connection_binds_then_streams_a_card_reply() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(Feishu::default());
    let server = feishu_server(state.clone()).await;
    let (url, to_client, mut from_client) = mock_ws().await;
    *state.ws_url.lock().unwrap() = url;
    let tasks = FakeTasks::new();
    tasks.script.lock().unwrap().tools = vec![("Bash".into(), json!({"command": "ls -la"}))];
    let bots = bots_with(dir.path(), &server.origin, tasks.clone());
    *bots.card_interval_ms.lock().unwrap() = 0;
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
    bots.refresh_feishu().await.unwrap();
    eventually("connected status", || {
        let status = bots.runtime_status("feishu-1")?;
        (status.get("message") == Some(&json!("Feishu WebSocket is running."))).then_some(())
    })
    .await;

    // 未绑定：任何命令都回 botDisabled（找不到绑定这个用户的 Bot）。
    to_client.send(event_frame("m0", "/help")).unwrap();
    let ack = wait_ack(&mut from_client, "m0").await;
    assert!(ack.header("biz_rt").is_some());
    assert_eq!(ack.payload.as_deref(), Some(br#"{"code":200}"#.as_slice()));
    eventually("botDisabled reply", || {
        (sent_cards(&server).len() == 1).then_some(())
    })
    .await;
    assert_eq!(card_text(&sent_cards(&server)[0].1), "当前 bot 未启用。");

    let code = bots
        .create_bind_code(&json!({"botId": "feishu-1"}))
        .await
        .unwrap();
    let code = code["code"].as_str().unwrap().to_lowercase();
    to_client
        .send(event_frame("m1", &format!("/bind {code}")))
        .unwrap();
    eventually("bind reply", || {
        (sent_cards(&server).len() == 2).then_some(())
    })
    .await;
    let bind = card_text(&sent_cards(&server)[1].1);
    assert!(bind.starts_with("绑定成功。"), "{bind}");
    assert!(!bind.contains("/回复"), "reply command is disabled: {bind}");
    let config = bots.repo.read_config().await.unwrap();
    assert_eq!(config["bots"][0]["providerUserId"], "ou_user");
    // 绑定时从通讯录补了昵称。
    assert_eq!(config["bots"][0]["displayName"], "Ann");

    to_client.send(event_frame("m2", "/status")).unwrap();
    eventually("status reply", || {
        (sent_cards(&server).len() == 3).then_some(())
    })
    .await;
    assert_eq!(
        card_text(&sent_cards(&server)[2].1),
        "工作区: ws-bot  \n模型: Z.ai/GLM-5\n\n---\n\n任务: 草稿  \n状态: 草稿"
    );
    // 短命令的 Typing reaction 在回复后删除。
    eventually("typing reaction removed", || {
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.method == "DELETE" && request.path.ends_with("/reactions/r1"))
            .then_some(())
    })
    .await;

    // 普通消息：流式卡片先创建，再随工具与正文 PATCH，终态显示已完成。同一条消息重投只处理一次。
    to_client.send(event_frame("m3", "hello agent")).unwrap();
    to_client.send(event_frame("m3", "hello agent")).unwrap();
    let final_card = eventually("completed card", || {
        sent_cards(&server)
            .into_iter()
            .skip(3)
            .map(|(_, card)| card_text(&card))
            .find(|text| text.contains("✅ 已完成"))
    })
    .await;
    assert!(final_card.contains("Hi from the agent"), "{final_card}");
    let cards_after = sent_cards(&server).into_iter().skip(3).collect::<Vec<_>>();
    assert_eq!(
        cards_after[0].0, "POST",
        "the streaming card is created once"
    );
    assert!(cards_after[1..].iter().all(|(method, _)| method == "PATCH"));
    let with_tools = cards_after
        .iter()
        .find(|(_, card)| card.to_string().contains("collapsible_panel"))
        .expect("tool panel");
    assert!(
        with_tools.1.to_string().contains("Bash"),
        "{}",
        with_tools.1
    );
    let created = tasks.created.lock().unwrap().clone();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].0, "/tmp/ws-bot");
    assert_eq!(created[0].2.as_deref(), Some("yolo"));
    let sent = tasks.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1.text, "hello agent");
    assert_eq!(
        sent[0].1.bot_delivery_target,
        Some(
            json!({"provider": "feishu", "botId": "feishu-1", "providerUserId": "ou_user", "chatType": "private"})
        )
    );
    let state_file = bots.repo.read_state().await.unwrap();
    assert_eq!(state_file["bots"]["feishu-1"]["mode"], "task");
    assert_eq!(state_file["bots"]["feishu-1"]["activeTaskId"], "task-1");

    // 选择命令：/model 回供应商选择卡片，点击按钮经长连接同步回包返回模型卡片。
    let before = sent_cards(&server).len();
    to_client.send(event_frame("m4", "/model")).unwrap();
    eventually("model card", || {
        (sent_cards(&server).len() > before).then_some(())
    })
    .await;
    let model_card = sent_cards(&server)[before].1.clone();
    assert!(
        card_text(&model_card).starts_with("当前模型 Z.ai/GLM-5$high"),
        "{model_card}"
    );
    assert!(
        model_card.to_string().contains("/model provider zai"),
        "{model_card}"
    );
    to_client
        .send(card_action_frame("c1", "/model provider mock"))
        .unwrap();
    let ack = wait_ack(&mut from_client, "c1").await;
    let body: Value = serde_json::from_slice(ack.payload.as_deref().unwrap()).unwrap();
    assert_eq!(body["code"], 200);
    let data: Value = serde_json::from_slice(
        &base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            body["data"].as_str().unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let card = &data["card"]["data"];
    assert!(card_text(card).contains("选择模型"), "{card}");
    assert!(
        card.to_string().contains("/model model custom:mock:m1"),
        "{card}"
    );

    let lock = super::runtime_lock::lock_path(
        &dir.path().join(".zcode/v2"),
        "feishu-websocket",
        "cli_0123456789abcdef",
    );
    assert!(lock.is_dir());
    bots.shutdown().await;
    assert_eq!(
        bots.runtime_status("feishu-1").unwrap()["message"],
        "Feishu WebSocket is stopped."
    );
    // Host 退出立即释放长连接锁，不等租约过期。
    assert!(!lock.exists());
}

#[tokio::test]
async fn automation_runs_are_delivered_to_the_bound_chat() {
    let dir = tempfile::tempdir().unwrap();
    let server = feishu_server(Arc::new(Feishu::default())).await;
    let tasks = FakeTasks::new();
    let bots = bots_with(dir.path(), &server.origin, tasks.clone());
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
    // automation 派发方随后才发送 prompt。
    use super::TaskPort;
    tasks
        .send(
            crate::agent::WorkspaceTarget::local("/tmp/ws-bot"),
            "run-session".into(),
            crate::automation::driver::SendText {
                input_id: "run-1".into(),
                text: "daily report".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    // Node 把 automation 回推的颗粒度设为 summary_changes，但飞书只支持流式卡片，归一后仍是流式卡片。
    let done = eventually("automation reply", || {
        sent_cards(&server)
            .into_iter()
            .map(|(_, card)| card_text(&card))
            .find(|text| text.contains("✅ 已完成"))
    })
    .await;
    assert_eq!(done, "Hi from the agent\n_✅ 已完成_");
    let request = &server.to("/open-apis/im/v1/messages")[0];
    assert_eq!(request.body["receive_id"], "oc_group");
    assert_eq!(request.query, "receive_id_type=chat_id");
    let count = sent_cards(&server).len();
    // 通道不一致：跳过。
    bots.watch_automation_run(&json!({
        "target": {"provider": "lark", "botId": "feishu-1", "providerUserId": "ou_user", "chatType": "private"},
        "taskId": "other", "workspacePath": "/tmp/ws-bot",
    }))
    .await;
    assert_eq!(sent_cards(&server).len(), count);
}

#[tokio::test]
async fn subscription_failure_falls_back_to_terminal_outcomes() {
    let dir = tempfile::tempdir().unwrap();
    let server = feishu_server(Arc::new(Feishu::default())).await;
    let tasks = FakeTasks::new();
    *tasks.subscribe_fails.lock().unwrap() = true;
    let bots = bots_with(dir.path(), &server.origin, tasks.clone());
    bots.save_bot(&json!({
        "bot": {"id": "feishu-1", "name": "Bot", "provider": "feishu", "enabled": true,
            "feishuAppId": "cli_0123456789abcdef", "providerUserId": "ou_user", "allowedWorkspaces": ["*"],
            "allowedCommands": {}, "currentOptions": {}},
        "credentialValue": "app-secret",
    }))
    .await
    .unwrap();
    bots.watch_automation_run(&json!({
        "target": {"provider": "feishu", "botId": "feishu-1", "providerUserId": "ou_user", "chatType": "private"},
        "taskId": "run-session", "workspacePath": "/tmp/ws-bot",
    }))
    .await;
    tasks.terminal.emit(TerminalOutcome {
        workspace_key: "/tmp/ws-bot".into(),
        session_id: "run-session".into(),
        outcome: "succeeded",
        input_id: Some("run-1".into()),
    });
    eventually("fallback reply", || {
        (sent_cards(&server).len() == 1).then_some(())
    })
    .await;
    assert_eq!(card_text(&sent_cards(&server)[0].1), "Fallback reply");
}

#[test]
fn streaming_cards_split_by_element_budget() {
    let mut blocks = Vec::new();
    for index in 0..120 {
        blocks.push(StreamBlock::Message(format!("part {index}")));
        blocks.push(StreamBlock::Tools {
            summaries: vec![format!("- 完成 · Bash · `echo {index}`")],
            title: None,
            expanded: None,
        });
    }
    let state = StreamState {
        locale: None,
        blocks,
        status: StreamStatus::Completed,
    };
    let segments = cards::split_streaming(&state);
    assert!(segments.len() > 1);
    for segment in &segments[..segments.len() - 1] {
        assert_eq!(segment.status, StreamStatus::Sealed);
        assert!(cards::tagged_elements(segment) <= cards::STREAMING_CARD_TAGGED_ELEMENT_BUDGET);
        // 封存段不带状态行。
        assert!(!cards::streaming(segment).to_string().contains("已完成"));
    }
    let last = segments.last().unwrap();
    assert_eq!(last.status, StreamStatus::Completed);
    assert!(cards::streaming(last).to_string().contains("✅ 已完成"));
    let empty = cards::streaming(&StreamState {
        locale: Some("en-US".into()),
        blocks: Vec::new(),
        status: StreamStatus::Running,
    });
    assert_eq!(empty["body"]["elements"][1]["content"], "_Running_");
    let selection = cards::interactive(
        &json!({"text": "Pick", "selection": {"action": "task.set", "cancelLabel": "取消",
        "options": [{"id": "t1", "label": ""}]}}),
    );
    assert_eq!(selection["body"]["elements"][1]["text"]["content"], "1");
    assert_eq!(
        selection["body"]["elements"][1]["behaviors"][0]["value"]["command"],
        "/task t1"
    );
    assert_eq!(
        selection["body"]["elements"][2]["behaviors"][0]["value"]["command"],
        "/cancel"
    );
}
