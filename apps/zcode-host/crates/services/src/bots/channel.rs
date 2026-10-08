//! Telegram / 微信长轮询的生命周期（telegramChannelRuntime.ts、weixinChannelRuntime.ts）。
//!
//! 每个启用且配了 token 的 Bot 一个轮询循环；配置变更后按“最新一次刷新”对齐。外部游标（Telegram
//! offset、微信 get_updates_buf）只在本批消息全部成功进入业务处理后才提交，失败时整批重投。
//! 同一个 token 在所有 ZCode 进程里只有一个轮询者（`bots-runtime-locks/<provider>-polling`）。
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use zcode_host_kit::log;

use super::model::{filter_allowed_workspaces, string_list};
use super::now_ms;
use super::providers::REQUEST_TIMEOUT;
use super::runtime_lock;
use super::service::{Bots, find_bot, guard};
use super::util::text;
use super::{telegram, weixin};

/// Telegram 服务端长轮询最多 25 秒；客户端截止覆盖响应体读取。
const TELEGRAM_LONG_POLL_TIMEOUT: Duration = Duration::from_secs(40);
const RETRY: Duration = Duration::from_secs(5);
const CONFLICT_RETRY: Duration = Duration::from_secs(10);

struct Entry {
    stop: watch::Sender<bool>,
    fingerprint: String,
    done: JoinHandle<()>,
}

/// 一类轮询通道的运行时表。
#[derive(Default)]
pub struct PollingRuntimes {
    entries: std::sync::Mutex<HashMap<String, Entry>>,
    generation: AtomicU64,
    queue: tokio::sync::Mutex<()>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Polling {
    Telegram,
    Weixin,
}

impl Polling {
    pub fn provider(self) -> &'static str {
        match self {
            Self::Telegram => "telegram",
            Self::Weixin => "weixin",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Telegram => "Telegram",
            Self::Weixin => "Weixin",
        }
    }

    fn message_prefix(self) -> &'static str {
        match self {
            Self::Telegram => "bots.runtime.telegram",
            Self::Weixin => "bots.runtime.weixin",
        }
    }
}

/// `createBotConnectionFingerprint([provider, credentialRef, credential])`。
fn fingerprint(parts: &[&str]) -> String {
    Sha256::digest(json!(parts).to_string().as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn should_poll(bot: &Value, kind: Polling) -> bool {
    text(bot, "provider") == Some(kind.provider())
        && bot["enabled"] == true
        && text(bot, "credentialRef").is_some_and(|value| !value.is_empty())
}

/// 等待；期间被要求停止返回 true。
async fn wait(duration: Duration, stop: &mut watch::Receiver<bool>) -> bool {
    let stopped = tokio::select! {
        _ = tokio::time::sleep(duration) => false,
        _ = stop.wait_for(|stopped| *stopped) => true,
    };
    stopped || *stop.borrow()
}

/// 在停止信号到来时放弃 future（等价于 fetch 的 AbortSignal）。
async fn until_stopped<T>(
    future: impl std::future::Future<Output = T>,
    stop: &mut watch::Receiver<bool>,
) -> Option<T> {
    if *stop.borrow() {
        return None;
    }
    tokio::select! {
        value = future => Some(value),
        _ = stop.wait_for(|stopped| *stopped) => None,
    }
}

impl Bots {
    fn polling(&self, kind: Polling) -> &PollingRuntimes {
        match kind {
            Polling::Telegram => &self.telegram_runtimes,
            Polling::Weixin => &self.weixin_runtimes,
        }
    }

    /// `scheduleRefresh`（Telegram 与微信）。
    pub(crate) fn schedule_polling_refresh(self: &Arc<Self>) {
        for kind in [Polling::Telegram, Polling::Weixin] {
            let bots = self.clone();
            tokio::spawn(async move {
                if let Err(error) = bots.refresh_polling(kind).await {
                    log::warn(format!(
                        "refresh {} polling failed: {error:#}",
                        kind.label()
                    ));
                }
            });
        }
    }

    /// 串行对齐；只有最新一次刷新会走完。
    pub async fn refresh_polling(self: &Arc<Self>, kind: Polling) -> anyhow::Result<()> {
        let runtimes = self.polling(kind);
        let generation = runtimes.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let _turn = runtimes.queue.lock().await;
        let is_latest = || runtimes.generation.load(Ordering::SeqCst) == generation;
        if !is_latest() {
            return Ok(());
        }
        // 游标写在 bot-state.v3.json：先完成旧状态导入，再启动轮询。
        self.repo.read_state().await?;
        let config = self.repo.read_config().await?;
        if !is_latest() {
            return Ok(());
        }
        let bots = config["bots"].as_array().cloned().unwrap_or_default();
        let active: Vec<String> = bots
            .iter()
            .filter(|bot| should_poll(bot, kind))
            .filter_map(|bot| text(bot, "id").map(str::to_owned))
            .collect();
        let running: Vec<String> = guard(&runtimes.entries).keys().cloned().collect();
        for id in running {
            if !active.contains(&id) {
                self.stop_polling(kind, &id).await;
                if !is_latest() {
                    return Ok(());
                }
            }
        }
        for bot in &bots {
            if text(bot, "provider") != Some(kind.provider()) {
                continue;
            }
            let id = text(bot, "id").unwrap_or_default();
            if should_poll(bot, kind) {
                if kind == Polling::Telegram {
                    self.sync_telegram_commands(bot).await;
                    if !is_latest() {
                        return Ok(());
                    }
                }
                let secret = self.load_bot_secret(bot).await;
                let print = fingerprint(&[
                    kind.provider(),
                    text(bot, "credentialRef").unwrap_or_default(),
                    secret.as_deref().unwrap_or_default(),
                ]);
                if !is_latest() {
                    return Ok(());
                }
                let stale = guard(&runtimes.entries)
                    .get(id)
                    .is_some_and(|entry| entry.fingerprint != print);
                if stale {
                    // token 变了：等旧轮询与锁完全退出，再用新凭据启动，避免两个实例短暂并行。
                    self.stop_polling(kind, id).await;
                    if !is_latest() {
                        return Ok(());
                    }
                }
                self.start_polling(kind, bot.clone(), print);
            } else if bot["enabled"] != true {
                if kind == Polling::Telegram {
                    self.sync_telegram_commands(bot).await;
                }
                self.set_runtime_status(
                    id,
                    json!({"provider": kind.provider(), "status": "disabled",
                           "messageId": "bots.runtime.botDisabled", "message": "Bot is disabled."}),
                );
            }
        }
        Ok(())
    }

    /// `syncCommands`：失败只告警。desktop-attached 远端不碰第三方 API。
    pub(crate) async fn sync_telegram_commands(&self, bot: &Value) {
        if !self.background {
            return;
        }
        let Some(token) = self
            .load_bot_secret(bot)
            .await
            .filter(|token| !token.trim().is_empty())
        else {
            return;
        };
        if let Err(error) = telegram::sync_commands(
            &self.providers.http,
            &self.providers.endpoints.telegram,
            &token,
            bot,
        )
        .await
        {
            log::warn(format!(
                "sync Telegram commands failed bot={}: {error:#}",
                text(bot, "id").unwrap_or_default()
            ));
        }
    }

    fn start_polling(self: &Arc<Self>, kind: Polling, bot: Value, print: String) {
        let id = text(&bot, "id").unwrap_or_default().to_owned();
        let runtimes = self.polling(kind);
        let mut entries = guard(&runtimes.entries);
        if entries.contains_key(&id) {
            return;
        }
        self.set_runtime_status(
            &id,
            json!({"provider": kind.provider(), "status": "polling",
                   "messageId": format!("{}LongPollingStarting", kind.message_prefix()),
                   "message": format!("{} long polling is starting.", kind.label())}),
        );
        let (stop, stop_rx) = watch::channel(false);
        let bots = self.clone();
        let task_id = id.clone();
        let done = tokio::spawn(async move {
            match kind {
                Polling::Telegram => bots.clone().poll_telegram(bot, stop_rx).await,
                Polling::Weixin => bots.clone().poll_weixin(bot, stop_rx).await,
            }
            let still_polling = bots
                .runtime_status(&task_id)
                .is_some_and(|status| status.get("status") == Some(&json!("polling")));
            if still_polling {
                let offset = bots
                    .runtime_status(&task_id)
                    .and_then(|status| status.get("offset").cloned())
                    .unwrap_or(Value::Null);
                bots.set_runtime_status(
                    &task_id,
                    json!({"provider": kind.provider(), "status": "idle",
                           "messageId": format!("{}LongPollingStopped", kind.message_prefix()),
                           "message": format!("{} long polling is stopped.", kind.label()),
                           "offset": offset}),
                );
            }
        });
        entries.insert(
            id,
            Entry {
                stop,
                fingerprint: print,
                done,
            },
        );
    }

    /// `stopPolling`：通知停止并等待请求退出、锁释放。
    pub async fn stop_polling(&self, kind: Polling, bot_id: &str) {
        let entry = guard(&self.polling(kind).entries).remove(bot_id);
        if let Some(entry) = entry {
            let _ = entry.stop.send(true);
            let _ = entry.done.await;
        }
        if self.runtime_status(bot_id).is_some() {
            self.set_runtime_status(
                bot_id,
                json!({"status": "idle",
                       "messageId": format!("{}LongPollingStopped", kind.message_prefix()),
                       "message": format!("{} long polling is stopped.", kind.label())}),
            );
        }
    }

    pub(crate) async fn dispose_polling(&self) {
        for kind in [Polling::Telegram, Polling::Weixin] {
            let runtimes = self.polling(kind);
            runtimes.generation.fetch_add(1, Ordering::SeqCst);
            let entries: Vec<Entry> = guard(&runtimes.entries)
                .drain()
                .map(|(_, entry)| entry)
                .collect();
            for entry in &entries {
                let _ = entry.stop.send(true);
            }
            for entry in entries {
                let _ = entry.done.await;
            }
        }
    }

    pub(crate) async fn read_telegram_offset(&self, bot_id: &str) -> Value {
        match self.repo.read_state().await {
            Ok(state) => state["bots"][bot_id]["telegramOffset"].clone(),
            Err(_) => Value::Null,
        }
    }

    /// `writeTelegramOffset` / `writeWeixinGetUpdatesBuf`：没有上下文时按首个允许的 workspace 建草稿上下文。
    async fn write_cursor(&self, bot_id: &str, key: &str, value: Value) -> anyhow::Result<()> {
        let mut state = self.repo.read_state().await?;
        let now = now_ms();
        let existing = state["bots"].get(bot_id).cloned();
        let next = match existing {
            Some(existing) => {
                let mut next = existing;
                next[key] = value;
                next["updatedAt"] = json!(now);
                Some(next)
            }
            None => {
                let config = self.repo.read_config().await?;
                match find_bot(&config, bot_id) {
                    Some(bot) => {
                        let workspaces = self.workspace_refs(&Value::Null).await;
                        filter_allowed_workspaces(
                            &workspaces,
                            &string_list(&bot["allowedWorkspaces"]),
                        )
                        .into_iter()
                        .next()
                        .map(|workspace| {
                            let mut context = Map::new();
                            context.insert("botId".into(), json!(bot_id));
                            context
                                .insert("workspacePath".into(), workspace["workspacePath"].clone());
                            if let Some(identity) = text(&workspace, "workspaceIdentity") {
                                context.insert("workspaceIdentity".into(), json!(identity));
                            }
                            context.insert("workspaceId".into(), workspace["id"].clone());
                            context.insert("mode".into(), json!("draft"));
                            context.insert("activeTaskId".into(), Value::Null);
                            context.insert(key.into(), value);
                            context.insert("updatedAt".into(), json!(now));
                            Value::Object(context)
                        })
                    }
                    None => None,
                }
            }
        };
        if let (Some(next), Some(bots)) = (next, state["bots"].as_object_mut()) {
            bots.insert(bot_id.to_owned(), next);
        }
        self.repo.write_state(&state).await?;
        Ok(())
    }

    async fn telegram_status(&self, id: &str, patch: Value) {
        let mut patch = patch;
        patch["provider"] = json!("telegram");
        patch["offset"] = self.read_telegram_offset(id).await;
        self.set_runtime_status(id, patch);
    }

    /// `pollBot`（Telegram）。
    async fn poll_telegram(self: Arc<Self>, bot: Value, mut stop: watch::Receiver<bool>) {
        let id = text(&bot, "id").unwrap_or_default().to_owned();
        let Some(token) = self
            .load_bot_secret(&bot)
            .await
            .filter(|token| !token.trim().is_empty())
        else {
            self.set_runtime_status(
                &id,
                json!({"provider": "telegram", "status": "error",
                       "messageId": "bots.runtime.telegramTokenMissing",
                       "message": "Telegram bot token is missing."}),
            );
            return;
        };
        let base = self.providers.endpoints.telegram.clone();
        while !*stop.borrow() {
            let lock = match runtime_lock::acquire(self.repo.dir(), "telegram-polling", &token, &id)
                .await
            {
                Ok(Some(lock)) => lock,
                Ok(None) => {
                    self.telegram_status(
                        &id,
                        json!({"status": "idle",
                               "messageId": "bots.runtime.telegramLongPollingHandledElsewhere",
                               "message": "Telegram long polling is handled by another ZCode window."}),
                    )
                    .await;
                    if wait(Duration::from_millis(runtime_lock::RETRY_MS), &mut stop).await {
                        return;
                    }
                    continue;
                }
                Err(error) => {
                    if *stop.borrow() {
                        return;
                    }
                    self.telegram_status(
                        &id,
                        json!({"status": "error", "message": format!("Telegram polling lock failed: {error}")}),
                    )
                    .await;
                    if wait(RETRY, &mut stop).await {
                        return;
                    }
                    continue;
                }
            };
            let result = self
                .telegram_loop(&bot, &id, &token, &base, &mut stop)
                .await;
            lock.release().await;
            match result {
                Ok(()) => return,
                Err(error) => {
                    if *stop.borrow() {
                        return;
                    }
                    log::debug(format!("Telegram polling failed bot={id}: {error:#}"));
                    self.telegram_status(
                        &id,
                        json!({"status": "error",
                               "messageId": "bots.runtime.telegramPollingFailedRetrying",
                               "message": "Telegram polling failed; retrying."}),
                    )
                    .await;
                    if wait(RETRY, &mut stop).await {
                        return;
                    }
                }
            }
        }
    }

    /// 持锁期间的轮询；正常停止返回 Ok，异常返回 Err（由外层退避重试）。
    async fn telegram_loop(
        self: &Arc<Self>,
        bot: &Value,
        id: &str,
        token: &str,
        base: &str,
        stop: &mut watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        let http = self.providers.http.clone();
        let delete = http.status(
            http.client()
                .post(format!("{base}/bot{token}/deleteWebhook"))
                .header("content-type", "application/json")
                .body(json!({"drop_pending_updates": false}).to_string()),
            REQUEST_TIMEOUT,
        );
        if until_stopped(delete, stop).await.is_none() {
            return Ok(());
        }
        self.telegram_status(
            id,
            json!({"status": "polling", "message": "Telegram long polling is running.",
                   "messageId": "bots.runtime.telegramLongPollingRunning"}),
        )
        .await;
        while !*stop.borrow() {
            let offset = self.read_telegram_offset(id).await;
            let mut body = Map::new();
            body.insert("timeout".into(), json!(25));
            if !offset.is_null() {
                body.insert("offset".into(), offset.clone());
            }
            body.insert(
                "allowed_updates".into(),
                json!(["message", "callback_query"]),
            );
            let request = http.json(
                http.client()
                    .post(format!("{base}/bot{token}/getUpdates"))
                    .header("content-type", "application/json")
                    .body(Value::Object(body).to_string()),
                TELEGRAM_LONG_POLL_TIMEOUT,
            );
            let Some(response) = until_stopped(request, stop).await else {
                return Ok(());
            };
            let response = response?;
            if !response.ok {
                let message = if response.status == 409 {
                    "Telegram token is already used by another polling client.".to_owned()
                } else {
                    format!("Telegram getUpdates failed: HTTP {}", response.status)
                };
                self.telegram_status(id, json!({"status": "error", "message": message}))
                    .await;
                let delay = if response.status == 409 {
                    CONFLICT_RETRY
                } else {
                    RETRY
                };
                if wait(delay, stop).await {
                    return Ok(());
                }
                continue;
            }
            let payload = response.payload;
            let Some(updates) = payload["result"]
                .as_array()
                .filter(|_| payload["ok"] == true)
            else {
                let message = text(&payload, "description")
                    .unwrap_or("Telegram getUpdates returned an invalid response.");
                self.telegram_status(id, json!({"status": "error", "message": message}))
                    .await;
                if wait(RETRY, stop).await {
                    return Ok(());
                }
                continue;
            };
            for update in updates {
                if *stop.borrow() {
                    return Ok(());
                }
                let result = self
                    .process_callback(
                        "telegram",
                        json!({"botId": bot["id"], "update": update}),
                        false,
                    )
                    .await;
                if result["ok"] != true {
                    // 业务失败时不提交 offset：同一条 update 下一轮重投，避免永久丢消息。
                    anyhow::bail!(
                        "Telegram callback failed: status={}",
                        result["status"]
                            .as_i64()
                            .map_or("unknown".to_owned(), |status| status.to_string())
                    );
                }
                if let Some(update_id) = update["update_id"].as_i64() {
                    self.write_cursor(id, "telegramOffset", json!(update_id + 1))
                        .await?;
                }
            }
            self.telegram_status(
                id,
                json!({"status": "polling", "message": "Telegram long polling is running.",
                       "messageId": "bots.runtime.telegramLongPollingRunning"}),
            )
            .await;
        }
        Ok(())
    }

    /// `pollBot`（微信）。
    async fn poll_weixin(self: Arc<Self>, bot: Value, mut stop: watch::Receiver<bool>) {
        let id = text(&bot, "id").unwrap_or_default().to_owned();
        let Some(token) = self
            .load_bot_secret(&bot)
            .await
            .filter(|token| !token.trim().is_empty())
        else {
            self.set_runtime_status(
                &id,
                json!({"provider": "weixin", "status": "error", "message": "Weixin bot token is missing."}),
            );
            return;
        };
        while !*stop.borrow() {
            let lock = match runtime_lock::acquire(self.repo.dir(), "weixin-polling", &token, &id)
                .await
            {
                Ok(Some(lock)) => lock,
                Ok(None) => {
                    self.set_runtime_status(
                        &id,
                        json!({"provider": "weixin", "status": "idle",
                               "message": "Weixin long polling is handled by another ZCode window."}),
                    );
                    if wait(Duration::from_millis(runtime_lock::RETRY_MS), &mut stop).await {
                        return;
                    }
                    continue;
                }
                Err(error) => {
                    if *stop.borrow() {
                        return;
                    }
                    self.set_runtime_status(
                        &id,
                        json!({"provider": "weixin", "status": "error",
                               "message": format!("Weixin polling lock failed: {error}")}),
                    );
                    if wait(RETRY, &mut stop).await {
                        return;
                    }
                    continue;
                }
            };
            let result = self.weixin_loop(&bot, &id, &mut stop).await;
            lock.release().await;
            match result {
                Ok(()) => return,
                Err(error) => {
                    if *stop.borrow() {
                        return;
                    }
                    self.set_runtime_status(
                        &id,
                        json!({"provider": "weixin", "status": "error",
                               "message": format!("Weixin polling failed: {error}")}),
                    );
                    if wait(RETRY, &mut stop).await {
                        return;
                    }
                }
            }
        }
    }

    async fn weixin_loop(
        self: &Arc<Self>,
        bot: &Value,
        id: &str,
        stop: &mut watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        let running = json!({"provider": "weixin", "status": "polling",
            "message": "Weixin long polling is running.",
            "messageId": "bots.runtime.weixinLongPollingRunning"});
        self.set_runtime_status(id, running.clone());
        while !*stop.borrow() {
            let buf = self.repo.read_state().await?["bots"][id]["weixinGetUpdatesBuf"]
                .as_str()
                .map(str::to_owned);
            let Some(updates) =
                until_stopped(weixin::get_updates(self, bot, buf.as_deref()), stop).await
            else {
                return Ok(());
            };
            let updates = updates?;
            if updates.raw_count > 0 || !updates.messages.is_empty() {
                let files: usize = updates.messages.iter().map(|m| m.attachments.len()).sum();
                log::debug(format!(
                    "weixin polling received bot={id} raw={} parsed={} attachments={files}",
                    updates.raw_count,
                    updates.messages.len()
                ));
            }
            for inbound in &updates.messages {
                if *stop.borrow() {
                    return Ok(());
                }
                let mut wrapped = Map::new();
                let mut put = |key: &str, value: Option<&String>| {
                    if let Some(value) = value {
                        wrapped.insert(key.into(), json!(value));
                    }
                };
                put("id", inbound.actor.provider_message_id.as_ref());
                put("text", Some(&inbound.text));
                put("from", Some(&inbound.actor.provider_user_id));
                put("chatId", inbound.actor.chat_id.as_ref());
                put("displayName", inbound.actor.display_name.as_ref());
                put(
                    "context_token",
                    inbound.actor.provider_context_token.as_ref(),
                );
                if !inbound.attachments.is_empty() {
                    // 已解析出的附件重新包装进回调时不能丢，否则纯图片消息会被静默吞掉。
                    wrapped.insert("attachments".into(), json!(inbound.attachments));
                }
                let mut payload = Map::new();
                payload.insert("botId".into(), json!(id));
                payload.insert("messages".into(), json!([Value::Object(wrapped)]));
                if let Some(buf) = &updates.buf {
                    payload.insert("buf".into(), json!(buf));
                }
                let result = self
                    .process_callback("weixin", Value::Object(payload), false)
                    .await;
                if result["ok"] != true {
                    anyhow::bail!(
                        "Weixin callback failed: status={}",
                        result["status"]
                            .as_i64()
                            .map_or("unknown".to_owned(), |status| status.to_string())
                    );
                }
            }
            if let Some(buf) = &updates.buf {
                // buf 是服务端游标：本批全部进入业务处理后才持久化。
                self.write_cursor(id, "weixinGetUpdatesBuf", json!(buf))
                    .await?;
            }
            self.set_runtime_status(id, running.clone());
        }
        Ok(())
    }
}
