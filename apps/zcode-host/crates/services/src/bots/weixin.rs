//! 微信 iLink 通道（providers/weixinProvider.ts、weixinRegistration.ts）：扫码注册、长轮询消息、
//! 发送、typing 与 CDN 附件解密。
//!
//! iLink 基址可由 `ZCODE_WEIXIN_ILINK_BASE_URL` 覆盖（测试与 E2E 指向 127.0.0.1 mock）。
use std::time::Duration;

use anyhow::{Result, anyhow};
use base64::Engine;
use serde_json::{Map, Value, json};

use super::feishu::{Actor, Inbound};
use super::now_ms;
use super::providers::{Downloaded, REQUEST_TIMEOUT, TypingTarget};
use super::service::Bots;
use super::util::{encode_uri_component, random_u32, text, uuid_v4};

pub const DEFAULT_ILINK_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
const API_PREFIX: &str = "/ilink/bot";
const CHANNEL_VERSION: &str = "2.0.0";
const MESSAGE_TYPE_BOT: i64 = 2;
const MESSAGE_STATE_FINISH: i64 = 2;
/// 长轮询 getupdates 的截止时间。
pub const GET_UPDATES_TIMEOUT: Duration = Duration::from_secs(90);
const LOGIN_INTERVAL_SECONDS: i64 = 3;
const LOGIN_EXPIRE_SECONDS: i64 = 120;
const LOGIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const TOKEN_MISSING: &str = "Weixin iLink bot token is missing. Scan the Weixin login QR code first.";

fn read<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn number(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(Value::as_f64)
}

fn number_or_string(value: &Value, key: &str) -> String {
    match value.get(key) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

fn first(candidates: &[&str]) -> String {
    candidates
        .iter()
        .find(|value| !value.is_empty())
        .map(|value| (*value).to_owned())
        .unwrap_or_default()
}

fn inner(message: &Value) -> &Value {
    if message["msg"].is_object() {
        &message["msg"]
    } else if message["message"].is_object() {
        &message["message"]
    } else {
        &Value::Null
    }
}

fn item_list(message: &Value) -> Vec<Value> {
    message["item_list"]
        .as_array()
        .or_else(|| inner(message)["item_list"].as_array())
        .cloned()
        .unwrap_or_default()
}

fn item_text(item: &Value) -> String {
    if !item.is_object() {
        return String::new();
    }
    first(&[read(&item["text_item"], "text"), read(item, "text"), read(item, "content")])
}

/// `readWeixinText`。
fn message_text(message: &Value) -> String {
    let direct = first(&[read(message, "text"), read(message, "content"), read(message, "message")]);
    if !direct.is_empty() {
        return direct;
    }
    let joined: Vec<String> = item_list(message)
        .iter()
        .map(item_text)
        .filter(|text| !text.is_empty())
        .collect();
    if !joined.is_empty() {
        return joined.join("\n");
    }
    first(&[read(inner(message), "text"), read(inner(message), "content")])
}

fn mime_from_filename(filename: &str) -> &'static str {
    let lower = filename.to_lowercase();
    if lower.ends_with(".svg") {
        "image/svg+xml"
    } else if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else {
        ""
    }
}

fn default_mime(kind: &str) -> &'static str {
    match kind {
        "image" => "image/jpeg",
        "audio" => "audio/mpeg",
        "video" => "video/mp4",
        _ => "application/octet-stream",
    }
}

fn infer_kind(item: &Value, merged: &Value) -> &'static str {
    if item["image_item"].is_object() {
        return "image";
    }
    let explicit = first(&[
        read(merged, "kind"),
        read(merged, "media_type"),
        read(merged, "mediaType"),
        read(merged, "type_name"),
    ]);
    let mime = first(&[read(merged, "mime_type"), read(merged, "mimeType")]);
    let filename = first(&[read(merged, "filename"), read(merged, "file_name"), read(merged, "name")]);
    let normalized = format!("{explicit} {mime} {filename}").to_lowercase();
    let image_ext = [".svg", ".png", ".jpg", ".jpeg", ".gif", ".webp", ".heic", ".bmp"]
        .iter()
        .any(|ext| filename.to_lowercase().ends_with(ext));
    if normalized.contains("image")
        || normalized.contains("photo")
        || normalized.contains("picture")
        || image_ext
    {
        "image"
    } else if normalized.contains("audio") || normalized.contains("voice") {
        "audio"
    } else if normalized.contains("video") {
        "video"
    } else {
        "file"
    }
}

/// `readWeixinAttachmentItem`。
fn item_attachment(item: &Value, index: usize) -> Option<Value> {
    if !item.is_object() || !item_text(item).is_empty() {
        return None;
    }
    let media = ["image_item", "file_item", "video_item", "audio_item", "media_item"]
        .iter()
        .map(|key| &item[*key])
        .find(|value| value.is_object())
        .unwrap_or(item);
    let mut source = media.as_object().cloned().unwrap_or_default();
    if let Some(nested) = media["media"].as_object() {
        source.extend(nested.clone());
    }
    let source = Value::Object(source);
    let file_id = [
        "file_id",
        "fileId",
        "media_id",
        "mediaId",
        "id",
        "encrypt_query_param",
        "encryptQueryParam",
        "media",
        "md5",
    ]
    .iter()
    .map(|key| number_or_string(&source, key))
    .find(|value| !value.is_empty())
    .unwrap_or_default();
    let download = first(&[
        read(&source, "url"),
        read(&source, "download_url"),
        read(&source, "downloadUrl"),
        read(&source, "full_url"),
        read(&source, "fullUrl"),
    ]);
    let data = first(&[read(&source, "data_base64"), read(&source, "dataBase64"), read(&source, "base64")]);
    if file_id.is_empty() && download.is_empty() && data.is_empty() {
        return None;
    }
    let mut merged = item.as_object().cloned().unwrap_or_default();
    merged.extend(source.as_object().cloned().unwrap_or_default());
    let kind = infer_kind(item, &Value::Object(merged));
    let filename = {
        let named = first(&[read(&source, "filename"), read(&source, "file_name"), read(&source, "name")]);
        if !named.is_empty() {
            named
        } else if kind == "image" {
            format!("weixin-image-{}.jpg", index + 1)
        } else {
            format!("weixin-attachment-{}", index + 1)
        }
    };
    let mime = {
        let explicit = first(&[read(&source, "mime_type"), read(&source, "mimeType")]);
        if !explicit.is_empty() {
            explicit
        } else if !mime_from_filename(&filename).is_empty() {
            mime_from_filename(&filename).to_owned()
        } else {
            default_mime(kind).to_owned()
        }
    };
    let size = ["size", "sizeBytes", "file_size", "len", "mid_size"]
        .iter()
        .map(|key| &source[*key])
        .find(|value| value.is_number())
        .cloned();
    let aes = first(&[read(&source, "aes_key"), read(&source, "aesKey"), read(&source, "aeskey")]);
    let mut out = Map::new();
    let id = first(&[&file_id, &download]);
    out.insert(
        "id".into(),
        json!(if id.is_empty() {
            format!("weixin-{}", index + 1)
        } else {
            id
        }),
    );
    out.insert("kind".into(), json!(kind));
    out.insert("filename".into(), json!(filename));
    out.insert("mimeType".into(), json!(mime));
    if let Some(size) = size.filter(|size| size.as_f64() != Some(0.0)) {
        out.insert("sizeBytes".into(), size);
    }
    if !file_id.is_empty() {
        out.insert("providerFileId".into(), json!(file_id));
    }
    if !download.is_empty() {
        out.insert("downloadUrl".into(), json!(download));
    }
    if !data.is_empty() {
        out.insert("dataBase64".into(), json!(data));
    }
    if !aes.is_empty() {
        out.insert("providerMetadata".into(), json!({"weixinAesKey": aes}));
    }
    Some(Value::Object(out))
}

/// `readWeixinDirectAttachment`（回调包装后的 `attachments`）。
fn direct_attachment(item: &Value, index: usize) -> Option<Value> {
    let kind = read(item, "kind");
    if !matches!(kind, "image" | "audio" | "video" | "file") {
        return None;
    }
    let id = first(&[read(item, "id"), &format!("weixin-{}", index + 1)]);
    let filename = first(&[read(item, "filename"), &format!("{id}.{kind}")]);
    let mime = first(&[read(item, "mimeType"), read(item, "mime_type"), default_mime(kind)]);
    let mut out = Map::new();
    out.insert("id".into(), json!(id));
    out.insert("kind".into(), json!(kind));
    out.insert("filename".into(), json!(filename));
    out.insert("mimeType".into(), json!(mime));
    if let Some(size) = ["sizeBytes", "size"]
        .iter()
        .map(|key| &item[*key])
        .find(|value| value.is_number())
    {
        out.insert("sizeBytes".into(), size.clone());
    }
    for (key, alternatives) in [
        ("providerFileId", ["providerFileId", "file_id"]),
        ("downloadUrl", ["downloadUrl", "download_url"]),
        ("dataBase64", ["dataBase64", "data_base64"]),
    ] {
        let value = first(&[read(item, alternatives[0]), read(item, alternatives[1])]);
        if !value.is_empty() {
            out.insert(key.into(), json!(value));
        }
    }
    if let Some(metadata) = item["providerMetadata"].as_object() {
        let strings: Map<String, Value> = metadata
            .iter()
            .filter(|(_, value)| value.is_string())
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if !strings.is_empty() {
            out.insert("providerMetadata".into(), Value::Object(strings));
        }
    }
    Some(Value::Object(out))
}

fn attachments(message: &Value) -> Vec<Value> {
    let direct = message["attachments"]
        .as_array()
        .or_else(|| inner(message)["attachments"].as_array())
        .cloned()
        .unwrap_or_default();
    item_list(message)
        .iter()
        .enumerate()
        .filter_map(|(index, item)| item_attachment(item, index))
        .chain(
            direct
                .iter()
                .enumerate()
                .filter_map(|(index, item)| direct_attachment(item, index)),
        )
        .collect()
}

fn user_id(message: &Value) -> String {
    first(&[
        read(message, "from_user_id"),
        read(message, "from"),
        read(message, "from_user"),
        read(message, "fromUser"),
        read(message, "user"),
        read(message, "user_id"),
        read(message, "userId"),
        read(&message["from"], "id"),
        read(&message["from"], "wxid"),
        read(&message["sender"], "id"),
        read(&message["sender"], "wxid"),
    ])
}

fn chat_id(message: &Value) -> Option<String> {
    let value = first(&[
        read(message, "room"),
        read(message, "room_id"),
        read(message, "roomId"),
        read(message, "chat"),
        read(message, "chat_id"),
        read(message, "chatId"),
    ]);
    (!value.is_empty()).then_some(value)
}

fn display_name(message: &Value) -> Option<String> {
    let value = first(&[
        read(message, "name"),
        read(message, "displayName"),
        read(message, "nickname"),
        read(&message["from"], "name"),
        read(&message["from"], "nickname"),
        read(&message["sender"], "name"),
        read(&message["sender"], "nickname"),
        read(inner(message), "sender_name"),
    ]);
    (!value.is_empty()).then_some(value)
}

fn message_id(message: &Value) -> Option<String> {
    let inner = inner(message);
    let value = first(&[
        read(message, "id"),
        read(message, "msgid"),
        read(message, "msgId"),
        &number_or_string(message, "message_id"),
        read(inner, "id"),
        read(inner, "msgid"),
        read(inner, "msgId"),
        &number_or_string(inner, "message_id"),
    ]);
    if !value.is_empty() {
        return Some(value);
    }
    ["id", "msgid", "msgId"]
        .iter()
        .find_map(|key| message[*key].as_number().or_else(|| inner[*key].as_number()))
        .map(ToString::to_string)
}

fn context_token(message: &Value) -> Option<String> {
    let value = first(&[
        read(message, "context_token"),
        read(message, "contextToken"),
        read(message, "context"),
        read(inner(message), "context_token"),
    ]);
    (!value.is_empty()).then_some(value)
}

fn container(payload: &Value) -> &Value {
    if payload.get("data").is_some() {
        &payload["data"]
    } else {
        payload
    }
}

/// `readWeixinMessages`。
fn raw_messages(payload: &Value) -> Vec<Value> {
    let container = container(payload);
    let raw = ["msgs", "messages", "updates", "items", "list"]
        .iter()
        .map(|key| &container[*key])
        .find(|value| !value.is_null())
        .unwrap_or(&Value::Null);
    match raw {
        Value::Array(items) => items.iter().filter(|item| item.is_object()).cloned().collect(),
        Value::Object(_) => vec![raw.clone()],
        _ => Vec::new(),
    }
}

/// `readNextBuf`。
fn next_buf(payload: &Value) -> Option<String> {
    let container = container(payload);
    let value = first(&[
        read(container, "get_updates_buf"),
        read(container, "buf"),
        read(container, "next_buf"),
        read(container, "nextBuf"),
        read(container, "getUpdatesBuf"),
        read(container, "syncKey"),
    ]);
    (!value.is_empty()).then_some(value)
}

/// `buildInboundMessage`：Bot 自己的消息（message_type 2）丢弃。
fn inbound(bot_id: &str, raw: &Value) -> Option<Inbound> {
    if raw["message_type"].as_f64() == Some(2.0) {
        return None;
    }
    let body = message_text(raw).trim().to_owned();
    let files = attachments(raw);
    let user = user_id(raw).trim().to_owned();
    if (body.is_empty() && files.is_empty()) || user.is_empty() {
        return None;
    }
    let chat = chat_id(raw);
    Some(Inbound {
        bot_id: bot_id.to_owned(),
        text: body,
        attachments: files,
        actor: Actor {
            provider: "weixin".into(),
            bot_id: bot_id.to_owned(),
            provider_user_id: user,
            display_name: display_name(raw),
            chat_type: if chat.is_some() {
                "group".into()
            } else {
                "private".into()
            },
            chat_id: chat,
            provider_message_id: message_id(raw),
            provider_context_token: context_token(raw),
        },
        elicitation_response: None,
    })
}

pub fn parse_callback(payload: &Value) -> Vec<Inbound> {
    let bot_id = read(payload, "botId");
    if bot_id.is_empty() {
        return Vec::new();
    }
    raw_messages(payload)
        .iter()
        .filter_map(|message| inbound(bot_id, message))
        .collect()
}

/// 轮询结果（`getWeixinUpdates`）。
pub struct Updates {
    pub messages: Vec<Inbound>,
    pub raw_count: usize,
    pub buf: Option<String>,
}

fn headers(request: reqwest::RequestBuilder, token: &str) -> reqwest::RequestBuilder {
    let uin = base64::engine::general_purpose::STANDARD.encode(random_u32().to_string());
    request
        .header("content-type", "application/json")
        .header("AuthorizationType", "ilink_bot_token")
        .header("Authorization", format!("Bearer {token}"))
        .header("X-WECHAT-UIN", uin)
}

/// `requestWeixinJson`：带 base_info；`ret` / `errcode` 非 0 视为失败。
async fn request(
    bots: &Bots,
    token: &str,
    path: &str,
    body: Value,
    timeout: Duration,
) -> Result<Value> {
    let mut object = Map::new();
    object.insert("base_info".into(), json!({"channel_version": CHANNEL_VERSION}));
    object.extend(body.as_object().cloned().unwrap_or_default());
    let url = format!("{}{API_PREFIX}{path}", bots.providers.endpoints.weixin);
    let response = bots
        .providers
        .http
        .json(
            headers(bots.providers.http.client().post(url), token.trim())
                .body(Value::Object(object).to_string()),
            timeout,
        )
        .await?;
    if !response.ok {
        return Err(anyhow!("Weixin iLink {path} failed: HTTP {}", response.status));
    }
    let payload = response.payload;
    let ret = number(&payload, "ret");
    let errcode = number(&payload, "errcode");
    if ret.is_some_and(|v| v != 0.0) || errcode.is_some_and(|v| v != 0.0) {
        let show = |value: Option<f64>| value.map(|v| (v as i64).to_string()).unwrap_or_default();
        let message = first(&[
            read(&payload, "errmsg"),
            read(&payload, "message"),
            format!("ret={} errcode={}", show(ret), show(errcode)).trim(),
        ]);
        return Err(anyhow!("Weixin iLink {path} failed: {message}"));
    }
    Ok(payload)
}

async fn token(bots: &Bots, bot: &Value) -> Result<String> {
    bots.load_bot_secret(bot)
        .await
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| anyhow!("{TOKEN_MISSING}"))
}

pub async fn get_updates(bots: &Bots, bot: &Value, buf: Option<&str>) -> Result<Updates> {
    let token = token(bots, bot).await?;
    let payload = request(
        bots,
        &token,
        "/getupdates",
        json!({"get_updates_buf": buf.unwrap_or_default()}),
        GET_UPDATES_TIMEOUT,
    )
    .await?;
    let raw = raw_messages(&payload);
    let bot_id = read(bot, "id");
    Ok(Updates {
        raw_count: raw.len(),
        messages: raw.iter().filter_map(|message| inbound(bot_id, message)).collect(),
        buf: next_buf(&payload).or_else(|| buf.map(str::to_owned)),
    })
}

pub async fn test(bots: &Bots, bot: &Value) -> Result<Value> {
    if bot["enabled"] != true {
        return Ok(json!({"ok": false, "message": "Weixin bot is disabled."}));
    }
    if text(bot, "credentialRef").is_none() {
        return Ok(json!({"ok": false, "message": TOKEN_MISSING}));
    }
    let token = token(bots, bot).await?;
    request(bots, &token, "/getconfig", json!({}), REQUEST_TIMEOUT).await?;
    Ok(json!({"ok": true, "message": "Weixin iLink API is reachable."}))
}

/// `send`：文本放进 msg.item_list，换行统一成 CRLF。
pub async fn send(bots: &Bots, bot: &Value, message: &Value) -> Result<()> {
    let token = token(bots, bot).await?;
    let body_text = read(message, "text")
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "\r\n");
    let mut msg = Map::new();
    msg.insert("from_user_id".into(), json!(read(bot, "providerUserId")));
    msg.insert("to_user_id".into(), message["providerUserId"].clone());
    msg.insert("client_id".into(), json!(format!("zcode-weixin-{}", uuid_v4())));
    msg.insert("message_type".into(), json!(MESSAGE_TYPE_BOT));
    msg.insert("message_state".into(), json!(MESSAGE_STATE_FINISH));
    if let Some(context) = text(message, "providerContextToken") {
        msg.insert("context_token".into(), json!(context));
    }
    msg.insert(
        "item_list".into(),
        json!([{"type": 1, "text_item": {"text": body_text}}]),
    );
    request(bots, &token, "/sendmessage", json!({"msg": msg}), REQUEST_TIMEOUT)
        .await
        .map(|_| ())
}

/// `sendTyping`：先 getconfig 取 typing_ticket，再 sendtyping。
pub async fn send_typing(bots: &Bots, bot: &Value, target: &TypingTarget) -> Result<()> {
    let token = token(bots, bot).await?;
    let mut body = Map::new();
    body.insert("ilink_user_id".into(), json!(target.provider_user_id));
    if let Some(context) = &target.provider_context_token {
        body.insert("context_token".into(), json!(context));
    }
    let config = request(bots, &token, "/getconfig", Value::Object(body), REQUEST_TIMEOUT).await?;
    let Some(ticket) = text(&config, "typing_ticket").filter(|ticket| !ticket.is_empty()) else {
        return Ok(());
    };
    request(
        bots,
        &token,
        "/sendtyping",
        json!({"ilink_user_id": target.provider_user_id, "typing_ticket": ticket, "status": 1}),
        REQUEST_TIMEOUT,
    )
    .await
    .map(|_| ())
}

/// `parseWeixinAesKey`：32 位十六进制，或 base64（16 字节，或内容是 32 位十六进制）。
fn aes_key(value: &str) -> Option<[u8; 16]> {
    let trimmed = value.trim();
    let from_hex = |hex: &str| -> Option<[u8; 16]> {
        if hex.len() != 32 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let mut out = [0u8; 16];
        for (index, slot) in out.iter_mut().enumerate() {
            *slot = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
        }
        Some(out)
    };
    if let Some(key) = from_hex(trimmed) {
        return Some(key);
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(trimmed))
        .ok()?;
    if decoded.len() == 16 {
        return decoded.try_into().ok();
    }
    from_hex(String::from_utf8_lossy(&decoded).trim())
}

/// `downloadAttachment`：只处理带 AES key 的 CDN 附件，下载后 AES-128-ECB 解密。
pub async fn download(bots: &Bots, attachment: &Value) -> Result<Option<Downloaded>> {
    let (Some(url), Some(key)) = (
        text(attachment, "downloadUrl"),
        text(&attachment["providerMetadata"], "weixinAesKey"),
    ) else {
        return Ok(None);
    };
    let encrypted = bots
        .providers
        .http
        .bytes(
            bots.providers.http.client().get(url),
            "Weixin attachment download failed",
            "Weixin attachment download timed out.",
        )
        .await?;
    let key = aes_key(key).ok_or_else(|| anyhow!("Weixin attachment AES key is invalid."))?;
    Ok(Some(Downloaded {
        attachment: attachment.clone(),
        data: super::aes::decrypt_ecb_pkcs7(&encrypted, &key)?,
    }))
}

// ── 扫码注册（weixinRegistration.ts） ──

/// `normalizeQrStatus`。
fn qr_status(status: &Value) -> &'static str {
    if let Some(number) = status.as_f64() {
        return match number as i64 {
            0 => "pending",
            1 => "scanned",
            2 => "success",
            3 | 4 => "expired",
            _ => "pending",
        };
    }
    let Some(text) = status.as_str() else {
        return "pending";
    };
    match text.to_lowercase().as_str() {
        "confirmed" | "confirm" | "authorized" | "success" | "ok" => "success",
        "scaned" | "scanned" | "scan" | "confirmed_wait" => "scanned",
        "expired" | "timeout" | "cancel" | "cancelled" | "canceled" => "expired",
        "error" | "failed" | "fail" => "error",
        _ => "pending",
    }
}

/// 登录接口的 GET（`getWeixinRegistrationJson`）：`data` 摊平到顶层。
async fn registration_json(bots: &Bots, path: &str) -> Result<Value> {
    let url = format!("{}{API_PREFIX}{path}", bots.providers.endpoints.weixin);
    let response = bots
        .providers
        .http
        .client()
        .get(url)
        .header("iLink-App-ClientVersion", "1")
        .timeout(LOGIN_REQUEST_TIMEOUT)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(anyhow!(
            "Weixin login {path} failed: HTTP {}",
            response.status().as_u16()
        ));
    }
    let payload: Value = response.json().await?;
    let mut merged = payload.as_object().cloned().unwrap_or_default();
    if let Some(data) = payload["data"].as_object() {
        merged.extend(data.clone());
    }
    let merged = Value::Object(merged);
    let ret = number(&merged, "ret");
    let errcode = number(&merged, "errcode");
    if ret.is_some_and(|v| v != 0.0) || errcode.is_some_and(|v| v != 0.0) {
        let show = |value: Option<f64>| value.map(|v| (v as i64).to_string()).unwrap_or_default();
        let message = first(&[
            read(&merged, "errmsg"),
            format!("ret={} errcode={}", show(ret), show(errcode)).trim(),
        ]);
        return Err(anyhow!("{message}"));
    }
    Ok(merged)
}

/// `beginWeixinRegistration`。
pub async fn begin_registration(bots: &Bots) -> Result<Value> {
    let payload = registration_json(bots, "/get_bot_qrcode?bot_type=3").await?;
    let qr_code = first(&[read(&payload, "qrcode"), read(&payload, "qr_code")]);
    let qr_url = first(&[
        read(&payload, "qrcode_img_content"),
        read(&payload, "qrcode_url"),
        &qr_code,
    ]);
    if qr_code.is_empty() || qr_url.is_empty() {
        return Err(anyhow!("Weixin login did not return a QR code."));
    }
    let expire = number(&payload, "expires_in").unwrap_or(LOGIN_EXPIRE_SECONDS as f64);
    Ok(json!({
        "qrCode": qr_code,
        "qrUrl": qr_url,
        "interval": LOGIN_INTERVAL_SECONDS,
        "expiresAt": now_ms() + (expire * 1000.0) as i64,
    }))
}

/// `pollWeixinRegistration`：请求超时视为仍在等待手机确认。
pub async fn poll_registration(bots: &Bots, params: &Value) -> Result<Value> {
    let qr_code = read(params, "qrCode");
    let path = format!("/get_qrcode_status?qrcode={}", encode_uri_component(qr_code));
    let payload = match registration_json(bots, &path).await {
        Ok(payload) => payload,
        Err(error)
            if error
                .downcast_ref::<reqwest::Error>()
                .is_some_and(reqwest::Error::is_timeout) =>
        {
            return Ok(json!({"status": "pending", "interval": LOGIN_INTERVAL_SECONDS}));
        }
        Err(error) => return Err(error),
    };
    let raw_status = ["status", "qrcode_status", "qr_status"]
        .iter()
        .map(|key| &payload[*key])
        .find(|value| !value.is_null())
        .unwrap_or(&Value::Null);
    Ok(match qr_status(raw_status) {
        "success" => {
            let token = first(&[read(&payload, "bot_token"), read(&payload, "token")]);
            if token.is_empty() {
                json!({"status": "error", "message": "Weixin login succeeded but did not return bot_token."})
            } else {
                let mut out = Map::new();
                out.insert("status".into(), json!("success"));
                out.insert("botToken".into(), json!(token));
                let bot_id = first(&[read(&payload, "ilink_bot_id"), read(&payload, "bot_id")]);
                if !bot_id.is_empty() {
                    out.insert("botId".into(), json!(bot_id));
                }
                Value::Object(out)
            }
        }
        "expired" => json!({"status": "expired"}),
        "error" => json!({"status": "error",
            "message": first(&[read(&payload, "errmsg"), "Weixin login failed."])}),
        status => json!({"status": status, "interval": LOGIN_INTERVAL_SECONDS}),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weixin_messages_parse_like_the_provider() {
        let payload = json!({"botId": "wx1", "data": {"msgs": [
            {"message_type": 2, "from_user_id": "self", "item_list": [{"type": 1, "text_item": {"text": "echo"}}]},
            {"message_type": 1, "from_user_id": "u1@im.wechat", "message_id": 77, "context_token": "ctx",
             "item_list": [{"type": 1, "text_item": {"text": "你好"}},
                {"type": 2, "image_item": {"media": {"encrypt_query_param": "q", "aes_key": "k", "full_url": "http://x/img"}, "mid_size": 12}}]},
        ], "get_updates_buf": "buf-2"}});
        let messages = parse_callback(&payload);
        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert_eq!(message.text, "你好");
        assert_eq!(message.actor.provider_message_id.as_deref(), Some("77"));
        assert_eq!(message.actor.provider_context_token.as_deref(), Some("ctx"));
        assert_eq!(message.actor.chat_type, "private");
        assert_eq!(message.attachments[0]["kind"], "image");
        assert_eq!(message.attachments[0]["filename"], "weixin-image-2.jpg");
        assert_eq!(message.attachments[0]["downloadUrl"], "http://x/img");
        assert_eq!(message.attachments[0]["providerMetadata"]["weixinAesKey"], "k");
        assert_eq!(message.attachments[0]["sizeBytes"], 12);
        assert_eq!(next_buf(&payload).as_deref(), Some("buf-2"));
        assert_eq!(qr_status(&json!(1)), "scanned");
        assert_eq!(qr_status(&json!("CONFIRMED")), "success");
        assert_eq!(qr_status(&json!("canceled")), "expired");
        assert_eq!(qr_status(&Value::Null), "pending");
        let hex = "000102030405060708090a0b0c0d0e0f";
        assert_eq!(aes_key(hex), Some(std::array::from_fn(|i| i as u8)));
        let b64 = base64::engine::general_purpose::STANDARD.encode(hex);
        assert_eq!(aes_key(&b64), Some(std::array::from_fn(|i| i as u8)));
    }
}
