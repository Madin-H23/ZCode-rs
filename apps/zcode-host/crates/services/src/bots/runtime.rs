//! 飞书 / Lark 长连接的生命周期（feishuChannelRuntime.ts）。
//!
//! 每个已启用且配好 App ID / App Secret 的飞书 Bot 一条长连接；配置变更后按「最新一次刷新」对齐
//! （较早的刷新发现自己过期就停止）。连接前先拿跨进程运行锁，其他 ZCode 进程持有时让出并每 10 秒重试。
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::FutureExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use zcode_host_kit::log;

use super::feishu::callback_payload;
use super::feishu::cards;
use super::feishu::ws::{Connection, ConnectionState, EventHandler};
use super::model::is_feishu;
use super::runtime_lock;
use super::service::{Bots, guard, text};

struct Entry {
    stop: watch::Sender<bool>,
    fingerprint: String,
    done: JoinHandle<()>,
}

#[derive(Default)]
pub struct Runtimes {
    entries: std::sync::Mutex<HashMap<String, Entry>>,
    generation: AtomicU64,
    queue: tokio::sync::Mutex<()>,
}

/// `createBotConnectionFingerprint`：sha256(JSON.stringify(parts))。
fn fingerprint(parts: &[&str]) -> String {
    let digest = Sha256::digest(json!(parts).to_string().as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn is_valid_app_id(app_id: &str) -> bool {
    app_id.len() == 20
        && app_id.starts_with("cli_")
        && app_id[4..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn should_connect(bot: &Value) -> bool {
    is_feishu(text(bot, "provider").unwrap_or_default())
        && bot["enabled"] == true
        && text(bot, "credentialRef").is_some_and(|value| !value.is_empty())
        && text(bot, "feishuAppId").is_some_and(|value| !value.is_empty())
}

/// 等待；期间被要求停止返回 true。
async fn wait(duration: Duration, stop: &mut watch::Receiver<bool>) -> bool {
    let stopped = tokio::select! {
        _ = tokio::time::sleep(duration) => false,
        _ = stop.wait_for(|stopped| *stopped) => true,
    };
    stopped || *stop.borrow()
}

impl Bots {
    /// `scheduleRefresh`（飞书）：后台对齐长连接，失败只记日志。
    pub(crate) fn schedule_feishu_refresh(self: &Arc<Self>) {
        let bots = self.clone();
        tokio::spawn(async move {
            if let Err(error) = bots.refresh_feishu().await {
                log::warn(format!("refresh Feishu WebSocket failed: {error:#}"));
            }
        });
    }

    /// 串行对齐；只有最新一次刷新会走完（`createLatestRuntimeRefreshQueue`）。
    pub async fn refresh_feishu(self: &Arc<Self>) -> anyhow::Result<()> {
        let generation = self.runtimes.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let _turn = self.runtimes.queue.lock().await;
        let is_latest = || self.runtimes.generation.load(Ordering::SeqCst) == generation;
        if !is_latest() {
            return Ok(());
        }
        let config = self.repo.read_config().await?;
        let bots = config["bots"].as_array().cloned().unwrap_or_default();
        let active: Vec<String> = bots
            .iter()
            .filter(|bot| should_connect(bot))
            .filter_map(|bot| text(bot, "id").map(str::to_owned))
            .collect();
        let running: Vec<String> = guard(&self.runtimes.entries).keys().cloned().collect();
        for id in running {
            if !active.contains(&id) {
                self.stop_websocket(&id).await;
                if !is_latest() {
                    return Ok(());
                }
            }
        }
        for bot in &bots {
            let id = text(bot, "id").unwrap_or_default();
            let provider = text(bot, "provider").unwrap_or_default();
            if should_connect(bot) {
                let secret = self.load_credential(text(bot, "credentialRef")).await;
                let print = fingerprint(&[
                    provider,
                    text(bot, "feishuAppId").unwrap_or_default(),
                    text(bot, "credentialRef").unwrap_or_default(),
                    secret.as_deref().unwrap_or_default(),
                ]);
                if !is_latest() {
                    return Ok(());
                }
                let stale = guard(&self.runtimes.entries)
                    .get(id)
                    .is_some_and(|entry| entry.fingerprint != print);
                if stale {
                    self.stop_websocket(id).await;
                    if !is_latest() {
                        return Ok(());
                    }
                }
                self.start_websocket(bot.clone(), print);
            } else if is_feishu(provider) && bot["enabled"] != true {
                self.set_runtime_status(
                    id,
                    json!({"provider": provider, "status": "disabled",
                           "messageId": "bots.runtime.botDisabled", "message": "Bot is disabled."}),
                );
            }
        }
        Ok(())
    }

    fn start_websocket(self: &Arc<Self>, bot: Value, print: String) {
        let id = text(&bot, "id").unwrap_or_default().to_owned();
        let mut entries = guard(&self.runtimes.entries);
        if entries.contains_key(&id) {
            return;
        }
        self.set_runtime_status(
            &id,
            json!({"provider": bot["provider"], "status": "connected",
                   "messageId": "bots.runtime.feishuWebSocketStarting",
                   "message": "Feishu WebSocket is starting."}),
        );
        let (stop, stop_rx) = watch::channel(false);
        let bots = self.clone();
        let done = tokio::spawn(async move {
            let provider = bot["provider"].clone();
            let id = text(&bot, "id").unwrap_or_default().to_owned();
            bots.clone().run_bot(bot, stop_rx).await;
            let still_connected = bots
                .runtime_status(&id)
                .is_some_and(|status| status.get("status") == Some(&json!("connected")));
            if still_connected {
                bots.set_runtime_status(
                    &id,
                    json!({"provider": provider, "status": "idle",
                           "messageId": "bots.runtime.feishuWebSocketStopped",
                           "message": "Feishu WebSocket is stopped."}),
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

    /// `stopWebSocket`：通知停止并等待连接收尾。
    pub async fn stop_websocket(&self, bot_id: &str) {
        let entry = guard(&self.runtimes.entries).remove(bot_id);
        if let Some(entry) = entry {
            let _ = entry.stop.send(true);
            let _ = entry.done.await;
        }
        if let Some(previous) = self.runtime_status(bot_id) {
            self.set_runtime_status(
                bot_id,
                json!({"provider": previous.get("provider").cloned().unwrap_or(Value::Null),
                       "status": "idle",
                       "messageId": "bots.runtime.feishuWebSocketStopped",
                       "message": "Feishu WebSocket is stopped."}),
            );
        }
    }

    /// 停止全部长连接（Host 退出）。
    pub(crate) async fn dispose_feishu(&self) {
        self.runtimes.generation.fetch_add(1, Ordering::SeqCst);
        let entries: Vec<Entry> = guard(&self.runtimes.entries)
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

    fn event_handler(self: &Arc<Self>, bot_id: String, provider: String) -> EventHandler {
        let bots = self.clone();
        Arc::new(move |event_type: String, event: Value| {
            let bots = bots.clone();
            let bot_id = bot_id.clone();
            let provider = provider.clone();
            async move {
                match event_type.as_str() {
                    "im.message.receive_v1" => {
                        let payload = callback_payload(&bot_id, &provider, event);
                        let result = bots.process_callback(&provider, payload, false).await;
                        if result["ok"] != true {
                            // 回调失败时不确认，交给飞书重投（assertBotCallbackSucceeded）。
                            let label = if provider == "lark" { "Lark" } else { "Feishu" };
                            anyhow::bail!(
                                "{label} callback failed: status={}",
                                result["status"]
                                    .as_i64()
                                    .map_or("unknown".to_owned(), |s| s.to_string())
                            );
                        }
                        Ok(None)
                    }
                    // 飞书会把我们自己创建的 reaction 推回长连接；业务不处理。
                    "im.message.reaction.created_v1" => Ok(None),
                    "card.action.trigger" => {
                        let mut payload = callback_payload(&bot_id, &provider, event);
                        payload["zcodeFeishuSynchronousCardAction"] = json!(true);
                        let result = bots.process_callback(&provider, payload, true).await;
                        if result["ok"] != true {
                            anyhow::bail!("Feishu callback failed");
                        }
                        // 飞书点击后的同步回包才是客户端可靠采用的卡片状态：用业务层推进完成的完整
                        // 出站消息（含选项与问答状态）生成卡片。
                        let reply = &result["replies"][0];
                        Ok(reply.is_object().then(
                            || json!({"card": {"type": "raw", "data": cards::for_message(reply)}}),
                        ))
                    }
                    other => Ok(Some(json!(format!("no {other} event handle")))),
                }
            }
            .boxed()
        })
    }

    /// `runBot`：拿锁 → 连接 → 断开或失败后 5 秒重试，直到被停止。
    async fn run_bot(self: Arc<Self>, bot: Value, mut stop: watch::Receiver<bool>) {
        let id = text(&bot, "id").unwrap_or_default().to_owned();
        let provider = text(&bot, "provider").unwrap_or("feishu").to_owned();
        let app_id = text(&bot, "feishuAppId").unwrap_or_default().to_owned();
        while !*stop.borrow() {
            let namespace = format!("{provider}-websocket");
            let lock = match runtime_lock::acquire(self.repo.dir(), &namespace, &app_id, &id).await
            {
                Ok(Some(lock)) => lock,
                Ok(None) => {
                    self.set_runtime_status(
                        &id,
                        json!({"provider": provider, "status": "idle",
                               "message": "Feishu WebSocket is handled by another ZCode window."}),
                    );
                    if wait(Duration::from_millis(runtime_lock::RETRY_MS), &mut stop).await {
                        return;
                    }
                    continue;
                }
                Err(error) => {
                    log::warn(format!(
                        "acquire Feishu WebSocket lock failed bot={id}: {error:#}"
                    ));
                    self.set_runtime_status(
                        &id,
                        json!({"provider": provider, "status": "error",
                               "message": format!("Feishu WebSocket lock failed: {error}")}),
                    );
                    if wait(Duration::from_secs(5), &mut stop).await {
                        return;
                    }
                    continue;
                }
            };
            self.set_runtime_status(
                &id,
                json!({"provider": provider, "status": "connected",
                       "messageId": "bots.runtime.feishuWebSocketConnecting",
                       "message": "Feishu WebSocket is connecting."}),
            );
            let result = self.connect_once(&bot, &provider, &app_id, &stop).await;
            lock.release().await;
            match result {
                Ok(()) => return,
                Err(error) => {
                    if *stop.borrow() {
                        return;
                    }
                    self.set_runtime_status(
                        &id,
                        json!({"provider": provider, "status": "error",
                               "message": format!("Feishu WebSocket failed: {error}")}),
                    );
                    if wait(Duration::from_secs(5), &mut stop).await {
                        return;
                    }
                }
            }
        }
    }

    async fn connect_once(
        self: &Arc<Self>,
        bot: &Value,
        provider: &str,
        app_id: &str,
        stop: &watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        if !is_valid_app_id(app_id) {
            anyhow::bail!("Invalid Feishu App ID.");
        }
        let Some(secret) = self.feishu_secret(bot).await else {
            anyhow::bail!("Feishu App ID and App Secret are required.");
        };
        let id = text(bot, "id").unwrap_or_default().to_owned();
        let connection = Connection {
            provider: provider.to_owned(),
            app_id: app_id.to_owned(),
            app_secret: secret,
        };
        let handler = self.event_handler(id.clone(), provider.to_owned());
        let status = self.clone();
        let provider_value = json!(provider);
        connection
            .run(
                &self.providers.feishu,
                handler,
                stop.clone(),
                move |state| {
                    let (message, message_id) = match state {
                        ConnectionState::Connected => (
                            "Feishu WebSocket is running.",
                            "bots.runtime.feishuWebSocketRunning",
                        ),
                        ConnectionState::Reconnecting => (
                            "Feishu WebSocket is reconnecting.",
                            "bots.runtime.feishuWebSocketConnecting",
                        ),
                    };
                    status.set_runtime_status(
                        &id,
                        json!({"provider": provider_value, "status": "connected",
                           "message": message, "messageId": message_id}),
                    );
                },
            )
            .await
    }
}
