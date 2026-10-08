//! Webhook 通道（providers/webhookProvider.ts）：入站由 `handleProviderCallback(Response)` 承载，
//! 出站 POST 到配置的 webhookUrl，按 0 / 500 / 1500 ms 重试。
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::{Map, Value, json};

use super::feishu::{Actor, Inbound};
use super::now_ms;
use super::providers::REQUEST_TIMEOUT;
use super::service::{Bots, guard};
use super::util::{non_empty, text};

/// 出站重试间隔（`postWebhookWithRetry`）。测试可缩短。
pub const RETRY_DELAYS_MS: [u64; 3] = [0, 500, 1_500];

/// `formatSelectionCommand`（Webhook 正文里列出的命令）。
fn selection_text(message: &Value) -> String {
    let body = text(message, "text").unwrap_or_default();
    let selection = &message["selection"];
    if !selection.is_object() {
        return body.to_owned();
    }
    let lines: Vec<String> = selection["options"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, option)| {
            let description = text(option, "description")
                .filter(|d| !d.is_empty())
                .map(|d| format!(" - {d}"))
                .unwrap_or_default();
            format!(
                "{}. {}{description}\n{}",
                index + 1,
                text(option, "label").unwrap_or_default(),
                super::providers::selection_command(selection, text(option, "id").unwrap_or_default())
            )
        })
        .collect();
    format!("{body}\n{}", lines.join("\n"))
}

/// `parseWebhookAttachment`。
fn attachment(value: &Value, index: usize) -> Option<Value> {
    let kind = text(value, "kind").filter(|k| matches!(*k, "image" | "audio" | "video" | "file"))?;
    let id = non_empty(value, "id")
        .map(str::to_owned)
        .unwrap_or_else(|| format!("webhook-{}", index + 1));
    let filename = non_empty(value, "filename")
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{id}.{kind}"));
    let mime = non_empty(value, "mimeType").unwrap_or("application/octet-stream");
    let mut out = Map::new();
    out.insert("id".into(), json!(id));
    out.insert("kind".into(), json!(kind));
    out.insert("filename".into(), json!(filename));
    out.insert("mimeType".into(), json!(mime));
    if value["sizeBytes"].is_number() {
        out.insert("sizeBytes".into(), value["sizeBytes"].clone());
    }
    for key in ["providerFileId", "downloadUrl", "dataBase64", "localPath"] {
        if let Some(field) = text(value, key) {
            out.insert(key.into(), json!(field));
        }
    }
    Some(Value::Object(out))
}

/// `parseWebhookElicitationResponse`。
fn elicitation_response(payload: &Value) -> Option<Value> {
    if payload["type"] != "zcode.bot.elicitation_response" {
        return None;
    }
    let request_id = non_empty(payload, "requestId")?;
    let action = text(payload, "action").filter(|a| matches!(*a, "accept" | "decline" | "cancel"))?;
    let mut out = Map::new();
    out.insert("requestId".into(), json!(request_id));
    out.insert("action".into(), json!(action));
    if payload["content"].is_object() {
        out.insert("content".into(), payload["content"].clone());
    }
    Some(Value::Object(out))
}

/// `parseCallback`。
pub fn parse_callback(payload: &Value) -> Vec<Inbound> {
    if !payload.is_object() {
        return Vec::new();
    }
    let bot_id = text(payload, "botId").unwrap_or_default();
    let body = text(payload, "text").unwrap_or_default();
    let user = text(payload, "userId").unwrap_or_default();
    let files: Vec<Value> = payload["attachments"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, item)| attachment(item, index))
        .collect();
    let response = elicitation_response(payload);
    if bot_id.is_empty() || (body.is_empty() && files.is_empty() && response.is_none()) || user.is_empty() {
        return Vec::new();
    }
    vec![Inbound {
        bot_id: bot_id.to_owned(),
        text: body.to_owned(),
        attachments: files,
        elicitation_response: response,
        actor: Actor {
            provider: "webhook".into(),
            bot_id: bot_id.to_owned(),
            provider_user_id: user.to_owned(),
            display_name: text(payload, "displayName").map(str::to_owned),
            chat_type: if payload["chatType"] == "group" {
                "group".into()
            } else {
                "private".into()
            },
            chat_id: text(payload, "chatId").map(str::to_owned),
            // webhook 回调也要带消息 id 进入通用幂等层：上游重试同一条消息时不重复建任务。
            provider_message_id: text(payload, "messageId")
                .or_else(|| text(payload, "id"))
                .map(str::to_owned),
            provider_context_token: None,
        },
    }]
}

/// `postWebhookWithRetry`：2xx 与 4xx 立即返回；5xx 与网络错误按间隔重试，最后一次的结果作数。
async fn post_with_retry(bots: &Bots, url: &str, headers: &[(String, String)], body: &Value) -> Result<(bool, u16)> {
    let delays = guard(&bots.webhook_retry_ms).clone();
    let mut last_status: Option<(bool, u16)> = None;
    let mut last_error: Option<anyhow::Error> = None;
    for delay in delays {
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        let mut request = bots
            .providers
            .http
            .client()
            .post(url)
            .header("content-type", "application/json")
            .body(body.to_string());
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        match bots.providers.http.status(request, REQUEST_TIMEOUT).await {
            Ok((ok, status)) if ok || (400..500).contains(&status) => return Ok((ok, status)),
            Ok(status) => last_status = Some(status),
            Err(error) => last_error = Some(error),
        }
    }
    match (last_status, last_error) {
        (Some(status), _) => Ok(status),
        (None, Some(error)) => Err(error),
        (None, None) => Err(anyhow!("Webhook outbound endpoint is unreachable.")),
    }
}

fn secret_header(bot: &Value) -> String {
    non_empty(bot, "webhookAuthHeaderName")
        .unwrap_or("x-zcode-bot-secret")
        .to_owned()
}

pub async fn test(bots: &Bots, bot: &Value) -> Result<Value> {
    if bot["enabled"] != true {
        return Ok(json!({"ok": false, "message": "Webhook bot is disabled."}));
    }
    let Some(secret_ref) = text(bot, "webhookSecretRef") else {
        return Ok(json!({"ok": false,
            "message": "Webhook secret is missing. Configure a secret before exposing the callback endpoint."}));
    };
    let Some(url) = non_empty(bot, "webhookUrl") else {
        return Ok(json!({"ok": true, "message": "Webhook bot is enabled for inbound callbacks."}));
    };
    let secret = bots.load_credential(Some(secret_ref)).await.unwrap_or_default();
    let body = json!({"type": "zcode.bot.test", "botId": bot["id"], "provider": "webhook", "sentAt": now_ms()});
    let (ok, status) = post_with_retry(bots, url, &[(secret_header(bot), secret)], &body).await?;
    Ok(json!({
        "ok": ok,
        "message": if ok {
            "Webhook outbound endpoint is reachable.".to_owned()
        } else {
            format!("Webhook outbound endpoint returned HTTP {status}.")
        },
    }))
}

/// `send`：没有 webhookUrl 时静默跳过；最终非 2xx 报错。
pub async fn send(bots: &Bots, bot: &Value, message: &Value) -> Result<()> {
    let Some(url) = non_empty(bot, "webhookUrl") else {
        return Ok(());
    };
    let mut headers = Vec::new();
    if let Some(secret) = bots
        .load_credential(text(bot, "webhookSecretRef"))
        .await
        .filter(|secret| !secret.is_empty())
    {
        headers.push((secret_header(bot), secret));
    }
    let elicitation = message.get("elicitation").filter(|e| !e.is_null());
    let mut body = Map::new();
    body.insert(
        "type".into(),
        json!(if elicitation.is_some() {
            "zcode.bot.elicitation_request"
        } else {
            "zcode.bot.message"
        }),
    );
    body.insert("botId".into(), bot["id"].clone());
    body.insert("provider".into(), json!("webhook"));
    body.insert("userId".into(), message["providerUserId"].clone());
    body.insert("text".into(), json!(selection_text(message)));
    if let Some(selection) = message.get("selection").filter(|s| !s.is_null()) {
        body.insert("selection".into(), selection.clone());
    }
    if let Some(elicitation) = elicitation {
        body.insert("elicitation".into(), elicitation.clone());
    }
    body.insert("sentAt".into(), json!(now_ms()));
    let (ok, status) = post_with_retry(bots, url, &headers, &Value::Object(body)).await?;
    if !ok {
        return Err(anyhow!("Webhook outbound endpoint returned HTTP {status}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_payloads_parse_like_the_provider() {
        let payload = json!({"botId": "w1", "userId": "u1", "text": "hello", "messageId": "m1",
            "attachments": [{"kind": "image", "dataBase64": "aGk="}, {"kind": "bogus"}]});
        let inbound = parse_callback(&payload).pop().unwrap();
        assert_eq!(inbound.actor.provider_message_id.as_deref(), Some("m1"));
        assert_eq!(inbound.attachments.len(), 1);
        assert_eq!(inbound.attachments[0]["id"], "webhook-1");
        assert_eq!(inbound.attachments[0]["filename"], "webhook-1.image");
        let answer = json!({"botId": "w1", "userId": "u1", "type": "zcode.bot.elicitation_response",
            "requestId": "r1", "action": "accept", "content": {"a": 1}});
        let inbound = parse_callback(&answer).pop().unwrap();
        assert_eq!(inbound.elicitation_response, Some(json!({"requestId": "r1", "action": "accept", "content": {"a": 1}})));
        assert!(parse_callback(&json!({"botId": "w1", "userId": "u1"})).is_empty());
        let text = selection_text(&json!({"text": "Pick", "selection": {"action": "task.set",
            "options": [{"id": "t1", "label": "One", "description": "running"}]}}));
        assert_eq!(text, "Pick\n1. One - running\n/task t1");
    }
}
