//! 飞书 / Lark 通道（providers/feishuProvider.ts 中长连接、消息解析与卡片发送的部分）。
pub mod api;
pub mod cards;
pub mod frame;
pub mod provider;
pub mod ws;

use serde_json::{Map, Value, json};

/// `BotInboundMessage` 的 Rust 形状。附件是 `BotInboundAttachment` JSON（只有元数据，处理时下载）。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Inbound {
    pub bot_id: String,
    pub text: String,
    pub actor: Actor,
    pub attachments: Vec<Value>,
    /// `BotStructuredElicitationResponse`（Webhook 的结构化问答回执）。
    pub elicitation_response: Option<Value>,
}

impl Inbound {
    pub fn has_attachments(&self) -> bool {
        !self.attachments.is_empty()
    }
}

/// `BotActor`。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Actor {
    pub provider: String,
    pub bot_id: String,
    pub provider_user_id: String,
    pub display_name: Option<String>,
    pub chat_type: String,
    pub chat_id: Option<String>,
    pub provider_message_id: Option<String>,
    /// 微信 iLink 的 context_token：回复与 typing 都要带上。
    pub provider_context_token: Option<String>,
}

impl Actor {
    /// 回复目标：群聊用 chatId，私聊用用户 id（`createOutbound`）。
    pub fn reply_target(&self) -> &str {
        self.chat_id.as_deref().unwrap_or(&self.provider_user_id)
    }
}

fn read<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn first_non_empty<'a>(candidates: &[&'a str]) -> &'a str {
    candidates
        .iter()
        .copied()
        .find(|text| !text.is_empty())
        .unwrap_or_default()
}

/// `parseJsonRecord`。
fn json_record(value: &Value) -> Value {
    match value {
        Value::Object(_) => value.clone(),
        Value::String(text) => serde_json::from_str::<Value>(text)
            .ok()
            .filter(Value::is_object)
            .unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

fn chat_type(value: &str) -> String {
    if value == "group" || value == "group_chat" {
        "group".into()
    } else {
        "private".into()
    }
}

/// `stripFeishuMentions`：去掉 `<at ...>...</at>` 与 `@xxx`。
pub fn strip_mentions(text: &str) -> String {
    static AT_TAG: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?is)<at\b[^>]*>.*?</at>").expect("valid regex")
    });
    static AT_NAME: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"@\S+").expect("valid regex"));
    let without_tags = AT_TAG.replace_all(text, "");
    AT_NAME.replace_all(&without_tags, "").trim().to_owned()
}

fn post_locale<'a>(content: &'a Value, field: &str) -> &'a Value {
    let post = &content["post"];
    let zh = if content["zh_cn"].is_object() {
        &content["zh_cn"]
    } else {
        &post["zh_cn"]
    };
    let en = if content["en_us"].is_object() {
        &content["en_us"]
    } else {
        &post["en_us"]
    };
    if field == "content" {
        if let Some(value) = content.get("content").filter(|value| !value.is_null()) {
            return value;
        }
        if zh.is_object() && !zh["content"].is_null() {
            return &zh["content"];
        }
        return &en["content"];
    }
    &Value::Null
}

fn post_title(content: &Value) -> String {
    let post = &content["post"];
    let zh = if content["zh_cn"].is_object() {
        &content["zh_cn"]
    } else {
        &post["zh_cn"]
    };
    let en = if content["en_us"].is_object() {
        &content["en_us"]
    } else {
        &post["en_us"]
    };
    first_non_empty(&[read(content, "title"), read(zh, "title"), read(en, "title")]).to_owned()
}

/// `formatFeishuPostToken`。
fn post_token(token: &Value) -> String {
    match token {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(post_token)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(""),
        Value::Object(_) => {
            let tag = read(token, "tag");
            if tag == "at" {
                return String::new();
            }
            let nested = ["content", "children", "elements"]
                .iter()
                .map(|key| &token[*key])
                .find(|value| !value.is_null());
            let nested_text = match nested {
                Some(Value::Array(_)) => post_token(nested.unwrap_or(&Value::Null)),
                _ => String::new(),
            };
            let text = first_non_empty(&[
                read(token, "text"),
                read(token, "un_escape_text"),
                read(token, "name"),
                &nested_text,
            ])
            .to_owned();
            if tag == "a" {
                let href = read(token, "href");
                if !href.is_empty() && href != text {
                    return if text.is_empty() {
                        href.to_owned()
                    } else {
                        format!("{text} {href}")
                    };
                }
            }
            text
        }
        _ => String::new(),
    }
}

/// `readFeishuPostText`。
fn post_text(content: &Value) -> String {
    if !content.is_object() {
        return String::new();
    }
    let lines: Vec<String> = match post_locale(content, "content") {
        Value::Array(lines) => lines
            .iter()
            .map(|line| post_token(line).trim().to_owned())
            .filter(|line| !line.is_empty())
            .collect(),
        _ => Vec::new(),
    };
    let title = post_title(content).trim().to_owned();
    let body = lines.join("\n").trim().to_owned();
    match (title.is_empty(), body.is_empty()) {
        (false, false) => format!("{title}\n{body}"),
        (_, false) => body,
        _ => title,
    }
}

/// `readFeishuAttachment`。
fn attachment(message_type: &str, content: &Value) -> Option<Value> {
    if !content.is_object() {
        return None;
    }
    let file_id = first_non_empty(&[
        read(content, "image_key"),
        read(content, "file_key"),
        read(content, "media_key"),
        read(content, "audio_key"),
        read(content, "key"),
    ]);
    if file_id.is_empty() {
        return None;
    }
    let kind = match message_type {
        "image" => "image",
        "audio" => "audio",
        "media" | "video" => "video",
        _ => "file",
    };
    let short: String = file_id.chars().take(8).collect();
    let fallback = format!("{message_type}-{short}");
    let filename = first_non_empty(&[
        read(content, "file_name"),
        read(content, "filename"),
        &fallback,
    ])
    .to_owned();
    let mime = first_non_empty(&[read(content, "mime_type"), read(content, "mimeType")]);
    let mime = if mime.is_empty() {
        match kind {
            "image" => "image/jpeg",
            "audio" => "audio/mpeg",
            "video" => "video/mp4",
            _ => "application/octet-stream",
        }
    } else {
        mime
    };
    let mut out = Map::new();
    out.insert("id".into(), json!(file_id));
    out.insert("kind".into(), json!(kind));
    out.insert("filename".into(), json!(filename));
    out.insert("mimeType".into(), json!(mime));
    if content["size"].is_number() {
        out.insert("sizeBytes".into(), content["size"].clone());
    }
    out.insert("providerFileId".into(), json!(file_id));
    Some(Value::Object(out))
}

fn provider_of(payload: &Value) -> String {
    if read(payload, "zcodeProvider") == "lark" {
        "lark".into()
    } else {
        "feishu".into()
    }
}

/// `readFeishuTextMessage`：长连接事件可能把 event 字段摊平到顶层。
fn text_message(bot_id: &str, payload: &Value) -> Option<Inbound> {
    let event = if payload["event"].is_object() {
        &payload["event"]
    } else {
        payload
    };
    let message = &event["message"];
    let sender_id = &event["sender"]["sender_id"];
    let content = json_record(&message["content"]);
    let message_type = first_non_empty(&[read(message, "message_type"), read(message, "msg_type")]);
    let attachment = attachment(message_type, &content);
    let post = post_text(&content);
    let raw = first_non_empty(&[
        read(&content, "text"),
        &post,
        read(event, "text_without_at_bot"),
        read(event, "text"),
    ])
    .to_owned();
    let text = strip_mentions(&raw);
    let user = first_non_empty(&[
        read(sender_id, "open_id"),
        read(sender_id, "user_id"),
        read(sender_id, "union_id"),
        read(event, "open_id"),
        read(event, "user_id"),
        read(event, "union_id"),
    ]);
    let chat_id = first_non_empty(&[read(message, "chat_id"), read(event, "open_chat_id")]);
    let message_id = read(message, "message_id");
    if (text.is_empty() && attachment.is_none()) || user.is_empty() {
        return None;
    }
    let kind = chat_type(first_non_empty(&[
        read(message, "chat_type"),
        read(event, "chat_type"),
    ]));
    Some(Inbound {
        bot_id: bot_id.to_owned(),
        text,
        attachments: attachment.into_iter().collect(),
        actor: Actor {
            provider: provider_of(payload),
            bot_id: bot_id.to_owned(),
            provider_user_id: user.to_owned(),
            chat_id: (kind == "group" && !chat_id.is_empty()).then(|| chat_id.to_owned()),
            chat_type: kind,
            provider_message_id: (!message_id.is_empty()).then(|| message_id.to_owned()),
            ..Actor::default()
        },
        ..Inbound::default()
    })
}

/// `readFeishuCardAction`（不含问答表单的拼装）。
fn card_action(bot_id: &str, payload: &Value) -> Option<Inbound> {
    let event = if payload["event"].is_object() {
        &payload["event"]
    } else {
        payload
    };
    let action = &event["action"];
    let behavior_value = action["behaviors"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item.is_object()))
        .map(|item| &item["value"])
        .unwrap_or(&Value::Null);
    let raw_command = first_non_empty(&[
        read(&action["value"], "command"),
        read(&action["value"], "text"),
        read(behavior_value, "command"),
        read(behavior_value, "text"),
    ]);
    // 问答表单提交：按钮命令只带 token，答案在 form_value.answer 里（buildFeishuElicitationFormCommand）。
    let command = if raw_command.contains(cards::ELICITATION_FORM_VALUE_PREFIX) {
        let token = raw_command.split_whitespace().nth(1).unwrap_or_default();
        let answer = action["form_value"]
            .get(cards::ELICITATION_FORM_FIELD_NAME)
            .filter(|_| action["form_value"].is_object());
        cards::elicitation_form_command(token, answer)
    } else {
        raw_command.to_owned()
    };
    let command = command.as_str();
    let operator = &event["operator"];
    let open_id = first_non_empty(&[
        read(&operator["operator_id"], "open_id"),
        read(operator, "open_id"),
        read(event, "open_id"),
        read(event, "user_id"),
        read(payload, "open_id"),
        read(payload, "user_id"),
    ]);
    let context = &event["context"];
    let kind = chat_type(first_non_empty(&[
        read(context, "chat_type"),
        read(event, "chat_type"),
        read(payload, "chat_type"),
    ]));
    let chat_id = first_non_empty(&[read(context, "open_chat_id"), read(context, "chat_id")]);
    let message_id = first_non_empty(&[
        read(event, "event_id"),
        read(&payload["header"], "event_id"),
        read(payload, "uuid"),
        read(payload, "event_id"),
        read(context, "open_message_id"),
        read(context, "message_id"),
        read(&event["message"], "message_id"),
        read(action, "value_id"),
    ]);
    if command.is_empty() || open_id.is_empty() {
        return None;
    }
    Some(Inbound {
        bot_id: bot_id.to_owned(),
        text: command.to_owned(),
        actor: Actor {
            provider: provider_of(payload),
            bot_id: bot_id.to_owned(),
            provider_user_id: open_id.to_owned(),
            chat_id: (kind == "group" && !chat_id.is_empty()).then(|| chat_id.to_owned()),
            chat_type: kind,
            provider_message_id: (!message_id.is_empty()).then(|| message_id.to_owned()),
            ..Actor::default()
        },
        ..Inbound::default()
    })
}

/// `parseCallback`：payload 带 `botId`、`zcodeProvider`（长连接处理器注入）。
pub fn parse_callback(payload: &Value) -> Option<Inbound> {
    let bot_id = read(payload, "botId");
    if bot_id.is_empty() {
        return None;
    }
    text_message(bot_id, payload).or_else(|| card_action(bot_id, payload))
}

/// `formatFeishuCardMarkdownContent`：代码块外的单换行转成硬换行，`---` 前后补空行。
pub fn card_markdown(text: &str) -> String {
    struct Line {
        text: String,
        hard_break: bool,
    }
    let mut lines: Vec<Line> = Vec::new();
    let mut in_fence = false;
    let is_rule = |line: &str| line.len() >= 3 && line.bytes().all(|byte| byte == b'-');
    for line in text.split('\n') {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if !in_fence && is_rule(line.trim()) {
            if lines.last().is_some_and(|last| !last.text.is_empty()) {
                lines.push(Line {
                    text: String::new(),
                    hard_break: false,
                });
            }
            lines.push(Line {
                text: "---".into(),
                hard_break: false,
            });
            lines.push(Line {
                text: String::new(),
                hard_break: false,
            });
            continue;
        }
        lines.push(Line {
            text: line.to_owned(),
            hard_break: !in_fence,
        });
    }
    let count = lines.len();
    lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let next_empty = lines.get(index + 1).is_none_or(|next| next.text.is_empty());
            if line.text.is_empty() || !line.hard_break || index + 1 >= count || next_empty {
                line.text.clone()
            } else {
                format!("{}  ", line.text)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `buildFeishuInteractiveCardPayload`（无选择项时）。
pub fn interactive_card(text: &str) -> Value {
    json!({
        "schema": "2.0",
        "config": {"wide_screen_mode": true},
        "body": {"elements": [{"tag": "markdown", "content": card_markdown(text)}]},
    })
}

/// `splitFeishuText`：按 1900 个 UTF-16 码元切分。
pub fn split_text(text: &str) -> Vec<String> {
    const LIMIT: usize = 1900;
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.is_empty() {
        return vec![text.to_owned()];
    }
    units.chunks(LIMIT).map(String::from_utf16_lossy).collect()
}

/// 长连接处理器给事件补上的字段（`createFeishuWebSocketEventHandlers`）。
pub fn callback_payload(bot_id: &str, provider: &str, event: Value) -> Value {
    let mut out = Map::new();
    out.insert("botId".into(), json!(bot_id));
    out.insert("zcodeProvider".into(), json!(provider));
    match event {
        Value::Object(map) => out.extend(map),
        other => {
            out.insert("payload".into(), other);
        }
    }
    Value::Object(out)
}
