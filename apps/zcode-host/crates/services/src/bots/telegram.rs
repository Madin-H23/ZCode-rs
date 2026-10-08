//! Telegram 通道（providers/telegramProvider.ts + telegramChannelRuntime.ts 的轮询部分）。
//!
//! Bot API 基址可由 `ZCODE_TELEGRAM_API_BASE_URL` 覆盖（测试与 E2E 指向 127.0.0.1 mock）。
use anyhow::{Result, anyhow};
use serde_json::{Map, Value, json};

use super::feishu::{Actor, Inbound};
use super::providers::{Downloaded, Http, REQUEST_TIMEOUT, TypingTarget};
use super::service::Bots;
use super::util::{collapse_whitespace, js_len, js_slice, split_utf16, text};

/// Telegram 单条消息上限之内的切分长度（`splitTelegramText`）。
const MESSAGE_CHUNK: usize = 3900;

/// `BOT_MENU_COMMAND_ORDER` 中各命令在 Telegram 菜单里的名称与描述。
const COMMANDS: [(&str, &str, &str); 9] = [
    ("help", "help", "Show help"),
    ("status", "status", "Show current status"),
    ("new", "new", "Create a new task"),
    ("workspace", "project", "Select project"),
    ("model", "model", "Select model"),
    ("mode", "mode", "Select mode"),
    ("thoughtLevel", "think", "Select thinking level"),
    ("reply", "reply", "Select reply detail"),
    ("bind", "bind", "Bind this chat"),
];

fn url(base: &str, token: &str, method: &str) -> String {
    format!("{base}/bot{token}/{method}")
}

fn post(http: &Http, base: &str, token: &str, method: &str, body: &Value) -> reqwest::RequestBuilder {
    http.client()
        .post(url(base, token, method))
        .header("content-type", "application/json")
        .body(body.to_string())
}

fn id_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn display_name(from: &Value) -> Option<String> {
    text(from, "username")
        .or_else(|| text(from, "first_name"))
        .map(str::to_owned)
}

fn default_mime(kind: &str) -> &'static str {
    match kind {
        "image" => "image/jpeg",
        "audio" => "audio/mpeg",
        "video" => "video/mp4",
        _ => "application/octet-stream",
    }
}

/// `readTelegramFileAttachment`。
fn file_attachment(value: &Value, kind: &str, fallback: &str) -> Option<Value> {
    let file_id = text(value, "file_id").filter(|id| !id.is_empty())?;
    let filename = text(value, "file_name")
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(fallback);
    let mime = text(value, "mime_type")
        .filter(|mime| !mime.trim().is_empty())
        .unwrap_or_else(|| default_mime(kind));
    let mut out = Map::new();
    out.insert("id".into(), json!(file_id));
    out.insert("kind".into(), json!(kind));
    out.insert("filename".into(), json!(filename));
    out.insert("mimeType".into(), json!(mime));
    if value["file_size"].is_number() {
        out.insert("sizeBytes".into(), value["file_size"].clone());
    }
    out.insert("providerFileId".into(), json!(file_id));
    Some(Value::Object(out))
}

fn attachments(message: &Value) -> Vec<Value> {
    let photo = message["photo"]
        .as_array()
        .and_then(|photos| photos.iter().filter(|p| p.is_object()).next_back())
        .and_then(|photo| file_attachment(photo, "image", "telegram-photo.jpg"));
    [
        photo,
        file_attachment(&message["document"], "file", "telegram-document"),
        file_attachment(&message["video"], "video", "telegram-video.mp4"),
        file_attachment(&message["audio"], "audio", "telegram-audio"),
        file_attachment(&message["voice"], "audio", "telegram-voice.ogg"),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// `readTelegramPrivateMessage`。
fn private_message(bot_id: &str, update: &Value) -> Option<Inbound> {
    let message = update.get("message").filter(|m| m.is_object())?;
    let chat = &message["chat"];
    let from = &message["from"];
    let body = text(message, "text")
        .or_else(|| text(message, "caption"))
        .unwrap_or_default();
    let user = id_string(&from["id"]).unwrap_or_default();
    let files = attachments(message);
    if (body.is_empty() && files.is_empty()) || user.is_empty() {
        return None;
    }
    Some(Inbound {
        bot_id: bot_id.to_owned(),
        text: body.to_owned(),
        attachments: files,
        actor: Actor {
            provider: "telegram".into(),
            bot_id: bot_id.to_owned(),
            provider_user_id: user,
            display_name: display_name(from),
            chat_type: if chat["type"] == "private" {
                "private".into()
            } else {
                "group".into()
            },
            chat_id: id_string(&chat["id"]),
            provider_message_id: id_string(&message["message_id"]),
            provider_context_token: None,
        },
        elicitation_response: None,
    })
}

/// `decodeTelegramCallbackData`。
pub fn decode_callback_data(data: &str) -> String {
    if data == "zc:cancel" {
        return "/cancel".into();
    }
    if let Some(rest) = data.strip_prefix("zc:e:") {
        let mut parts = rest.split(':');
        return match (parts.next(), parts.next()) {
            (Some(token), Some(value)) if !token.is_empty() && !value.is_empty() => {
                format!("/elicitation {token} {value}")
            }
            _ => "/elicitation".into(),
        };
    }
    if let Some(command) = data.strip_prefix("zc:cmd:") {
        return command.to_owned();
    }
    if let Some(rest) = data.strip_prefix("zc:") {
        return format!("/{}", rest.replacen(':', " ", 1));
    }
    data.to_owned()
}

/// `buildSelectionCallbackData`：callback_data 最多 64 字节，只回传序号。
pub fn callback_data(selection: &Value, index: usize) -> String {
    let n = index + 1;
    match text(selection, "action").unwrap_or_default() {
        "permission.respond" => format!("zc:permission:{n}"),
        "elicitation.respond" => match text(selection, "token").filter(|t| !t.is_empty()) {
            Some(token) => format!("zc:e:{token}:{n}"),
            None => format!("zc:elicitation:{n}"),
        },
        "model.provider.set" => format!("zc:cmd:/model provider {n}"),
        "model.set" => format!("zc:cmd:/model model {n}"),
        action => format!("zc:{}:{n}", action.replacen(".set", "", 1)),
    }
}

/// `buildSelectionReplyMarkup`。
pub fn reply_markup(selection: &Value) -> Value {
    let mut rows: Vec<Value> = selection["options"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, option)| {
            json!([{"text": option["label"], "callback_data": callback_data(selection, index)}])
        })
        .collect();
    if selection["showCancel"] != false {
        let label = text(selection, "cancelLabel").unwrap_or("Cancel");
        rows.push(json!([{"text": label, "callback_data": "zc:cancel"}]));
    }
    json!({"inline_keyboard": rows})
}

/// `readTelegramCallbackMessage`。
fn callback_message(bot_id: &str, update: &Value) -> Option<Inbound> {
    let query = update.get("callback_query").filter(|q| q.is_object())?;
    let message = &query["message"];
    let chat = &message["chat"];
    let from = &query["from"];
    let data = text(query, "data").unwrap_or_default();
    let user = id_string(&from["id"]).unwrap_or_default();
    if data.is_empty() || user.is_empty() {
        return None;
    }
    Some(Inbound {
        bot_id: bot_id.to_owned(),
        text: decode_callback_data(data),
        actor: Actor {
            provider: "telegram".into(),
            bot_id: bot_id.to_owned(),
            provider_user_id: user,
            display_name: display_name(from),
            chat_type: if chat["type"] == "private" {
                "private".into()
            } else {
                "group".into()
            },
            chat_id: id_string(&chat["id"]),
            provider_message_id: text(query, "id")
                .map(str::to_owned)
                .or_else(|| id_string(&message["message_id"])),
            provider_context_token: None,
        },
        ..Inbound::default()
    })
}

/// `parseCallback`：payload 为 `{botId, update}`（轮询）或直接是 update。
pub fn parse_callback(payload: &Value) -> Vec<Inbound> {
    let bot_id = text(payload, "botId").unwrap_or_default();
    if bot_id.is_empty() {
        return Vec::new();
    }
    let update = if payload["update"].is_object() {
        &payload["update"]
    } else {
        payload
    };
    private_message(bot_id, update)
        .or_else(|| callback_message(bot_id, update))
        .into_iter()
        .collect()
}

/// `buildTelegramCommands`。
pub fn menu_commands(bot: &Value) -> Vec<Value> {
    COMMANDS
        .iter()
        .filter(|(command, _, _)| {
            matches!(*command, "help" | "bind") || bot["allowedCommands"][*command] != false
        })
        .map(|(_, name, description)| json!({"command": name, "description": description}))
        .collect()
}

async fn get_me(bots: &Bots, bot: &Value) -> Result<Option<Value>> {
    let Some(token) = bots.load_bot_secret(bot).await.filter(|t| !t.trim().is_empty()) else {
        return Ok(None);
    };
    let base = &bots.providers.endpoints.telegram;
    let response = bots
        .providers
        .http
        .json(
            bots.providers.http.client().get(url(base, &token, "getMe")),
            REQUEST_TIMEOUT,
        )
        .await?;
    if !response.ok {
        return Ok(None);
    }
    Ok(Some(if response.payload.is_null() {
        json!({})
    } else {
        response.payload
    }))
}

fn me_name(payload: &Value) -> Option<String> {
    text(&payload["result"], "first_name")
        .filter(|name| !name.is_empty())
        .or_else(|| text(&payload["result"], "username").filter(|name| !name.is_empty()))
        .map(str::to_owned)
}

pub async fn test(bots: &Bots, bot: &Value) -> Result<Value> {
    if text(bot, "credentialRef").is_none() {
        return Ok(json!({"ok": false, "message": "Telegram bot token is missing."}));
    }
    let Some(payload) = get_me(bots, bot).await? else {
        return Ok(json!({"ok": false, "message": "Telegram getMe failed."}));
    };
    let ok = payload["ok"] == true;
    let mut out = Map::new();
    out.insert("ok".into(), json!(ok));
    if let Some(name) = me_name(&payload) {
        out.insert("name".into(), json!(name));
    }
    let message = if ok {
        "Telegram bot is reachable.".to_owned()
    } else {
        text(&payload, "description")
            .unwrap_or("Telegram getMe failed.")
            .to_owned()
    };
    out.insert("message".into(), json!(message));
    Ok(Value::Object(out))
}

pub async fn resolve_name(bots: &Bots, bot: &Value) -> Result<Option<String>> {
    Ok(get_me(bots, bot)
        .await?
        .filter(|payload| payload["ok"] == true)
        .and_then(|payload| me_name(&payload)))
}

/// `syncCommands`：禁用的 Bot 删除菜单（失败忽略），启用的设置菜单。
pub async fn sync_commands(http: &Http, base: &str, token: &str, bot: &Value) -> Result<()> {
    if bot["enabled"] != true {
        let _ = http
            .status(post(http, base, token, "deleteMyCommands", &json!({})), REQUEST_TIMEOUT)
            .await;
        return Ok(());
    }
    http.status(
        post(
            http,
            base,
            token,
            "setMyCommands",
            &json!({"commands": menu_commands(bot)}),
        ),
        REQUEST_TIMEOUT,
    )
    .await
    .map(|_| ())
}

/// `send`：按 3900 码元切分；选项按钮只挂在最后一段；Markdown 被拒时去掉 parse_mode 重发。
pub async fn send(http: &Http, base: &str, token: &str, message: &Value) -> Result<()> {
    let markup = message
        .get("selection")
        .filter(|selection| selection.is_object())
        .map(reply_markup);
    let chunks = split_utf16(text(message, "text").unwrap_or_default(), MESSAGE_CHUNK);
    let last = chunks.len() - 1;
    for (index, chunk) in chunks.iter().enumerate() {
        let attach = (index == last).then_some(markup.as_ref()).flatten();
        let mut body = Map::new();
        body.insert("chat_id".into(), message["providerUserId"].clone());
        body.insert("text".into(), json!(chunk));
        body.insert("parse_mode".into(), json!("Markdown"));
        if let Some(markup) = attach {
            body.insert("reply_markup".into(), markup.clone());
        }
        let (ok, _) = http
            .status(
                post(http, base, token, "sendMessage", &Value::Object(body.clone())),
                REQUEST_TIMEOUT,
            )
            .await?;
        if !ok {
            // Markdown 对未闭合的 `_*[]()` 敏感：解析失败时退回纯文本重发，保证消息不丢。
            body.remove("parse_mode");
            http.status(
                post(http, base, token, "sendMessage", &Value::Object(body)),
                REQUEST_TIMEOUT,
            )
            .await?;
        }
    }
    Ok(())
}

pub async fn send_typing(http: &Http, base: &str, token: &str, target: &TypingTarget) -> Result<()> {
    http.status(
        post(
            http,
            base,
            token,
            "sendChatAction",
            &json!({"chat_id": target.provider_user_id, "action": "typing"}),
        ),
        REQUEST_TIMEOUT,
    )
    .await
    .map(|_| ())
}

fn toast(text: &str) -> String {
    let normalized = collapse_whitespace(text);
    if js_len(&normalized) > 180 {
        format!("{}...", js_slice(&normalized, 0, 177))
    } else {
        normalized
    }
}

/// `acknowledgeCallback`：answerCallbackQuery（toast 为首条回复），再清掉原消息的按钮。
pub async fn acknowledge(
    http: &Http,
    base: &str,
    token: &str,
    payload: &Value,
    text_value: &str,
) -> Result<()> {
    let update = if payload["update"].is_object() {
        &payload["update"]
    } else {
        payload
    };
    let query = &update["callback_query"];
    let Some(id) = text(query, "id") else {
        return Ok(());
    };
    let mut body = Map::new();
    body.insert("callback_query_id".into(), json!(id));
    if !text_value.trim().is_empty() {
        body.insert("text".into(), json!(toast(text_value)));
    }
    http.status(
        post(http, base, token, "answerCallbackQuery", &Value::Object(body)),
        REQUEST_TIMEOUT,
    )
    .await?;
    let chat_id = id_string(&query["message"]["chat"]["id"]).unwrap_or_default();
    let message_id = &query["message"]["message_id"];
    if !chat_id.is_empty() && message_id.is_number() && !text_value.trim().is_empty() {
        let _ = http
            .status(
                post(
                    http,
                    base,
                    token,
                    "editMessageReplyMarkup",
                    &json!({"chat_id": chat_id, "message_id": message_id, "reply_markup": {"inline_keyboard": []}}),
                ),
                REQUEST_TIMEOUT,
            )
            .await;
    }
    Ok(())
}

/// `downloadAttachment`：getFile 取 file_path，再下载 `<base>/file/bot<token>/<path>`。
pub async fn download(bots: &Bots, bot: &Value, attachment: &Value) -> Result<Option<Downloaded>> {
    let Some(token) = bots.load_bot_secret(bot).await.filter(|t| !t.trim().is_empty()) else {
        return Ok(None);
    };
    let Some(file_id) = text(attachment, "providerFileId") else {
        return Ok(None);
    };
    let http = &bots.providers.http;
    let base = &bots.providers.endpoints.telegram;
    let response = http
        .json(
            post(http, base, &token, "getFile", &json!({"file_id": file_id})),
            super::providers::ATTACHMENT_TIMEOUT,
        )
        .await
        .map_err(|error| {
            if error.to_string().contains("timed out") {
                anyhow!("Telegram file download timed out.")
            } else {
                error
            }
        })?;
    if !response.ok {
        return Err(anyhow!("Telegram getFile failed: HTTP {}", response.status));
    }
    let path = text(&response.payload["result"], "file_path").filter(|_| response.payload["ok"] == true);
    let Some(path) = path else {
        return Err(anyhow!(
            "{}",
            text(&response.payload, "description").unwrap_or("Telegram getFile did not return file_path.")
        ));
    };
    let data = http
        .bytes(
            http.client().get(format!("{base}/file/bot{token}/{path}")),
            "Telegram file download failed",
            "Telegram file download timed out.",
        )
        .await?;
    let mut attachment = attachment.clone();
    if response.payload["result"]["file_size"].is_number() {
        attachment["sizeBytes"] = response.payload["result"]["file_size"].clone();
    }
    Ok(Some(Downloaded { attachment, data }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_data_round_trips_like_node() {
        let selection = json!({"action": "workspace.set", "options": [{"id": "a", "label": "A"}]});
        assert_eq!(callback_data(&selection, 0), "zc:workspace:1");
        assert_eq!(decode_callback_data("zc:workspace:1"), "/workspace 1");
        assert_eq!(decode_callback_data("zc:cancel"), "/cancel");
        assert_eq!(decode_callback_data("zc:e:abc:2"), "/elicitation abc 2");
        assert_eq!(decode_callback_data("zc:e:abc"), "/elicitation");
        assert_eq!(decode_callback_data("zc:cmd:/model provider 2"), "/model provider 2");
        assert_eq!(decode_callback_data("zc:thoughtLevel:3"), "/thoughtLevel 3");
        let markup = reply_markup(&json!({"action": "model.set", "cancelLabel": "取消",
            "options": [{"id": "m", "label": "M"}]}));
        assert_eq!(
            markup,
            json!({"inline_keyboard": [[{"text": "M", "callback_data": "zc:cmd:/model model 1"}],
                [{"text": "取消", "callback_data": "zc:cancel"}]]})
        );
        let bot = json!({"allowedCommands": {"model": false, "reply": true}});
        let names: Vec<String> = menu_commands(&bot)
            .iter()
            .map(|c| c["command"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            names,
            ["help", "status", "new", "project", "mode", "think", "reply", "bind"]
        );
        assert_eq!(toast(&"字".repeat(200)).chars().count(), 180);
    }

    #[test]
    fn updates_parse_like_the_provider() {
        let payload = json!({"botId": "t1", "update": {"update_id": 5, "message": {
            "message_id": 9, "text": "hi", "chat": {"id": 42, "type": "private"},
            "from": {"id": 42, "first_name": "Ann"},
            "photo": [{"file_id": "small"}, {"file_id": "big", "file_size": 10}]}}});
        let inbound = parse_callback(&payload).pop().unwrap();
        assert_eq!(inbound.text, "hi");
        assert_eq!(inbound.actor.provider_user_id, "42");
        assert_eq!(inbound.actor.chat_id.as_deref(), Some("42"));
        assert_eq!(inbound.actor.display_name.as_deref(), Some("Ann"));
        assert_eq!(inbound.actor.provider_message_id.as_deref(), Some("9"));
        assert_eq!(inbound.attachments[0]["providerFileId"], "big");
        assert_eq!(inbound.attachments[0]["filename"], "telegram-photo.jpg");
        let callback = json!({"botId": "t1", "update": {"callback_query": {"id": "cq", "data": "zc:task:2",
            "from": {"id": 7, "username": "bob"}, "message": {"message_id": 3, "chat": {"id": 7, "type": "private"}}}}});
        let inbound = parse_callback(&callback).pop().unwrap();
        assert_eq!(inbound.text, "/task 2");
        assert_eq!(inbound.actor.provider_message_id.as_deref(), Some("cq"));
        assert!(parse_callback(&json!({"update": {}})).is_empty());
    }
}
