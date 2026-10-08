//! `IBotsService` 的管理面（botsService.ts 的公开方法）与 `bots` channel。
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use zcode_host_kit::{Args, Reply, RpcError, ServiceBuilder, log};

use super::TaskPort;
use super::feishu::api::{AppCredentials, FeishuApi};
use super::model::{
    self, ALL_BOT_WORKSPACES, BOT_BIND_CODE_TTL_MS, is_feishu, normalize_allowed_workspaces,
    normalize_bot, string_list,
};
use super::now_ms;
use super::runtime::Runtimes;
use super::store::BotsRepo;
use crate::account::credential::CredentialStore;
use crate::settings::SettingStore;

/// 第一次读取应用名称可能因飞书同步延迟失败：扫码创建后按 0 / 800 / 1800 ms 重试。
const FEISHU_NAME_RETRY_MS: [u64; 3] = [0, 800, 1_800];
/// 只能被一个启用 Bot 使用的凭据通道（同一 token 不能被两条长连接 / 轮询共用）。
const EXCLUSIVE_CREDENTIAL_PROVIDERS: [&str; 3] = ["telegram", "feishu", "lark"];

pub struct BindCode {
    pub bot_id: String,
    pub allowed_workspaces: Vec<String>,
    pub expires_at: i64,
}

pub struct BotsDeps {
    pub repo: BotsRepo,
    pub credentials: CredentialStore,
    pub api: Arc<FeishuApi>,
    pub settings: SettingStore,
    pub tasks: Arc<dyn TaskPort>,
    pub background: bool,
}

pub struct Bots {
    pub(crate) repo: BotsRepo,
    pub(crate) credentials: CredentialStore,
    pub(crate) api: Arc<FeishuApi>,
    pub(crate) settings: SettingStore,
    pub(crate) tasks: Arc<dyn TaskPort>,
    pub(crate) background: bool,
    pub(crate) bind_codes: Mutex<HashMap<String, BindCode>>,
    /// `runtimeByBotId`。
    runtime_status: Mutex<HashMap<String, Map<String, Value>>>,
    pub(crate) runtimes: Runtimes,
    /// 每个聊天上下文串行处理入站消息（`enqueueInboundProcessing`）。
    pub(crate) inbound_queues: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// 2 分钟内处理过的入站消息（`recentInboundDeliveryAtByKey`）。
    pub(crate) recent_inbound: Mutex<HashMap<String, i64>>,
    /// 本进程发起、尚未观察到终态的任务（`runningTasks`）。
    pub(crate) running: Mutex<HashSet<String>>,
    pub(crate) delivery_warnings: Mutex<HashMap<String, i64>>,
    pub(crate) name_retry_ms: Mutex<Vec<u64>>,
}

pub(crate) fn guard<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn error(message: impl std::fmt::Display) -> RpcError {
    RpcError::new("Error", message.to_string())
}

fn anyhow_error(error: anyhow::Error) -> RpcError {
    error_message(&error)
}

fn error_message(error: &anyhow::Error) -> RpcError {
    RpcError::new("Error", error.to_string())
}

pub(crate) fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

pub(crate) fn find_bot(config: &Value, bot_id: &str) -> Option<Value> {
    config["bots"]
        .as_array()?
        .iter()
        .find(|bot| text(bot, "id") == Some(bot_id))
        .cloned()
}

/// `validateBotConfig`。
pub(crate) fn validate(config: &Value, candidate: &Value) -> Result<(), RpcError> {
    let id = text(candidate, "id").unwrap_or_default();
    if id.trim().is_empty() {
        return Err(error("Bot id is required."));
    }
    let enabled = candidate["enabled"] == true;
    let provider = text(candidate, "provider").unwrap_or_default();
    let others = || {
        config["bots"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(move |bot| {
                text(bot, "id") != Some(id)
                    && bot["enabled"] == true
                    && text(bot, "provider") == Some(provider)
            })
    };
    if enabled
        && let Some(user) = text(candidate, "providerUserId").filter(|user| !user.trim().is_empty())
        && others().any(|bot| text(bot, "providerUserId") == Some(user))
    {
        return Err(error(
            "An enabled bot with this provider user already exists.",
        ));
    }
    if enabled
        && EXCLUSIVE_CREDENTIAL_PROVIDERS.contains(&provider)
        && let Some(reference) =
            text(candidate, "credentialRef").filter(|value| !value.trim().is_empty())
        && others().any(|bot| text(bot, "credentialRef") == Some(reference))
    {
        return Err(error(
            "Enabled polling bots cannot share the same credential.",
        ));
    }
    Ok(())
}

fn random_code() -> String {
    let mut bytes = [0u8; 3];
    let _ = getrandom::fill(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02X}")).collect()
}

impl Bots {
    pub fn new(deps: BotsDeps) -> Arc<Self> {
        Arc::new(Self {
            repo: deps.repo,
            credentials: deps.credentials,
            api: deps.api,
            settings: deps.settings,
            tasks: deps.tasks,
            background: deps.background,
            bind_codes: Mutex::new(HashMap::new()),
            runtime_status: Mutex::new(HashMap::new()),
            runtimes: Runtimes::default(),
            inbound_queues: Mutex::new(HashMap::new()),
            recent_inbound: Mutex::new(HashMap::new()),
            running: Mutex::new(HashSet::new()),
            delivery_warnings: Mutex::new(HashMap::new()),
            name_retry_ms: Mutex::new(FEISHU_NAME_RETRY_MS.to_vec()),
        })
    }

    pub(crate) async fn locale(&self) -> Option<String> {
        text(&self.settings.get().await, "locale").map(str::to_owned)
    }

    /// `setRuntimeStatus`：与上一状态合并；值为 null 的键删除（TS 里赋 undefined）。
    pub(crate) fn set_runtime_status(&self, bot_id: &str, patch: Value) {
        let mut statuses = guard(&self.runtime_status);
        let entry = statuses.entry(bot_id.to_owned()).or_default();
        entry.insert("botId".into(), json!(bot_id));
        for (key, value) in patch.as_object().into_iter().flatten() {
            if value.is_null() {
                entry.shift_remove(key);
            } else {
                entry.insert(key.clone(), value.clone());
            }
        }
        entry.insert("lastUpdateAt".into(), json!(now_ms()));
    }

    pub(crate) fn runtime_status(&self, bot_id: &str) -> Option<Map<String, Value>> {
        guard(&self.runtime_status).get(bot_id).cloned()
    }

    /// `onDeliveryResult`：投递错误单独记录，不覆盖连接状态。
    pub(crate) fn on_delivery_result(&self, bot: &Value, delivery_error: Option<String>) {
        let id = text(bot, "id").unwrap_or_default();
        let status = self
            .runtime_status(id)
            .and_then(|current| current.get("status").cloned())
            .unwrap_or_else(|| {
                json!(if bot["enabled"] == true {
                    "idle"
                } else {
                    "disabled"
                })
            });
        self.set_runtime_status(
            id,
            json!({
                "provider": bot["provider"],
                "status": status,
                "deliveryError": delivery_error,
            }),
        );
    }

    pub(crate) async fn load_credential(&self, key: Option<&str>) -> Option<String> {
        let key = key?;
        match self.credentials.load(key).await {
            Ok(value) => value,
            Err(error) => {
                log::warn(format!("bots load credential failed key={key}: {error:#}"));
                None
            }
        }
    }

    /// 飞书 Bot 的应用凭据；缺 App ID / App Secret 时为 None。
    pub(crate) async fn feishu_secret(&self, bot: &Value) -> Option<String> {
        text(bot, "feishuAppId")?;
        self.load_credential(text(bot, "credentialRef")).await
    }

    pub(crate) async fn tenant_token(&self, bot: &Value) -> anyhow::Result<Option<String>> {
        let Some(secret) = self.feishu_secret(bot).await else {
            return Ok(None);
        };
        let app = AppCredentials {
            provider: text(bot, "provider").unwrap_or("feishu"),
            app_id: text(bot, "feishuAppId").unwrap_or_default(),
            credential_ref: text(bot, "credentialRef").unwrap_or_default(),
            app_secret: &secret,
        };
        self.api.tenant_token(&app).await.map(Some)
    }

    async fn read_config(&self) -> Result<Value, RpcError> {
        self.repo.read_config().await.map_err(anyhow_error)
    }

    async fn write_config(&self, config: &Value) -> Result<Value, RpcError> {
        self.repo.write_config(config).await.map_err(anyhow_error)
    }

    pub async fn get_status(&self) -> Result<Value, RpcError> {
        let config = self.read_config().await?;
        let state = self.repo.read_state().await.map_err(anyhow_error)?;
        let bots = config["bots"].as_array().cloned().unwrap_or_default();
        let runtime: Vec<Value> = bots
            .iter()
            .map(|bot| {
                let id = text(bot, "id").unwrap_or_default();
                match self.runtime_status(id) {
                    Some(status) => Value::Object(status),
                    None => {
                        let enabled = bot["enabled"] == true;
                        let mut out = json!({
                            "botId": id,
                            "provider": bot["provider"],
                            "status": if enabled { "idle" } else { "disabled" },
                            "message": if enabled { "Bot is configured." } else { "Bot is disabled." },
                        });
                        if let Some(offset) = state["bots"][id].get("telegramOffset") {
                            out["offset"] = offset.clone();
                        }
                        out
                    }
                }
            })
            .collect();
        Ok(json!({
            "botsCount": bots.len(),
            "enabledBotsCount": bots.iter().filter(|bot| bot["enabled"] == true).count(),
            "contextsCount": state["bots"].as_object().map(Map::len).unwrap_or_default(),
            "botRuntime": runtime,
        }))
    }

    pub async fn save_config(self: &Arc<Self>, config: &Value) -> Result<Value, RpcError> {
        let mut normalized = config.clone();
        if let Some(bots) = normalized.get_mut("bots").and_then(Value::as_array_mut) {
            for bot in bots.iter_mut() {
                *bot = normalize_bot(bot);
            }
        }
        let saved = self.write_config(&normalized).await?;
        self.schedule_refresh();
        Ok(saved)
    }

    /// `adapter.resolveName`：只有飞书 / Lark 能从开放平台读应用名称。
    async fn resolve_name(&self, bot: &Value) -> Option<String> {
        let provider = text(bot, "provider").unwrap_or_default();
        if !is_feishu(provider) {
            return None;
        }
        let delays = guard(&self.name_retry_ms).clone();
        let mut last_error = None;
        for delay in delays {
            if delay > 0 {
                // 飞书 / Lark 扫码创建应用后，应用信息接口可能短暂不可读；重试后再回填名称。
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
            let secret = self.feishu_secret(bot).await?;
            let app = AppCredentials {
                provider,
                app_id: text(bot, "feishuAppId").unwrap_or_default(),
                credential_ref: text(bot, "credentialRef").unwrap_or_default(),
                app_secret: &secret,
            };
            match self.api.read_app_display_name(&app).await {
                Ok(Some(name)) if !name.trim().is_empty() => return Some(name.trim().to_owned()),
                Ok(_) => {}
                Err(error) => last_error = Some(error),
            }
        }
        if let Some(error) = last_error {
            log::warn(format!(
                "resolve bot name failed bot={}: {error}",
                text(bot, "id").unwrap_or_default()
            ));
        }
        None
    }

    pub async fn save_bot(self: &Arc<Self>, params: &Value) -> Result<Value, RpcError> {
        let config = self.read_config().await?;
        let input = &params["bot"];
        let provider = text(input, "provider").unwrap_or_default().to_owned();
        let mut bot = input.as_object().cloned().unwrap_or_default();
        bot.insert(
            "id".into(),
            json!(text(input, "id").unwrap_or_default().trim()),
        );
        bot.insert(
            "name".into(),
            json!(text(input, "name").unwrap_or_default().trim()),
        );
        let mut bot = normalize_bot(&Value::Object(bot));
        bot["replyMode"] = json!(model::normalize_reply_mode(
            &provider,
            text(input, "replyMode")
        ));
        let id = text(&bot, "id").unwrap_or_default().to_owned();
        let credential = text(params, "credentialValue")
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if let Some(value) = credential {
            let key = format!("bot:{id}:credential");
            self.credentials
                .save(&key, value)
                .await
                .map_err(|error| error.to_rpc())?;
            bot["credentialRef"] = json!(key);
        }
        if let Some(value) = text(params, "webhookSecretValue")
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let key = format!("bot:{id}:webhook-secret");
            self.credentials
                .save(&key, value)
                .await
                .map_err(|error| error.to_rpc())?;
            bot["webhookSecretRef"] = json!(key);
        }
        if (credential.is_some() || text(&bot, "name").unwrap_or_default().trim().is_empty())
            && let Some(name) = self.resolve_name(&bot).await
        {
            bot["name"] = json!(name);
        }
        let bot = normalize_bot(&bot);
        validate(&config, &bot)?;
        let mut bots: Vec<Value> = config["bots"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| text(item, "id") != Some(id.as_str()))
            .cloned()
            .collect();
        bots.push(bot.clone());
        let mut next = config.clone();
        next["bots"] = Value::Array(bots);
        self.write_config(&next).await?;
        self.schedule_refresh();
        Ok(bot)
    }

    pub async fn remove_bot_secret(self: &Arc<Self>, bot_id: &str) -> Result<Value, RpcError> {
        let config = self.read_config().await?;
        let bot =
            find_bot(&config, bot_id).ok_or_else(|| error(format!("Bot not found: {bot_id}")))?;
        let provider = text(&bot, "provider").unwrap_or_default().to_owned();
        if is_feishu(&provider) {
            self.stop_websocket(bot_id).await;
        }
        // 只移除密钥时如果保留旧绑定身份，UI 会显示“已连通”但运行时已经没有 token 可用；
        // 同步清理绑定状态，让 Bot 回到可重新添加的状态。
        let mut next_bot = bot.as_object().cloned().unwrap_or_default();
        for key in [
            "credentialRef",
            "webhookSecretRef",
            "providerUserId",
            "displayName",
        ] {
            next_bot.shift_remove(key);
        }
        if is_feishu(&provider) {
            next_bot.shift_remove("feishuAppId");
        }
        let next_bot = normalize_bot(&Value::Object(next_bot));
        let mut next = config.clone();
        next["bots"] = Value::Array(
            config["bots"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|item| {
                    if text(item, "id") == Some(bot_id) {
                        next_bot.clone()
                    } else {
                        item.clone()
                    }
                })
                .collect(),
        );
        self.write_config(&next).await?;
        self.delete_state(bot_id).await?;
        self.schedule_refresh();
        for key in ["credentialRef", "webhookSecretRef"] {
            if let Some(reference) = text(&bot, key) {
                self.credentials
                    .delete(reference)
                    .await
                    .map_err(|error| error.to_rpc())?;
            }
        }
        Ok(next_bot)
    }

    async fn delete_state(&self, key: &str) -> Result<(), RpcError> {
        let mut state = self.repo.read_state().await.map_err(anyhow_error)?;
        if let Some(bots) = state["bots"].as_object_mut() {
            bots.shift_remove(key);
        }
        self.repo.write_state(&state).await.map_err(anyhow_error)?;
        Ok(())
    }

    pub async fn delete_bot(self: &Arc<Self>, bot_id: &str) -> Result<(), RpcError> {
        let config = self.read_config().await?;
        let bot = find_bot(&config, bot_id);
        if bot
            .as_ref()
            .is_some_and(|bot| is_feishu(text(bot, "provider").unwrap_or_default()))
        {
            self.stop_websocket(bot_id).await;
        }
        let mut next = config.clone();
        next["bots"] = Value::Array(
            config["bots"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| text(item, "id") != Some(bot_id))
                .cloned()
                .collect(),
        );
        self.write_config(&next).await?;
        self.delete_state(bot_id).await?;
        for key in ["credentialRef", "webhookSecretRef"] {
            if let Some(reference) = bot.as_ref().and_then(|bot| text(bot, key)) {
                self.credentials
                    .delete(reference)
                    .await
                    .map_err(|error| error.to_rpc())?;
            }
        }
        Ok(())
    }

    pub async fn test_bot(&self, bot_id: &str) -> Result<Value, RpcError> {
        let config = self.read_config().await?;
        let Some(bot) = find_bot(&config, bot_id) else {
            return Ok(json!({"ok": false, "message": "Bot not found."}));
        };
        let provider = text(&bot, "provider").unwrap_or_default();
        let result = match provider {
            "feishu" | "lark" => {
                if bot["enabled"] != true {
                    json!({"ok": false, "message": "Feishu bot is disabled."})
                } else if text(&bot, "feishuAppId").is_none()
                    || text(&bot, "credentialRef").is_none()
                {
                    json!({"ok": false, "message": "Feishu App ID and App Secret are required."})
                } else {
                    match self.tenant_token(&bot).await.map_err(anyhow_error)? {
                        Some(_) => {
                            json!({"ok": true, "message": "Feishu app credentials are valid."})
                        }
                        None => {
                            json!({"ok": false, "message": "Feishu app credentials are missing."})
                        }
                    }
                }
            }
            "discord" | "wecom" => {
                return Ok(json!({
                    "ok": false,
                    "message": format!("{provider} is reserved for a future version."),
                    "provider": provider,
                }));
            }
            _ => return Err(not_implemented(provider)),
        };
        let mut result = result;
        result["provider"] = json!(provider);
        Ok(result)
    }

    pub async fn create_bind_code(&self, params: &Value) -> Result<Value, RpcError> {
        let config = self.read_config().await?;
        let bot_id = text(params, "botId")
            .filter(|id| !id.is_empty())
            .ok_or_else(|| error("Bot id is required."))?;
        if find_bot(&config, bot_id).is_none() {
            return Err(error(format!("Bot not found: {bot_id}")));
        }
        let code = random_code();
        let ttl = params["ttlMs"].as_i64().unwrap_or(BOT_BIND_CODE_TTL_MS);
        let expires_at = now_ms() + ttl;
        let allowed = match params
            .get("allowedWorkspaces")
            .filter(|value| !value.is_null())
        {
            Some(value) => string_list(value),
            None => vec![ALL_BOT_WORKSPACES.to_owned()],
        };
        guard(&self.bind_codes).insert(
            code.clone(),
            BindCode {
                bot_id: bot_id.to_owned(),
                allowed_workspaces: normalize_allowed_workspaces(&allowed),
                expires_at,
            },
        );
        Ok(json!({"code": code, "expiresAt": expires_at}))
    }

    /// `watchAutomationRun`：只对已启用、通道一致的 Bot 回推终态摘要。
    pub async fn watch_automation_run(self: &Arc<Self>, params: &Value) {
        let target = &params["target"];
        let provider = text(target, "provider").unwrap_or_default();
        let bot_id = text(target, "botId").unwrap_or_default();
        let config = match self.read_config().await {
            Ok(config) => config,
            Err(error) => {
                log::warn(format!(
                    "automation Bot delivery skipped: {}",
                    error.message
                ));
                return;
            }
        };
        let reason = match find_bot(&config, bot_id) {
            None => Some("bot_missing"),
            Some(bot) if bot["enabled"] != true => Some("bot_disabled"),
            Some(bot) if text(&bot, "provider") != Some(provider) => Some("provider_mismatch"),
            Some(bot) => {
                let actor = super::feishu::Actor {
                    provider: provider.to_owned(),
                    bot_id: bot_id.to_owned(),
                    provider_user_id: text(target, "providerUserId")
                        .unwrap_or_default()
                        .to_owned(),
                    chat_type: text(target, "chatType").unwrap_or("private").to_owned(),
                    ..super::feishu::Actor::default()
                };
                let workspace = crate::agent::WorkspaceTarget {
                    workspace_path: text(params, "workspacePath").unwrap_or_default().to_owned(),
                    workspace_identity: text(params, "workspaceIdentity").map(str::to_owned),
                    remote_session_id: None,
                };
                let task_id = text(params, "taskId").unwrap_or_default().to_owned();
                self.watch_task(bot, actor, workspace, task_id, None);
                None
            }
        };
        if let Some(reason) = reason {
            // 同一原因 5 分钟内只告警一次。
            let key = format!("{provider}:{bot_id}:{reason}");
            let now = now_ms();
            let mut warnings = guard(&self.delivery_warnings);
            if warnings.get(&key).is_none_or(|at| now - at >= 5 * 60_000) {
                warnings.insert(key, now);
                log::warn(format!(
                    "automation Bot delivery skipped provider={provider} bot={bot_id} reason={reason}"
                ));
            }
        }
    }
}

pub(crate) fn not_implemented(provider: &str) -> RpcError {
    RpcError::new(
        "NotImplemented",
        format!("bots: the {provider} channel is not implemented in the Rust Host yet"),
    )
    .with_code(zcode_host_kit::NOT_IMPLEMENTED_CODE)
}

fn arg(args: &Args, index: usize) -> Value {
    args.raw(index).cloned().unwrap_or(Value::Null)
}

type Handler = fn(Arc<Bots>, Args) -> BoxFuture<'static, Result<Reply, RpcError>>;

pub fn channel(bots: Arc<Bots>) -> ServiceBuilder {
    let members: [(&str, Handler); 20] = [
        ("syncAppRuntimePreferences", |_, _| {
            Box::pin(async { Ok(Reply::Undefined) })
        }),
        ("getStatus", |bots, _| {
            Box::pin(async move { bots.get_status().await.map(Reply::Value) })
        }),
        ("getConfig", |bots, _| {
            Box::pin(async move { bots.read_config().await.map(Reply::Value) })
        }),
        ("listWorkspaceRefs", |bots, args| {
            Box::pin(async move {
                Ok(Reply::Value(json!(
                    bots.workspace_refs(&arg(&args, 0)).await
                )))
            })
        }),
        ("getUserConfigOptions", |_, _| {
            Box::pin(async { Ok(Reply::Value(json!([]))) })
        }),
        ("beginFeishuRegistration", |bots, args| {
            Box::pin(async move {
                let params = arg(&args, 0);
                bots.api
                    .begin_registration(text(&params, "domain"))
                    .await
                    .map(Reply::Value)
                    .map_err(anyhow_error)
            })
        }),
        ("pollFeishuRegistration", |bots, args| {
            Box::pin(async move {
                bots.api
                    .poll_registration(&arg(&args, 0))
                    .await
                    .map(Reply::Value)
                    .map_err(anyhow_error)
            })
        }),
        ("saveConfig", |bots, args| {
            Box::pin(async move { bots.save_config(&arg(&args, 0)).await.map(Reply::Value) })
        }),
        ("listBots", |bots, _| {
            Box::pin(async move { Ok(Reply::Value(bots.read_config().await?["bots"].clone())) })
        }),
        ("saveBot", |bots, args| {
            Box::pin(async move { bots.save_bot(&arg(&args, 0)).await.map(Reply::Value) })
        }),
        ("removeBotSecret", |bots, args| {
            Box::pin(async move {
                let id = args.get::<String>(0, "botId")?;
                bots.remove_bot_secret(&id).await.map(Reply::Value)
            })
        }),
        ("deleteBot", |bots, args| {
            Box::pin(async move {
                let id = args.get::<String>(0, "botId")?;
                bots.delete_bot(&id).await.map(|()| Reply::Undefined)
            })
        }),
        ("testBot", |bots, args| {
            Box::pin(async move {
                let id = args.get::<String>(0, "botId")?;
                bots.test_bot(&id).await.map(Reply::Value)
            })
        }),
        ("createBindCode", |bots, args| {
            Box::pin(async move {
                bots.create_bind_code(&arg(&args, 0))
                    .await
                    .map(Reply::Value)
            })
        }),
        ("getBotStates", |bots, _| {
            Box::pin(async move {
                let state = bots.repo.read_state().await.map_err(anyhow_error)?;
                let values: Vec<Value> = state["bots"]
                    .as_object()
                    .map(|bots| bots.values().cloned().collect())
                    .unwrap_or_default();
                Ok(Reply::Value(Value::Array(values)))
            })
        }),
        ("resetBotState", |bots, args| {
            Box::pin(async move {
                let key = args.get::<String>(0, "contextKey")?;
                bots.delete_state(&key).await.map(|()| Reply::Undefined)
            })
        }),
        ("watchAutomationRun", |bots, args| {
            Box::pin(async move {
                bots.watch_automation_run(&arg(&args, 0)).await;
                Ok(Reply::Undefined)
            })
        }),
        ("handleInboundMessage", |bots, args| {
            Box::pin(async move {
                let message = arg(&args, 0);
                let inbound = super::inbound::inbound_from_value(&message)
                    .ok_or_else(|| error("Invalid bot inbound message"))?;
                bots.handle_inbound(&inbound)
                    .await
                    .map(|replies| Reply::Value(json!(replies)))
            })
        }),
        ("handleProviderCallback", |bots, args| {
            Box::pin(async move {
                let provider = args.get::<String>(0, "provider")?;
                let result = bots.provider_callback(&provider, arg(&args, 1)).await?;
                Ok(Reply::Value(result["replies"].clone()))
            })
        }),
        ("handleProviderCallbackResponse", |bots, args| {
            Box::pin(async move {
                let provider = args.get::<String>(0, "provider")?;
                bots.provider_callback(&provider, arg(&args, 1))
                    .await
                    .map(Reply::Value)
            })
        }),
    ];
    let mut builder = ServiceBuilder::new("bots");
    for (name, handler) in members {
        let bots = bots.clone();
        builder = builder.method(name, move |_, args| handler(bots.clone(), args));
    }
    builder
}
