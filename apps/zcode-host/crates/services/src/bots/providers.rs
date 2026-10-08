//! 通道适配的公共部分（providers/types.ts、providerRequest.ts）：基址、有界的出站 HTTP 请求，
//! 以及按通道分派的发送 / typing / 回调确认 / 测试 / 名称 / 附件下载 / 入站解析。
//!
//! 每个出站请求都有截止时间，且截止覆盖到读完响应体（`fetchBotProvider`）：第三方接口悬挂不能
//! 占住聊天上下文的串行队列。
use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use zcode_host_kit::log;

use super::feishu::Inbound;
use super::model::is_feishu;
use super::service::Bots;
use super::util::text;
use super::{feishu, telegram, webhook, weixin};

/// `BOT_PROVIDER_REQUEST_TIMEOUT_MS`。
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// `BOT_ATTACHMENT_DOWNLOAD_TIMEOUT_MS` 及各通道附件下载的截止时间。
pub const ATTACHMENT_TIMEOUT: Duration = Duration::from_secs(30);

/// Telegram Bot API、微信 iLink 的基址（飞书见 [`feishu::api::Endpoints`]）。生产使用官方域名；
/// `ZCODE_TELEGRAM_API_BASE_URL`、`ZCODE_WEIXIN_ILINK_BASE_URL` 只供测试与 E2E 指向本机 mock。
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub feishu: feishu::api::Endpoints,
    pub telegram: String,
    pub weixin: String,
}

fn pick(vars: &BTreeMap<String, String>, key: &str, default: &str) -> String {
    vars.get(key)
        .map(|value| value.trim().trim_end_matches('/').to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_owned())
}

impl Endpoints {
    pub fn from_vars(vars: &BTreeMap<String, String>) -> Self {
        Self {
            feishu: feishu::api::Endpoints::from_vars(vars),
            telegram: pick(vars, "ZCODE_TELEGRAM_API_BASE_URL", "https://api.telegram.org"),
            weixin: pick(
                vars,
                "ZCODE_WEIXIN_ILINK_BASE_URL",
                weixin::DEFAULT_ILINK_BASE_URL,
            ),
        }
    }

    /// 所有通道都指向同一个 origin（测试用）。
    pub fn local(origin: &str) -> Self {
        Self {
            feishu: feishu::api::Endpoints {
                feishu_open: origin.to_owned(),
                lark_open: origin.to_owned(),
                feishu_accounts: origin.to_owned(),
                lark_accounts: origin.to_owned(),
            },
            telegram: origin.to_owned(),
            weixin: origin.to_owned(),
        }
    }
}

/// `fetchBotProviderJson` 的结果。
#[derive(Debug, Clone)]
pub struct JsonResponse {
    pub ok: bool,
    pub status: u16,
    pub payload: Value,
}

/// 出站 HTTP：每个请求自带截止时间（覆盖读完响应体）。
#[derive(Clone)]
pub struct Http {
    client: reqwest::Client,
}

impl Http {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: crate::support::http::client(REQUEST_TIMEOUT)?,
        })
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// `fetchBotProviderJson`：2xx 时响应体必须是合法 JSON（空体为 null）；非 2xx 时非法 JSON 视为无载荷。
    pub async fn json(
        &self,
        request: reqwest::RequestBuilder,
        timeout: Duration,
    ) -> Result<JsonResponse> {
        let work = async {
            let response = request.timeout(timeout).send().await?;
            let status = response.status();
            let body = response.text().await?;
            let payload = if body.is_empty() {
                Value::Null
            } else {
                match serde_json::from_str(&body) {
                    Ok(value) => value,
                    Err(error) if status.is_success() => return Err(anyhow!(error)),
                    Err(_) => Value::Null,
                }
            };
            Ok(JsonResponse {
                ok: status.is_success(),
                status: status.as_u16(),
                payload,
            })
        };
        tokio::time::timeout(timeout, work)
            .await
            .map_err(|_| request_timeout(timeout))?
    }

    /// `fetchBotProvider`：只关心状态码，响应体在同一截止时间内读完丢弃。
    pub async fn status(&self, request: reqwest::RequestBuilder, timeout: Duration) -> Result<(bool, u16)> {
        let work = async {
            let response = request.timeout(timeout).send().await?;
            let status = response.status();
            let _ = response.bytes().await?;
            Ok((status.is_success(), status.as_u16()))
        };
        tokio::time::timeout(timeout, work)
            .await
            .map_err(|_| request_timeout(timeout))?
    }

    /// 下载二进制（附件）：非 2xx 报 `<label> failed: HTTP <status>`，超时报 `<label> timed out.`。
    pub async fn bytes(
        &self,
        request: reqwest::RequestBuilder,
        failed: &str,
        timed_out: &str,
    ) -> Result<Vec<u8>> {
        let work = async {
            let response = request.timeout(ATTACHMENT_TIMEOUT).send().await?;
            if !response.status().is_success() {
                return Err(anyhow!("{failed}: HTTP {}", response.status().as_u16()));
            }
            Ok(response.bytes().await?.to_vec())
        };
        match tokio::time::timeout(ATTACHMENT_TIMEOUT, work).await {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(error)) if is_timeout(&error) => Err(anyhow!("{timed_out}")),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(anyhow!("{timed_out}")),
        }
    }
}

fn request_timeout(timeout: Duration) -> anyhow::Error {
    anyhow!(
        "Bot provider request timed out after {}ms.",
        timeout.as_millis()
    )
}

fn is_timeout(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<reqwest::Error>()
        .is_some_and(reqwest::Error::is_timeout)
}

/// 各通道共用的出站能力与进程内缓存（Node 里是各 provider 模块的模块级 Map）。
pub struct Providers {
    pub endpoints: Endpoints,
    pub http: Http,
    pub feishu: feishu::api::FeishuApi,
    /// 飞书 Typing reaction：`<botId>:<messageId>` → reaction_id（`typingReactionIds`）。
    pub(crate) typing_reactions: std::sync::Mutex<std::collections::HashMap<String, String>>,
    /// 飞书通讯录昵称缓存（10 分钟，`userDisplayNameCache`）。
    pub(crate) display_names:
        std::sync::Mutex<std::collections::HashMap<String, (Option<String>, i64)>>,
}

impl Providers {
    pub fn new(endpoints: Endpoints) -> Result<Self> {
        Ok(Self {
            feishu: feishu::api::FeishuApi::new(endpoints.feishu.clone())?,
            http: Http::new()?,
            endpoints,
            typing_reactions: Default::default(),
            display_names: Default::default(),
        })
    }
}

/// typing 的目标（`BotTypingTarget`）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TypingTarget {
    pub provider_user_id: String,
    pub provider_message_id: Option<String>,
    pub provider_context_token: Option<String>,
}

/// 原生支持的 typing 方式：飞书用 reaction（start/stop），Telegram 与微信每 4 秒重发一次。
pub fn typing_is_reaction(provider: &str) -> bool {
    is_feishu(provider)
}

/// 已下载的附件（`BotProviderDownloadedAttachment`）。
pub struct Downloaded {
    pub attachment: Value,
    pub data: Vec<u8>,
}

impl Bots {
    pub(crate) async fn load_bot_secret(&self, bot: &Value) -> Option<String> {
        self.load_credential(text(bot, "credentialRef")).await
    }

    /// `adapter.parseCallback`。未知通道没有解析器（Node 的 discord / wecom 为 null）。
    pub(crate) fn parse_callback(provider: &str, payload: &Value) -> Option<Vec<Inbound>> {
        match provider {
            "telegram" => Some(telegram::parse_callback(payload)),
            "webhook" => Some(webhook::parse_callback(payload)),
            "weixin" => Some(weixin::parse_callback(payload)),
            "feishu" | "lark" => Some(feishu::parse_callback(payload).into_iter().collect()),
            _ => None,
        }
    }

    /// `sendOutbound` → `adapter.send`。没有适配器的通道静默跳过。
    pub(crate) async fn send_outbound(&self, bot: &Value, message: &Value) -> Result<()> {
        match text(bot, "provider").unwrap_or_default() {
            "telegram" => {
                let Some(token) = self.load_bot_secret(bot).await.filter(|t| !t.trim().is_empty())
                else {
                    return Ok(());
                };
                telegram::send(&self.providers.http, &self.providers.endpoints.telegram, &token, message).await
            }
            "webhook" => webhook::send(self, bot, message).await,
            "weixin" => weixin::send(self, bot, message).await,
            "feishu" | "lark" => self.feishu_send(bot, message).await,
            _ => Ok(()),
        }
    }

    /// `adapter.sendTyping`：一次性 typing（短命令与 Telegram / 微信的周期重发）。失败静默。
    pub(crate) async fn send_typing(&self, bot: &Value, target: &TypingTarget) {
        let result = match text(bot, "provider").unwrap_or_default() {
            "telegram" => match self.load_bot_secret(bot).await.filter(|t| !t.trim().is_empty()) {
                Some(token) => {
                    telegram::send_typing(&self.providers.http, &self.providers.endpoints.telegram, &token, target)
                        .await
                }
                None => Ok(()),
            },
            "weixin" => weixin::send_typing(self, bot, target).await,
            "feishu" | "lark" => self.feishu_add_typing(bot, target).await,
            _ => Ok(()),
        };
        if let Err(error) = result {
            log::debug(format!(
                "bot typing failed bot={}: {error:#}",
                text(bot, "id").unwrap_or_default()
            ));
        }
    }

    /// `adapter.stopTyping`（只有飞书：删除 Typing reaction）。失败静默。
    pub(crate) async fn stop_typing_reaction(&self, bot: &Value, target: &TypingTarget) {
        if is_feishu(text(bot, "provider").unwrap_or_default())
            && let Err(error) = self.feishu_delete_typing(bot, target).await
        {
            log::debug(format!(
                "bot stop typing failed bot={}: {error:#}",
                text(bot, "id").unwrap_or_default()
            ));
        }
    }

    /// `adapter.test`。
    pub(crate) async fn provider_test(&self, bot: &Value) -> Result<Option<Value>> {
        Ok(Some(match text(bot, "provider").unwrap_or_default() {
            "telegram" => telegram::test(self, bot).await?,
            "webhook" => webhook::test(self, bot).await?,
            "weixin" => weixin::test(self, bot).await?,
            "feishu" | "lark" => self.feishu_test(bot).await?,
            _ => return Ok(None),
        }))
    }

    /// `adapter.resolveName`（只有 Telegram 与飞书 / Lark 有）。
    pub(crate) async fn provider_resolve_name(&self, bot: &Value) -> Result<Option<String>> {
        match text(bot, "provider").unwrap_or_default() {
            "telegram" => telegram::resolve_name(self, bot).await,
            "feishu" | "lark" => self.feishu_resolve_name(bot).await,
            _ => Ok(None),
        }
    }

    /// `adapter.resolveActorDisplayName`（只有飞书）。
    pub(crate) async fn provider_actor_display_name(
        &self,
        bot: &Value,
        user_id: &str,
    ) -> Result<Option<String>> {
        if is_feishu(text(bot, "provider").unwrap_or_default()) {
            self.feishu_user_display_name(bot, user_id).await
        } else {
            Ok(None)
        }
    }

    /// `adapter.acknowledgeCallback`：返回 true 表示原卡片已由 card/update 承载（问答推进）。
    pub(crate) async fn provider_acknowledge(
        &self,
        bot: &Value,
        payload: &Value,
        text_value: &str,
        message: Option<&Value>,
    ) -> Result<bool> {
        match text(bot, "provider").unwrap_or_default() {
            "telegram" => {
                let Some(token) = self.load_bot_secret(bot).await.filter(|t| !t.trim().is_empty())
                else {
                    return Ok(false);
                };
                telegram::acknowledge(
                    &self.providers.http,
                    &self.providers.endpoints.telegram,
                    &token,
                    payload,
                    text_value,
                )
                .await
                .map(|()| false)
            }
            "feishu" | "lark" => self.feishu_acknowledge(bot, payload, text_value, message).await,
            _ => Ok(false),
        }
    }

    /// `adapter.downloadAttachment`。
    pub(crate) async fn provider_download(
        &self,
        bot: &Value,
        attachment: &Value,
        actor: &feishu::Actor,
    ) -> Result<Option<Downloaded>> {
        match text(bot, "provider").unwrap_or_default() {
            "telegram" => telegram::download(self, bot, attachment).await,
            "weixin" => weixin::download(self, attachment).await,
            "feishu" | "lark" => self.feishu_download(bot, attachment, actor).await,
            _ => Ok(None),
        }
    }
}

/// `buildSelectionCallbackData` / `formatSelectionCommand` 共用：选择动作去掉 `.set`。
pub fn selection_command_name(action: &str) -> &str {
    action.strip_suffix(".set").unwrap_or(action)
}

/// `formatSelectionCommand`（飞书 / Webhook 的按钮命令）。
pub fn selection_command(selection: &Value, option_id: &str) -> String {
    let action = text(selection, "action").unwrap_or_default();
    match action {
        "permission.respond" => option_id.to_owned(),
        "elicitation.respond" => match text(selection, "token") {
            Some(token) if !token.is_empty() => format!("/elicitation {token} {option_id}"),
            _ => format!("/elicitation {option_id}"),
        },
        "model.provider.set" => format!("/model provider {option_id}"),
        "model.set" => format!("/model model {option_id}"),
        other => format!("/{} {option_id}", selection_command_name(other)),
    }
}

/// 构造出站消息（`createOutbound`）的选项。
#[derive(Default)]
pub struct OutboundExtras {
    pub selection: Option<Value>,
    pub elicitation: Option<Value>,
    pub locale: Option<String>,
}

/// `createOutbound`。
pub fn outbound(actor: &feishu::Actor, text_value: String, extras: OutboundExtras) -> Value {
    let mut out = serde_json::Map::new();
    out.insert("botId".into(), json!(actor.bot_id));
    out.insert("provider".into(), json!(actor.provider));
    out.insert("providerUserId".into(), json!(actor.reply_target()));
    out.insert("text".into(), json!(text_value));
    if let Some(selection) = extras.selection {
        out.insert("selection".into(), selection);
    }
    if let Some(locale) = extras.locale {
        out.insert("locale".into(), json!(locale));
    }
    if let Some(elicitation) = extras.elicitation {
        out.insert("elicitation".into(), elicitation);
    }
    if let Some(token) = &actor.provider_context_token {
        out.insert("providerContextToken".into(), json!(token));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_commands_match_node() {
        let base = |action: &str| json!({"action": action, "token": "abc"});
        assert_eq!(selection_command(&base("workspace.set"), "w1"), "/workspace w1");
        assert_eq!(selection_command(&base("model.provider.set"), "p"), "/model provider p");
        assert_eq!(selection_command(&base("model.set"), "m"), "/model model m");
        assert_eq!(selection_command(&base("permission.respond"), "/approve r o"), "/approve r o");
        assert_eq!(selection_command(&base("elicitation.respond"), "v"), "/elicitation abc v");
        assert_eq!(
            selection_command(&json!({"action": "elicitation.respond"}), "v"),
            "/elicitation v"
        );
        assert_eq!(selection_command(&base("thoughtLevel.set"), "high"), "/thoughtLevel high");
    }
}
