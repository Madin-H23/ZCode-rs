//! 飞书 / Lark 的通道适配（`createFeishuBotProvider`）：发送、流式卡片、typing reaction、
//! 卡片回调确认、昵称与附件下载。HTTP 细节在 [`super::api`]，卡片结构在 [`super::cards`]。
use anyhow::Result;
use serde_json::{Map, Value, json};

use super::api::AppCredentials;
use super::cards::{self, StreamState};
use super::{Actor, split_text};
use crate::bots::now_ms;
use crate::bots::providers::{Downloaded, TypingTarget};
use crate::bots::service::{Bots, guard};
use crate::bots::util::text;

const DISPLAY_NAME_TTL_MS: i64 = 10 * 60_000;

fn read<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn first<'a>(candidates: &[&'a str]) -> &'a str {
    candidates
        .iter()
        .copied()
        .find(|value| !value.is_empty())
        .unwrap_or_default()
}

fn callback_event(payload: &Value) -> &Value {
    if payload["event"].is_object() {
        &payload["event"]
    } else {
        payload
    }
}

/// `readFeishuCardUpdateToken`：Card JSON 2.0 回调的更新 token 可能出现在事件、payload、action 或 context 上。
pub fn card_update_token(payload: &Value) -> String {
    let event = callback_event(payload);
    let action = &event["action"];
    let context = &event["context"];
    let keys = ["token", "card_update_token", "update_token"];
    [event, payload, action, context]
        .iter()
        .flat_map(|source| keys.iter().map(move |key| read(source, key)))
        .find(|value| !value.is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// `readFeishuCardUpdateOpenIds`。
pub fn card_update_open_ids(payload: &Value) -> Vec<String> {
    let event = callback_event(payload);
    let operator = &event["operator"];
    let open_id = first(&[
        read(&operator["operator_id"], "open_id"),
        read(operator, "open_id"),
        read(event, "open_id"),
        read(payload, "open_id"),
    ]);
    if open_id.is_empty() {
        Vec::new()
    } else {
        vec![open_id.to_owned()]
    }
}

/// `readFeishuCardOriginalText`。
pub fn card_original_text(payload: &Value) -> Option<String> {
    let event = callback_event(payload);
    let action = &event["action"];
    let value = &action["value"];
    let behavior = action["behaviors"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item.is_object()))
        .map(|item| &item["value"])
        .unwrap_or(&Value::Null);
    let text = first(&[
        read(value, "zcodeCardText"),
        read(value, "cardText"),
        read(behavior, "zcodeCardText"),
        read(behavior, "cardText"),
    ]);
    (!text.is_empty()).then(|| text.to_owned())
}

impl Bots {
    fn feishu_app<'a>(bot: &'a Value, secret: &'a str) -> AppCredentials<'a> {
        AppCredentials {
            provider: text(bot, "provider").unwrap_or("feishu"),
            app_id: text(bot, "feishuAppId").unwrap_or_default(),
            credential_ref: text(bot, "credentialRef").unwrap_or_default(),
            app_secret: secret,
        }
    }

    /// `trackDelivery(sendFeishuInteractiveCard)`：投递结果单独记入 deliveryError。
    async fn feishu_send_card(
        &self,
        bot: &Value,
        token: &str,
        receive_id: &str,
        card: &Value,
    ) -> Result<Option<String>> {
        let provider = text(bot, "provider").unwrap_or("feishu");
        let result = self
            .providers
            .feishu
            .send_card(provider, token, receive_id, card)
            .await;
        self.on_delivery_result(bot, result.as_ref().err().map(ToString::to_string));
        result
    }

    async fn feishu_update_card_tracked(
        &self,
        bot: &Value,
        token: &str,
        message_id: &str,
        card: &Value,
    ) -> Result<()> {
        let provider = text(bot, "provider").unwrap_or("feishu");
        let result = self
            .providers
            .feishu
            .update_card(provider, token, message_id, card)
            .await;
        self.on_delivery_result(bot, result.as_ref().err().map(ToString::to_string));
        result
    }

    /// `send`：带选择的消息发一张交互 / 问答卡片；普通回复按 1900 个 UTF-16 码元分卡。token 缺失时静默跳过。
    pub(crate) async fn feishu_send(&self, bot: &Value, message: &Value) -> Result<()> {
        let Some(token) = self.tenant_token(bot).await? else {
            return Ok(());
        };
        let receive_id = text(message, "providerUserId").unwrap_or_default();
        if message["selection"].is_object() {
            self.feishu_send_card(bot, &token, receive_id, &cards::for_message(message))
                .await?;
            return Ok(());
        }
        for chunk in split_text(text(message, "text").unwrap_or_default()) {
            let mut plain = message.as_object().cloned().unwrap_or_default();
            plain.insert("text".into(), json!(chunk));
            plain.remove("selection");
            let card = cards::interactive(&Value::Object(plain));
            self.feishu_send_card(bot, &token, receive_id, &card)
                .await?;
        }
        Ok(())
    }

    /// `createStreamingReplyCard`：返回 message_id。
    pub(crate) async fn feishu_create_stream_card(
        &self,
        bot: &Value,
        receive_id: &str,
        state: &StreamState,
    ) -> Result<Option<String>> {
        let Some(token) = self.tenant_token(bot).await? else {
            return Ok(None);
        };
        self.feishu_send_card(bot, &token, receive_id, &cards::streaming(state))
            .await
    }

    /// `updateStreamingReplyCard` / `updateTransientInteractionCard`：PATCH 同一条消息。
    pub(crate) async fn feishu_update_card(
        &self,
        bot: &Value,
        message_id: &str,
        card: &Value,
    ) -> Result<()> {
        let Some(token) = self.tenant_token(bot).await? else {
            return Ok(());
        };
        self.feishu_update_card_tracked(bot, &token, message_id, card)
            .await
    }

    /// `createTransientInteractionCard`。
    pub(crate) async fn feishu_create_card(
        &self,
        bot: &Value,
        message: &Value,
    ) -> Result<Option<String>> {
        let Some(token) = self.tenant_token(bot).await? else {
            return Ok(None);
        };
        let receive_id = text(message, "providerUserId").unwrap_or_default();
        self.feishu_send_card(bot, &token, receive_id, &cards::for_message(message))
            .await
    }

    pub(crate) async fn feishu_test(&self, bot: &Value) -> Result<Value> {
        Ok(if bot["enabled"] != true {
            json!({"ok": false, "message": "Feishu bot is disabled."})
        } else if text(bot, "feishuAppId").is_none() || text(bot, "credentialRef").is_none() {
            json!({"ok": false, "message": "Feishu App ID and App Secret are required."})
        } else {
            match self.tenant_token(bot).await? {
                Some(_) => json!({"ok": true, "message": "Feishu app credentials are valid."}),
                None => json!({"ok": false, "message": "Feishu app credentials are missing."}),
            }
        })
    }

    /// `resolveName`：应用名称（`readFeishuAppDisplayName`）。
    pub(crate) async fn feishu_resolve_name(&self, bot: &Value) -> Result<Option<String>> {
        let Some(secret) = self.feishu_secret(bot).await else {
            return Ok(None);
        };
        self.providers
            .feishu
            .read_app_display_name(&Self::feishu_app(bot, &secret))
            .await
    }

    /// `resolveActorDisplayName`：通讯录昵称，结果（含空）缓存 10 分钟。
    pub(crate) async fn feishu_user_display_name(
        &self,
        bot: &Value,
        user_id: &str,
    ) -> Result<Option<String>> {
        let user_id = user_id.trim();
        if user_id.is_empty() {
            return Ok(None);
        }
        let provider = if text(bot, "provider") == Some("lark") {
            "lark"
        } else {
            "feishu"
        };
        let key = format!(
            "{provider}:{}:{}:{user_id}",
            text(bot, "feishuAppId").unwrap_or_default(),
            text(bot, "credentialRef").unwrap_or_default()
        );
        if let Some((name, expires_at)) = guard(&self.providers.display_names).get(&key).cloned()
            && expires_at > now_ms()
        {
            return Ok(name);
        }
        let Some(token) = self.tenant_token(bot).await? else {
            return Ok(None);
        };
        let name = self
            .providers
            .feishu
            .user_name(provider, &token, user_id)
            .await?;
        guard(&self.providers.display_names)
            .insert(key, (name.clone(), now_ms() + DISPLAY_NAME_TTL_MS));
        Ok(name)
    }

    /// `addFeishuTypingReaction`：同一条入站消息只加一次。
    pub(crate) async fn feishu_add_typing(&self, bot: &Value, target: &TypingTarget) -> Result<()> {
        let Some(message_id) = target.provider_message_id.as_deref() else {
            return Ok(());
        };
        let Some(token) = self.tenant_token(bot).await? else {
            return Ok(());
        };
        let key = format!("{}:{message_id}", text(bot, "id").unwrap_or_default());
        if guard(&self.providers.typing_reactions).contains_key(&key) {
            return Ok(());
        }
        let provider = text(bot, "provider").unwrap_or("feishu");
        if let Some(reaction) = self
            .providers
            .feishu
            .add_reaction(provider, &token, message_id)
            .await?
        {
            guard(&self.providers.typing_reactions).insert(key, reaction);
        }
        Ok(())
    }

    /// `deleteFeishuTypingReaction`。
    pub(crate) async fn feishu_delete_typing(
        &self,
        bot: &Value,
        target: &TypingTarget,
    ) -> Result<()> {
        let Some(message_id) = target.provider_message_id.as_deref() else {
            return Ok(());
        };
        let key = format!("{}:{message_id}", text(bot, "id").unwrap_or_default());
        let Some(reaction) = guard(&self.providers.typing_reactions).get(&key).cloned() else {
            return Ok(());
        };
        let Some(token) = self.tenant_token(bot).await? else {
            return Ok(());
        };
        let provider = text(bot, "provider").unwrap_or("feishu");
        self.providers
            .feishu
            .delete_reaction(provider, &token, message_id, &reaction)
            .await?;
        guard(&self.providers.typing_reactions).remove(&key);
        Ok(())
    }

    /// `acknowledgeCallback`：用回调的 card update token 原地更新原卡片（去掉按钮，或推进问答）。
    pub(crate) async fn feishu_acknowledge(
        &self,
        bot: &Value,
        payload: &Value,
        text_value: &str,
        message: Option<&Value>,
    ) -> Result<bool> {
        let token = self.tenant_token(bot).await?;
        let update_token = card_update_token(payload);
        let elicitation = message.is_some_and(|message| message["elicitation"].is_object());
        let Some(token) = token else {
            return Ok(false);
        };
        if update_token.is_empty() || (text_value.trim().is_empty() && !elicitation) {
            return Ok(false);
        }
        let card = match message {
            Some(message) if elicitation => cards::elicitation(message),
            _ => {
                let original = card_original_text(payload)
                    .map(|text| text.trim().to_owned())
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| text_value.to_owned());
                cards::interactive(&json!({
                    "botId": bot["id"], "provider": bot["provider"], "providerUserId": "", "text": original,
                }))
            }
        };
        let mut body = Map::new();
        body.insert("token".into(), json!(update_token));
        let open_ids = card_update_open_ids(payload);
        if !open_ids.is_empty() {
            body.insert("open_ids".into(), json!(open_ids));
        }
        body.insert("card".into(), card);
        let provider = text(bot, "provider").unwrap_or("feishu");
        self.providers
            .feishu
            .card_update(provider, &token, &Value::Object(body))
            .await?;
        Ok(elicitation)
    }

    /// `downloadAttachment`：按入站消息 id 下载资源。
    pub(crate) async fn feishu_download(
        &self,
        bot: &Value,
        attachment: &Value,
        actor: &Actor,
    ) -> Result<Option<Downloaded>> {
        let (Some(file_key), Some(message_id)) = (
            text(attachment, "providerFileId"),
            actor.provider_message_id.as_deref(),
        ) else {
            return Ok(None);
        };
        let Some(token) = self.tenant_token(bot).await? else {
            return Ok(None);
        };
        let provider = text(bot, "provider").unwrap_or("feishu");
        let data = self
            .providers
            .feishu
            .resource(
                provider,
                &token,
                message_id,
                file_key,
                text(attachment, "kind").unwrap_or("file"),
            )
            .await?;
        Ok(Some(Downloaded {
            attachment: attachment.clone(),
            data,
        }))
    }
}
