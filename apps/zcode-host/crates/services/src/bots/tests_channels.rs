//! 通道（Telegram / Webhook / 微信）对 127.0.0.1 mock 的测试：配置、轮询生命周期、游标提交、
//! 消息格式、重试与选择回调。不连任何真实服务，不读写真实 HOME。
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::channel::Polling;
use super::test_support::{
    Answer, FakeTasks, MockServer, Script, bots_in, bots_with, bound_bot, eventually,
};

fn last_body(server: &MockServer, suffix: &str) -> Option<Value> {
    server.to(suffix).last().map(|request| request.body.clone())
}

/// Telegram Bot API mock：getUpdates 依次弹出预置的批次（未提交 offset 时同一批次重投）。
async fn telegram_server(batches: Arc<Mutex<VecDeque<Vec<Value>>>>) -> MockServer {
    let server = MockServer::start().await;
    server.on(move |request| {
        let method = request
            .path
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_owned();
        if !request.path.starts_with("/bottg-token/") {
            return None;
        }
        Some(match method.as_str() {
            "getMe" => Answer::json(
                200,
                json!({"ok": true, "result": {"first_name": "TG Bot", "username": "tgbot"}}),
            ),
            "getUpdates" => {
                let offset = request.body["offset"].as_i64().unwrap_or(0);
                let mut batches = batches.lock().unwrap();
                let batch: Vec<Value> = batches
                    .front()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|update| update["update_id"].as_i64().unwrap_or(0) >= offset)
                    .collect();
                if batch.is_empty() {
                    batches.pop_front();
                    let mut answer = Answer::json(200, json!({"ok": true, "result": []}));
                    answer.delay = Duration::from_millis(50);
                    return Some(answer);
                }
                Answer::json(200, json!({"ok": true, "result": batch}))
            }
            _ => Answer::json(200, json!({"ok": true, "result": true})),
        })
    });
    server
}

fn message(update_id: i64, text: &str) -> Value {
    json!({"update_id": update_id, "message": {"message_id": update_id, "text": text,
        "chat": {"id": 42, "type": "private"}, "from": {"id": 42, "first_name": "Ann"}}})
}

fn callback(update_id: i64, data: &str) -> Value {
    json!({"update_id": update_id, "callback_query": {"id": format!("cq{update_id}"), "data": data,
        "from": {"id": 42}, "message": {"message_id": 7, "chat": {"id": 42, "type": "private"}}}})
}

fn sent_texts(server: &MockServer) -> Vec<String> {
    server
        .to("/sendMessage")
        .iter()
        .map(|request| request.body["text"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[tokio::test]
async fn telegram_polls_commits_offsets_and_streams_replies() {
    let dir = tempfile::tempdir().unwrap();
    let batches = Arc::new(Mutex::new(VecDeque::from(vec![
        vec![message(10, "hello agent")],
        vec![message(11, "/project")],
        vec![callback(12, "zc:workspace:2")],
    ])));
    let server = telegram_server(batches.clone()).await;
    let tasks = FakeTasks::new();
    tasks.script.lock().unwrap().tools = vec![("Bash".into(), json!({"command": "pwd"}))];
    let bots = bots_in(dir.path(), &server.origin, tasks.clone(), true);
    let saved = bots
        .save_bot(&json!({"bot": bound_bot("tg-1", "telegram", "42", json!({"name": ""})), "credentialValue": "tg-token"}))
        .await
        .unwrap();
    // 名称从 getMe 回填。
    assert_eq!(saved["name"], "TG Bot");
    assert_eq!(saved["replyMode"], "assistant_changes");
    assert_eq!(
        bots.test_bot("tg-1").await.unwrap(),
        json!({"ok": true, "name": "TG Bot", "message": "Telegram bot is reachable.", "provider": "telegram"})
    );
    // 启用时同步菜单、清掉 webhook，然后开始长轮询。
    let commands = eventually("setMyCommands", || last_body(&server, "/setMyCommands")).await;
    assert_eq!(
        commands["commands"][0],
        json!({"command": "help", "description": "Show help"})
    );
    assert_eq!(commands["commands"].as_array().unwrap().len(), 9);
    eventually("deleteWebhook", || last_body(&server, "/deleteWebhook")).await;

    // 普通消息：先发工具前的空缓冲（无），完成后整段发出；typing 周期发送。
    let reply = eventually("assistant reply", || {
        sent_texts(&server)
            .into_iter()
            .find(|text| text == "Hi from the agent")
    })
    .await;
    assert_eq!(reply, "Hi from the agent");
    let send = server.to("/sendMessage")[0].body.clone();
    assert_eq!(send["chat_id"], "42");
    assert_eq!(send["parse_mode"], "Markdown");
    assert_eq!(
        last_body(&server, "/sendChatAction").unwrap(),
        json!({"chat_id": "42", "action": "typing"})
    );
    assert_eq!(tasks.sent.lock().unwrap()[0].1.text, "hello agent");

    // /project 列出 workspace（inline 按钮只带序号），点按钮切换。
    let menu = eventually("workspace menu", || {
        server
            .to("/sendMessage")
            .into_iter()
            .find(|request| request.body["reply_markup"].is_object())
    })
    .await;
    assert!(
        menu.body["text"]
            .as_str()
            .unwrap()
            .starts_with("当前 workspace ws-bot")
    );
    assert_eq!(
        menu.body["reply_markup"]["inline_keyboard"],
        json!([[{"text": "ws-bot · 当前", "callback_data": "zc:workspace:1"}],
               [{"text": "ws-other", "callback_data": "zc:workspace:2"}],
               [{"text": "取消", "callback_data": "zc:cancel"}]])
    );
    let answer = eventually("callback answer", || {
        last_body(&server, "/answerCallbackQuery")
    })
    .await;
    assert_eq!(answer["callback_query_id"], "cq12");
    assert!(
        answer["text"]
            .as_str()
            .unwrap()
            .starts_with("工作区: ws-other"),
        "{answer}"
    );
    eventually("buttons removed", || {
        last_body(&server, "/editMessageReplyMarkup")
    })
    .await;
    // offset 只在回调成功后提交为 update_id + 1。
    eventually("offset committed", || {
        let state = std::fs::read_to_string(dir.path().join(".zcode/v2/bot-state.v3.json")).ok()?;
        let state: Value = serde_json::from_str(&state).ok()?;
        (state["bots"]["tg-1"]["telegramOffset"] == 13
            && state["bots"]["tg-1"]["workspacePath"] == "/tmp/ws-other")
            .then_some(())
    })
    .await;
    let status = bots.get_status().await.unwrap();
    assert_eq!(
        status["botRuntime"][0]["messageId"],
        "bots.runtime.telegramLongPollingRunning"
    );
    assert_eq!(status["botRuntime"][0]["offset"], 13);

    // 禁用后停止轮询并删除菜单。
    let mut disabled = saved.clone();
    disabled["enabled"] = json!(false);
    bots.save_bot(&json!({"bot": disabled})).await.unwrap();
    eventually("deleteMyCommands", || {
        last_body(&server, "/deleteMyCommands")
    })
    .await;
    eventually("disabled status", || {
        bots.runtime_status("tg-1")
            .filter(|status| status.get("status") == Some(&json!("disabled")))
            .map(|_| ())
    })
    .await;
    bots.dispose().await;
}

#[tokio::test]
async fn telegram_business_failures_keep_the_offset_for_redelivery() {
    let dir = tempfile::tempdir().unwrap();
    let batches = Arc::new(Mutex::new(VecDeque::from(vec![vec![message(20, "hello")]])));
    let server = telegram_server(batches).await;
    let tasks = FakeTasks::new();
    *tasks.create_fails.lock().unwrap() = Some("model unavailable".into());
    let bots = bots_with(dir.path(), &server.origin, tasks.clone());
    bots.save_bot(&json!({"bot": bound_bot("tg-1", "telegram", "42", json!({})), "credentialValue": "tg-token"}))
        .await
        .unwrap();
    bots.refresh_polling(Polling::Telegram).await.unwrap();
    let notice = eventually("failure notice", || {
        sent_texts(&server)
            .into_iter()
            .find(|text| text.starts_with("处理机器人回调失败"))
    })
    .await;
    assert_eq!(notice, "处理机器人回调失败：model unavailable");
    eventually("retrying status", || {
        bots.runtime_status("tg-1")
            .filter(|status| {
                status.get("messageId")
                    == Some(&json!("bots.runtime.telegramPollingFailedRetrying"))
            })
            .map(|_| ())
    })
    .await;
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join(".zcode/v2/bot-state.v3.json")).unwrap(),
    )
    .unwrap();
    assert!(state["bots"]["tg-1"]["telegramOffset"].is_null(), "{state}");
    bots.dispose().await;
}

#[tokio::test]
async fn telegram_conflicts_and_markdown_fallbacks_match_node() {
    let server = MockServer::start().await;
    server.on(|request| {
        if request.path.ends_with("/getUpdates") {
            return Some(Answer::json(409, json!({"ok": false})));
        }
        if request.path.ends_with("/sendMessage") {
            // Markdown 解析失败时 Telegram 回 400：去掉 parse_mode 重发。
            let status = if request.body["parse_mode"].is_string() {
                400
            } else {
                200
            };
            return Some(Answer::json(status, json!({"ok": status == 200})));
        }
        Some(Answer::json(200, json!({"ok": true, "result": true})))
    });
    let http = super::providers::Http::new().unwrap();
    super::telegram::send(
        &http,
        &server.origin,
        "tg-token",
        &json!({"providerUserId": "42", "text": "a_b"}),
    )
    .await
    .unwrap();
    let bodies: Vec<Value> = server
        .to("/sendMessage")
        .iter()
        .map(|request| request.body.clone())
        .collect();
    assert_eq!(
        bodies,
        vec![
            json!({"chat_id": "42", "text": "a_b", "parse_mode": "Markdown"}),
            json!({"chat_id": "42", "text": "a_b"}),
        ]
    );
    let dir = tempfile::tempdir().unwrap();
    let bots = bots_with(dir.path(), &server.origin, FakeTasks::new());
    bots.save_bot(&json!({"bot": bound_bot("tg-1", "telegram", "42", json!({})), "credentialValue": "tg-token"}))
        .await
        .unwrap();
    bots.refresh_polling(Polling::Telegram).await.unwrap();
    eventually("conflict status", || {
        bots.runtime_status("tg-1")
            .filter(|status| {
                status.get("message")
                    == Some(&json!(
                        "Telegram token is already used by another polling client."
                    ))
            })
            .map(|_| ())
    })
    .await;
    bots.stop_polling(Polling::Telegram, "tg-1").await;
    let status = bots.runtime_status("tg-1").unwrap();
    assert_eq!(status["message"], "Telegram long polling is stopped.");
    assert_eq!(status["status"], "idle");
}

#[tokio::test]
async fn webhook_round_trips_with_secret_retry_and_selection_text() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let failures = Arc::new(Mutex::new(2));
    server.on(move |request| {
        if request.path != "/hook" {
            return None;
        }
        let mut left = failures.lock().unwrap();
        if *left > 0 {
            *left -= 1;
            return Some(Answer::json(502, json!({})));
        }
        Some(Answer::json(200, json!({"ok": true})))
    });
    let tasks = FakeTasks::new();
    let bots = bots_with(dir.path(), &server.origin, tasks.clone());
    let hook = format!("{}/hook", server.origin);
    bots.save_bot(
        &json!({"bot": bound_bot("wh-1", "webhook", "u1", json!({"webhookUrl": hook})),
        "webhookSecretValue": "s3cret"}),
    )
    .await
    .unwrap();
    // 测试连接：两次 502 后第三次成功（0 / 500 / 1500 ms 重试，测试缩短）。
    let test = bots.test_bot("wh-1").await.unwrap();
    assert_eq!(
        test,
        json!({"ok": true, "message": "Webhook outbound endpoint is reachable.", "provider": "webhook"})
    );
    let probe = server.to("/hook").last().unwrap().clone();
    assert_eq!(probe.body["type"], "zcode.bot.test");
    assert_eq!(probe.headers["x-zcode-bot-secret"], "s3cret");

    // secret 不符：回 webhookSecretInvalid，不处理。
    let rejected = bots
        .provider_callback(
            "webhook",
            json!({"botId": "wh-1", "userId": "u1", "text": "hi", "webhookSecret": "nope"}),
        )
        .await
        .unwrap();
    assert_eq!(rejected["replies"][0]["text"], "Webhook secret 校验失败。");
    assert!(tasks.created.lock().unwrap().is_empty());

    // 普通消息：回复 POST 到 webhookUrl。
    let result = bots
        .provider_callback(
            "webhook",
            json!({"botId": "wh-1", "userId": "u1", "text": "hello", "messageId": "m1", "webhookSecret": "s3cret"}),
        )
        .await
        .unwrap();
    assert_eq!(result, json!({"ok": true, "replies": []}));
    let reply = eventually("webhook reply", || {
        server
            .to("/hook")
            .into_iter()
            .find(|request| request.body["text"] == "Hi from the agent")
    })
    .await;
    assert_eq!(reply.body["type"], "zcode.bot.message");
    assert_eq!(reply.body["userId"], "u1");
    assert_eq!(reply.body["provider"], "webhook");
    // 同一 messageId 重投只处理一次。
    bots.provider_callback(
        "webhook",
        json!({"botId": "wh-1", "userId": "u1", "text": "hello", "messageId": "m1", "webhookSecret": "s3cret"}),
    )
    .await
    .unwrap();
    assert_eq!(tasks.sent.lock().unwrap().len(), 1);

    // 选择：正文里列出命令；回复同时带结构化 selection。
    let result = bots
        .provider_callback(
            "webhook",
            json!({"botId": "wh-1", "userId": "u1", "text": "/reply", "webhookSecret": "s3cret"}),
        )
        .await
        .unwrap();
    assert_eq!(result["replies"][0]["selection"]["action"], "reply.set");
    let menu = eventually("reply menu", || {
        server
            .to("/hook")
            .into_iter()
            .find(|request| request.body["selection"].is_object())
    })
    .await;
    assert!(
        menu.body["text"]
            .as_str()
            .unwrap()
            .contains("1. 标准回复 · 当前\n/reply assistant_changes"),
        "{}",
        menu.body
    );
}

#[tokio::test]
async fn webhook_structured_elicitation_resolves_the_interaction() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    server.on(|request| (request.path == "/hook").then(|| Answer::json(200, json!({"ok": true}))));
    let tasks = FakeTasks::new();
    *tasks.script.lock().unwrap() = Script {
        chunks: vec!["Thanks!".into()],
        interaction: Some(
            json!({"interactionId": "ask-1", "kind": "userInput", "anchorRowId": null, "createdAt": 1,
            "payload": {"kind": "userInput", "prompt": "Pick", "freeText": true, "traceId": "run-1",
                "questions": [{"question": "Color?", "header": "Color", "options": [
                    {"value": "red", "label": "Red"}, {"value": "blue", "label": "Blue"}]}]}}),
        ),
        ..Script::default()
    };
    let bots = bots_with(dir.path(), &server.origin, tasks.clone());
    let hook = format!("{}/hook", server.origin);
    bots.save_bot(&json!({"bot": bound_bot("wh-1", "webhook", "u1", json!({"webhookUrl": hook}))}))
        .await
        .unwrap();
    bots.provider_callback(
        "webhook",
        json!({"botId": "wh-1", "userId": "u1", "text": "go"}),
    )
    .await
    .unwrap();
    let request = eventually("elicitation request", || {
        server
            .to("/hook")
            .into_iter()
            .find(|request| request.body["type"] == "zcode.bot.elicitation_request")
    })
    .await;
    let elicitation = &request.body["elicitation"];
    assert_eq!(elicitation["requestId"], "ask-1");
    assert_eq!(elicitation["status"], "pending");
    assert_eq!(
        request.body["selection"]["options"][0],
        json!({"id": "red", "label": "Red"})
    );
    let token = request.body["selection"]["token"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        request.body["text"]
            .as_str()
            .unwrap()
            .contains(&format!("/elicitation {token} red"))
    );
    // 结构化回执直接提交。
    let result = bots
        .provider_callback(
            "webhook",
            json!({"botId": "wh-1", "userId": "u1", "type": "zcode.bot.elicitation_response",
                   "requestId": "ask-1", "action": "accept", "content": {"answer": "blue"}}),
        )
        .await
        .unwrap();
    assert_eq!(result["replies"][0]["text"], "已提交问答响应。");
    assert_eq!(result["replies"][0]["elicitation"]["status"], "completed");
    let resolved = tasks.resolved.lock().unwrap().clone();
    assert_eq!(
        resolved,
        vec![(
            "task-1".into(),
            "ask-1".into(),
            json!({"action": "accept", "content": {"answer": "blue"}})
        )]
    );
    eventually("continuation reply", || {
        server
            .to("/hook")
            .into_iter()
            .find(|request| request.body["text"] == "Thanks!")
    })
    .await;
    let state = bots.repo.read_state().await.unwrap();
    assert!(
        state["bots"]["wh-1"].get("pendingElicitation").is_none(),
        "{state}"
    );
}

#[tokio::test]
async fn weixin_registration_activation_and_numeric_selection() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let polls = Arc::new(Mutex::new(0u32));
    let updates = Arc::new(Mutex::new(VecDeque::from(vec![
        json!([{"message_type": 1, "from_user_id": "u1@im.wechat", "message_id": 1, "context_token": "ctx",
            "item_list": [{"type": 1, "text_item": {"text": "你好"}}]}]),
        json!([{"message_type": 1, "from_user_id": "u1@im.wechat", "message_id": 2, "context_token": "ctx",
            "item_list": [{"type": 1, "text_item": {"text": "/项目"}}]}]),
        json!([{"message_type": 1, "from_user_id": "u1@im.wechat", "message_id": 3, "context_token": "ctx",
            "item_list": [{"type": 1, "text_item": {"text": "2"}}]}]),
        json!([{"message_type": 1, "from_user_id": "u1@im.wechat", "message_id": 4, "context_token": "ctx",
            "item_list": [{"type": 1, "text_item": {"text": "run it"}}]}]),
    ])));
    let poll_counter = polls.clone();
    server.on(move |request| {
        Some(match request.path.as_str() {
            "/ilink/bot/get_bot_qrcode" => {
                assert_eq!(request.query, "bot_type=3");
                assert_eq!(request.headers["ilink-app-clientversion"], "1");
                Answer::json(200, json!({"ret": 0, "data": {"qrcode": "qr-1", "qrcode_img_content": "https://x/qr.png", "expires_in": 60}}))
            }
            "/ilink/bot/get_qrcode_status" => {
                let mut count = poll_counter.lock().unwrap();
                *count += 1;
                Answer::json(200, match *count {
                    1 => json!({"status": 0}),
                    2 => json!({"status": "scaned"}),
                    _ => json!({"data": {"status": 2, "bot_token": "wx-token", "ilink_bot_id": "bot@im.bot"}}),
                })
            }
            "/ilink/bot/getupdates" => {
                assert_eq!(request.headers["authorizationtype"], "ilink_bot_token");
                assert_eq!(request.headers["authorization"], "Bearer wx-token");
                assert_eq!(request.body["base_info"], json!({"channel_version": "2.0.0"}));
                let next = updates.lock().unwrap().pop_front();
                let mut answer = Answer::json(200, json!({"ret": 0, "msgs": next.unwrap_or(json!([])),
                    "get_updates_buf": format!("buf-{}", request.raw.len())}));
                answer.delay = Duration::from_millis(30);
                answer
            }
            "/ilink/bot/getconfig" => Answer::json(200, json!({"ret": 0, "typing_ticket": "tt"})),
            _ => Answer::json(200, json!({"ret": 0})),
        })
    });
    let tasks = FakeTasks::new();
    let bots = bots_with(dir.path(), &server.origin, tasks.clone());
    let begin = super::weixin::begin_registration(&bots).await.unwrap();
    assert_eq!(
        (
            begin["qrCode"].as_str(),
            begin["qrUrl"].as_str(),
            begin["interval"].as_i64()
        ),
        (Some("qr-1"), Some("https://x/qr.png"), Some(3))
    );
    let params = json!({"qrCode": "qr-1"});
    assert_eq!(
        super::weixin::poll_registration(&bots, &params)
            .await
            .unwrap(),
        json!({"status": "pending", "interval": 3})
    );
    assert_eq!(
        super::weixin::poll_registration(&bots, &params)
            .await
            .unwrap(),
        json!({"status": "scanned", "interval": 3})
    );
    let success = super::weixin::poll_registration(&bots, &params)
        .await
        .unwrap();
    assert_eq!(
        success,
        json!({"status": "success", "botToken": "wx-token", "botId": "bot@im.bot"})
    );

    // UI 拿到 token 后保存 Bot（微信没有用户 id，按 botId 授权）。
    bots.save_bot(&json!({"bot": {"id": "wx-1", "name": "WX", "provider": "weixin", "enabled": true,
        "providerUserId": "bot@im.bot", "allowedWorkspaces": ["*"], "allowedCommands": {}, "currentOptions": {},
        "webhookUrl": "https://example.invalid"}, "credentialValue": "wx-token"}))
        .await
        .unwrap();
    assert!(
        bots.repo.read_config().await.unwrap()["bots"][0]
            .get("webhookUrl")
            .is_none()
    );
    bots.refresh_polling(Polling::Weixin).await.unwrap();
    let texts = || -> Vec<String> {
        server
            .to("/sendmessage")
            .iter()
            .map(|request| {
                request.body["msg"]["item_list"][0]["text_item"]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect()
    };
    // 首条消息只做激活：欢迎语 + 帮助，CRLF 换行。
    let welcome = eventually("activation", || texts().into_iter().next()).await;
    assert!(welcome.starts_with("微信 Bot 已激活。"), "{welcome}");
    assert!(welcome.contains("\r\n"));
    let first = server.to("/sendmessage")[0].body["msg"].clone();
    assert_eq!(first["to_user_id"], "u1@im.wechat");
    assert_eq!(first["from_user_id"], "bot@im.bot");
    assert_eq!(first["context_token"], "ctx");
    assert_eq!(first["message_type"], 2);
    // 纯文本编号菜单，回复数字选择。
    let menu = eventually("workspace menu", || {
        texts()
            .into_iter()
            .find(|text| text.contains("回复数字选择"))
    })
    .await;
    assert_eq!(
        menu,
        "当前 workspace ws-bot\r\n选择 workspace\r\n0. 取消\r\n1. ws-bot\r\n2. ws-other\r\n\r\n回复数字选择，0 取消。"
    );
    let status = eventually("switched", || {
        texts()
            .into_iter()
            .find(|text| text.starts_with("工作区: ws-other"))
    })
    .await;
    assert!(status.contains("状态: 草稿"), "{status}");
    // 普通消息在新 workspace 建任务，回复以纯文本发送；typing 先取 ticket。
    eventually("task reply", || {
        texts().into_iter().find(|text| text == "Hi from the agent")
    })
    .await;
    assert_eq!(tasks.created.lock().unwrap()[0].0, "/tmp/ws-other");
    assert_eq!(
        last_body(&server, "/sendtyping").unwrap()["typing_ticket"],
        "tt"
    );
    // buf 在整批成功后提交。
    eventually("buf persisted", || {
        let state = std::fs::read_to_string(dir.path().join(".zcode/v2/bot-state.v3.json")).ok()?;
        let state: Value = serde_json::from_str(&state).ok()?;
        state["bots"]["wx-1"]["weixinGetUpdatesBuf"]
            .as_str()
            .map(str::to_owned)
    })
    .await;
    bots.dispose().await;
}
