//! 入站消息处理（botsService.ts `handleInboundMessage`、`processProviderCallback`、授权、上下文、
//! `/bind`、`/help`、`/status`、`/new`、`/stop`、`/reconnect`、普通消息与附件）。
//!
//! 选择命令在 [`super::selection`]，问答与权限在 [`super::interaction`]，回复流水线在 [`super::watch`]。
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use base64::Engine;
use serde_json::{Map, Value, json};
use zcode_host_kit::{RpcError, log};

use super::TaskPort;
use super::feishu::{Actor, Inbound};
use super::model::{
    self, ZCODE_AGENT_PROVIDER, filter_allowed_workspaces, is_feishu, is_workspace_allowed,
    message as msg, normalize_allowed_workspaces, normalize_command_policy,
    normalize_configured_allowed_workspaces, normalize_reply_mode, string_list, workspace_key,
    workspace_label, workspace_ref,
};
use super::now_ms;
use super::providers::{OutboundExtras, outbound};
use super::selection::{
    is_draft, parse_model_option, select_current, select_option, selection_value,
};
use super::service::{Bots, find_bot, guard};
use super::status::{format_duration, latest_progress, task_status, worked_duration};
use super::util::{js_slice, sha256_hex, text};
use super::watch::{context_target, strip_pending};
use crate::agent::WorkspaceTarget;
use crate::automation::driver::{SendText, uuid_v7};

const INBOUND_DEDUPE_TTL_MS: i64 = 2 * 60_000;
/// 同一聊天与 workspace 的 `/reconnect` 结束后 3 s 内不再执行（`REMOTE_RECONNECT_DEDUPE_TTL_MS`）。
const RECONNECT_DEDUPE_TTL_MS: i64 = 3_000;
/// 同一条 provider 消息的 `/reconnect` 2 分钟内只执行一次（`REMOTE_RECONNECT_DELIVERY_DEDUPE_TTL_MS`）。
const RECONNECT_DELIVERY_DEDUPE_TTL_MS: i64 = 2 * 60_000;

/// `/reconnect` 的两层幂等状态（Node `recentRemoteReconnectDeliveryAtByKey`、
/// `pendingRemoteReconnectsByKey`、`recentRemoteReconnectAtByKey`）。
#[derive(Default)]
pub(crate) struct Reconnects {
    /// provider 消息投递键 → 登记时间。
    delivered: HashMap<String, i64>,
    /// 命令键（bot + provider + 聊天 + workspace）→ (登记 id, 在途重连)，结束时置 true。
    pending: HashMap<String, (u64, tokio::sync::watch::Receiver<bool>)>,
    /// 命令键 → 上次重连结束的时间。
    recent: HashMap<String, i64>,
    next_id: u64,
}

/// 一次在途重连的登记：结束（包括调用方被取消）时移除并唤醒等待者，不会留下永远“在途”的键。
struct PendingReconnect<'a> {
    bots: &'a Bots,
    key: String,
    id: u64,
    done: tokio::sync::watch::Sender<bool>,
}

impl Drop for PendingReconnect<'_> {
    fn drop(&mut self) {
        {
            let mut reconnects = guard(&self.bots.reconnects);
            if reconnects
                .pending
                .get(&self.key)
                .is_some_and(|(id, _)| *id == self.id)
            {
                reconnects.pending.remove(&self.key);
            }
        }
        self.done.send_replace(true);
    }
}
/// Bot 硬锁 yolo 运行模式。
const BOT_FORCED_MODE: &str = "yolo";
const MAX_ATTACHMENTS: usize = 4;
const MAX_ATTACHMENT_BYTES: usize = 5 * 1024 * 1024;
/// `BOT_MENU_COMMAND_ORDER` 与帮助文案。
const HELP_COMMANDS: [(&str, &str); 9] = [
    ("help", "helpHelp"),
    ("status", "helpStatus"),
    ("new", "helpNew"),
    ("workspace", "helpWorkspace"),
    ("model", "helpModel"),
    ("mode", "helpMode"),
    ("thoughtLevel", "helpThoughtLevel"),
    ("reply", "helpReply"),
    ("bind", "helpBind"),
];

/// `withAuthorizedContext` 的结果。
pub(crate) struct Auth {
    pub config: Value,
    pub bot: Value,
    /// Bot 配置化后用户就是 Bot 本身（`findBoundUser`）。
    pub user: Value,
    pub context: Value,
    pub locale: Option<String>,
}

/// `requireActiveTask` 的结果。
pub(crate) struct Active {
    pub task_id: String,
    pub task: Value,
    pub config_options: Value,
}

impl Active {
    /// `readCurrentActiveTaskModel`：会话 configOptions 的模型，其次任务元数据。
    pub fn current_model(&self) -> Option<String> {
        select_current(&self.config_options, "model")
            .or_else(|| text(&self.task, "model").map(str::to_owned))
            .filter(|model| !model.is_empty())
    }

    /// 任务所在 workspace（读模型视图、标签用）。
    pub fn context(&self) -> Value {
        let mut context = Map::new();
        context.insert("workspacePath".into(), self.task["workspacePath"].clone());
        if let Some(identity) = text(&self.task, "workspaceIdentity") {
            context.insert("workspaceIdentity".into(), json!(identity));
        }
        Value::Object(context)
    }
}

/// `BotInboundMessage` JSON → [`Inbound`]。
pub fn inbound_from_value(value: &Value) -> Option<Inbound> {
    let actor = &value["actor"];
    let string = |value: &Value, key: &str| text(value, key).map(str::to_owned);
    Some(Inbound {
        bot_id: string(value, "botId")?,
        text: string(value, "text").unwrap_or_default(),
        attachments: value["attachments"].as_array().cloned().unwrap_or_default(),
        elicitation_response: value
            .get("elicitationResponse")
            .filter(|response| response.is_object())
            .cloned(),
        actor: Actor {
            provider: string(actor, "provider")?,
            bot_id: string(actor, "botId")?,
            provider_user_id: string(actor, "providerUserId")?,
            display_name: string(actor, "displayName"),
            chat_type: string(actor, "chatType").unwrap_or_else(|| "private".into()),
            chat_id: string(actor, "chatId"),
            provider_message_id: string(actor, "providerMessageId"),
            provider_context_token: string(actor, "providerContextToken"),
        },
    })
}

fn context_key(context: &Value) -> String {
    workspace_key(
        text(context, "workspacePath").unwrap_or_default(),
        text(context, "workspaceIdentity"),
    )
}

/// `formatStatusStateValue`。
fn state_label(locale: Option<&str>, state: &str) -> String {
    if locale == Some("en-US") {
        return state.to_owned();
    }
    let id = match state {
        "draft" => "statusDraft",
        "remote disconnected" => "statusRemoteDisconnected",
        "running" => "streamingStatusRunning",
        "completed" => "streamingStatusCompleted",
        "error" | "failed" => "streamingStatusFailed",
        "cancelled" => "statusCancelled",
        "stopped" => "statusStopped",
        _ => return state.to_owned(),
    };
    msg(locale, id, &[])
}

fn line(locale: Option<&str>, label: &str, value: &str) -> String {
    format!("{}: {value}", msg(locale, label, &[]))
}

fn plain(actor: &Actor, body: String) -> Vec<Value> {
    vec![outbound(actor, body, OutboundExtras::default())]
}

/// `isSessionExpiredError`。
fn is_session_expired(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("session not found:") || lower.contains("session is not active:")
}

/// `requiresRemoteWorkspaceRuntime`。
fn requires_remote_runtime(command: &str) -> bool {
    !matches!(
        command,
        "help" | "status" | "workspace" | "reconnect" | "reply"
    )
}

/// `isUserCommandAllowed`。
fn command_allowed(bot: &Value, command: &str) -> bool {
    match command {
        "help" | "message" | "approve" | "task" | "stop" => true,
        "reconnect" => normalize_command_policy(&bot["allowedCommands"])["workspace"] != false,
        other => normalize_command_policy(&bot["allowedCommands"])[other] != false,
    }
}

/// `sanitizeAttachmentFilename`。
fn sanitize_filename(filename: &str) -> String {
    let normalized: String = filename
        .trim()
        .chars()
        .map(|c| {
            if (c as u32) < 32 || "\\/:*?\"<>|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    if normalized.is_empty() {
        "attachment".into()
    } else {
        js_slice(&normalized, 0, 160)
    }
}

/// `formatAttachmentSize`。
fn attachment_size(size: Option<f64>) -> String {
    match size.filter(|size| *size > 0.0) {
        None => "unknown size".into(),
        Some(size) if size >= 1024.0 * 1024.0 => format!("{:.1}MB", size / (1024.0 * 1024.0)),
        Some(size) if size >= 1024.0 => format!("{}KB", (size / 1024.0).ceil() as i64),
        Some(size) => format!("{}B", size as i64),
    }
}

/// `formatAttachmentRejectedReason`：下载类错误换成可行动的提示，不暴露通道内部细节。
fn attachment_rejected_reason(message: &str, locale: Option<&str>) -> String {
    let lower = message.to_lowercase();
    if lower.contains("exceeds 5mb") {
        return msg(locale, "attachmentTooLarge", &[]);
    }
    let download_failed = lower.contains("attachment download failed")
        || lower.contains("file download failed")
        || lower.contains("attachment download timed out")
        || lower.contains("file download timed out")
        || (lower.starts_with("download ")
            && (lower.contains(" failed: http") || lower.contains(" timed out")));
    if download_failed {
        return msg(locale, "attachmentDownloadUnavailable", &[]);
    }
    message.to_owned()
}

/// `resolveAutomationBotDeliveryTarget`：会话里创建的 automation 默认回推到这个聊天。
fn automation_target(actor: &Actor) -> Option<Value> {
    if !matches!(actor.provider.as_str(), "feishu" | "lark" | "weixin") {
        return None;
    }
    let user = actor
        .chat_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .unwrap_or(actor.provider_user_id.trim());
    (!user.is_empty()).then(|| {
        json!({
            "provider": actor.provider,
            "botId": actor.bot_id,
            "providerUserId": user,
            "chatType": actor.chat_type,
        })
    })
}

impl Bots {
    /// 上下文对应的任务能力：本地 workspace 用本 Host；远端只用远端运行时，不能静默落到本地。
    pub(crate) fn tasks_for(&self, target: &WorkspaceTarget) -> Result<Arc<dyn TaskPort>, String> {
        if target
            .workspace_identity
            .as_deref()
            .is_some_and(|id| !id.trim().is_empty())
        {
            return self.remote.tasks(target).ok_or_else(|| {
                format!(
                    "当前远端项目 {} runtime 不可用，请发送 **/重连** 后重试。",
                    target.workspace_path
                )
            });
        }
        Ok(self.tasks.clone())
    }

    /// `isRemoteWorkspaceConnected`：本地恒为已连接；远端没有重连服务时视为未连接。
    async fn remote_connected(&self, context: &Value) -> bool {
        if text(context, "workspaceIdentity").is_none() {
            return true;
        }
        if !self.remote.available() {
            return false;
        }
        self.remote.is_connected(context_target(context)).await
    }

    /// `listWorkspaceRefs`：当前 workspace + settings.lastWorkspaceSession（按身份 key 去重）。
    pub async fn workspace_refs(&self, params: &Value) -> Vec<Value> {
        let mut refs: Vec<Value> = Vec::new();
        let mut push = |workspace: Value| {
            let key = workspace_key(
                text(&workspace, "workspacePath").unwrap_or_default(),
                text(&workspace, "workspaceIdentity"),
            );
            match refs.iter_mut().find(|existing| {
                workspace_key(
                    text(existing, "workspacePath").unwrap_or_default(),
                    text(existing, "workspaceIdentity"),
                ) == key
            }) {
                Some(existing) => *existing = workspace,
                None => refs.push(workspace),
            }
        };
        if let Some(current) = params
            .get("currentWorkspace")
            .filter(|value| value.is_object())
        {
            push(current.clone());
        }
        let settings = self.settings.get().await;
        for entry in settings["lastWorkspaceSession"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let Some(path) = text(entry, "workspacePath") else {
                continue;
            };
            let identity = (text(entry, "kind") == Some("remote"))
                .then(|| text(entry, "workspaceIdentity"))
                .flatten();
            push(workspace_ref(path, identity));
        }
        refs
    }

    /// `resolveCanonicalContextWorkspace`。
    fn canonical_workspace(context: &Value, workspaces: &[Value]) -> Option<Value> {
        let key = context_key(context);
        if let Some(exact) = workspaces.iter().find(|workspace| {
            workspace_key(
                text(workspace, "workspacePath").unwrap_or_default(),
                text(workspace, "workspaceIdentity"),
            ) == key
        }) {
            return Some(exact.clone());
        }
        if let Some(id) = text(context, "workspaceId")
            && let Some(found) = workspaces
                .iter()
                .find(|workspace| text(workspace, "id") == Some(id))
        {
            return Some(found.clone());
        }
        if text(context, "workspaceIdentity").is_some() {
            return None;
        }
        let path = text(context, "workspacePath");
        let same: Vec<&Value> = workspaces
            .iter()
            .filter(|workspace| text(workspace, "workspacePath") == path)
            .collect();
        (same.len() == 1).then(|| same[0].clone())
    }

    /// `buildInitializedDraftOptions`：远端未连接时只给最小默认值，不偷偷申请远端运行时。
    pub(crate) async fn initialized_draft(&self, context: &Value) -> Value {
        if text(context, "workspaceIdentity").is_some() && !self.remote_connected(context).await {
            json!({"provider": ZCODE_AGENT_PROVIDER})
        } else {
            json!({"provider": ZCODE_AGENT_PROVIDER, "mode": BOT_FORCED_MODE})
        }
    }

    /// `readContext`：已有状态时按最新 workspace 列表自愈身份；没有时取第一个允许的 workspace（不落盘）。
    async fn read_context(&self, bot: &Value) -> Result<Option<Value>> {
        let state = self.repo.read_state().await?;
        let bot_id = text(bot, "id").unwrap_or_default();
        let workspaces = self.workspace_refs(&Value::Null).await;
        if let Some(existing) = state["bots"].get(bot_id).cloned() {
            let Some(canonical) = Self::canonical_workspace(&existing, &workspaces) else {
                return Ok(Some(existing));
            };
            let current_key = context_key(&existing);
            let next_id = match text(&existing, "workspaceId") {
                Some(id) if id != current_key => json!(id),
                _ => canonical["id"].clone(),
            };
            let mut next = existing.clone();
            next["workspacePath"] = canonical["workspacePath"].clone();
            match canonical.get("workspaceIdentity") {
                Some(identity) if !identity.is_null() => {
                    next["workspaceIdentity"] = identity.clone()
                }
                _ => {
                    next.as_object_mut()
                        .map(|object| object.shift_remove("workspaceIdentity"));
                }
            }
            next["workspaceId"] = next_id;
            if next == existing {
                return Ok(Some(existing));
            }
            self.write_context(&next).await?;
            return Ok(Some(next));
        }
        let allowed = string_list(&bot["allowedWorkspaces"]);
        let Some(workspace) = filter_allowed_workspaces(&workspaces, &allowed)
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        let mut context = Map::new();
        context.insert("botId".into(), json!(bot_id));
        context.insert("workspacePath".into(), workspace["workspacePath"].clone());
        if let Some(identity) = text(&workspace, "workspaceIdentity") {
            context.insert("workspaceIdentity".into(), json!(identity));
        }
        context.insert("workspaceId".into(), workspace["id"].clone());
        context.insert("mode".into(), json!("draft"));
        context.insert("activeTaskId".into(), Value::Null);
        let mut context = Value::Object(context);
        context["draftOptions"] = self.initialized_draft(&context).await;
        context["updatedAt"] = json!(now_ms());
        Ok(Some(context))
    }

    pub(crate) async fn write_context(&self, context: &Value) -> Result<()> {
        let mut state = self.repo.read_state().await?;
        let mut next = strip_pending(context);
        next["updatedAt"] = json!(now_ms());
        let id = text(context, "botId").unwrap_or_default().to_owned();
        if let Some(bots) = state["bots"].as_object_mut() {
            bots.insert(id, next);
        }
        self.repo.write_state(&state).await?;
        Ok(())
    }

    /// `writeDraftContext`：进入新任务草稿，清掉旧任务、待答交互与该 Bot 的待选项。
    pub(crate) async fn write_draft_context(
        &self,
        context: &Value,
        draft: Option<Value>,
    ) -> Result<Value> {
        let mut next = context.as_object().cloned().unwrap_or_default();
        next.insert("mode".into(), json!("draft"));
        next.insert("activeTaskId".into(), Value::Null);
        for key in [
            "draftOptions",
            "pendingPermissionOptions",
            "pendingElicitation",
        ] {
            next.shift_remove(key);
        }
        if let Some(draft) = draft {
            next.insert("draftOptions".into(), draft);
        }
        let next = Value::Object(next);
        self.clear_pending_for_bot(text(context, "botId").unwrap_or_default());
        self.write_context(&next).await?;
        Ok(next)
    }

    /// `ensureDraftOptions`：草稿没有选项时按默认值初始化并写回。
    pub(crate) async fn ensure_draft_options(&self, context: &Value) -> Result<Value> {
        if let Some(draft) = context
            .get("draftOptions")
            .filter(|draft| draft.is_object())
        {
            // 旧三方 CLI 的草稿 provider 统一归一成 ZCode Agent。
            if draft["provider"] != ZCODE_AGENT_PROVIDER {
                let mut normalized = draft.clone();
                normalized["provider"] = json!(ZCODE_AGENT_PROVIDER);
                let mut next = context.clone();
                next["draftOptions"] = normalized.clone();
                self.write_context(&next).await?;
                return Ok(normalized);
            }
            return Ok(draft.clone());
        }
        let draft = self.initialized_draft(context).await;
        let mut next = context.clone();
        next["draftOptions"] = draft.clone();
        self.write_context(&next).await?;
        Ok(draft)
    }

    /// `writeDraftOptions`。
    pub(crate) async fn write_draft_options(&self, context: &Value, draft: Value) -> Result<Value> {
        let mut draft = draft;
        draft["provider"] = json!(ZCODE_AGENT_PROVIDER);
        let mut next = context.clone();
        next["mode"] = json!("draft");
        next["activeTaskId"] = Value::Null;
        next["draftOptions"] = draft;
        self.write_context(&next).await?;
        Ok(next)
    }

    /// `normalizeBotWorkspaceConfig`：旧的 path-only 授权升级成身份 key（变化时写回配置）。返回 (bot, workspaces)。
    pub(crate) async fn normalize_workspace_config(
        &self,
        config: &Value,
        bot: &Value,
        current: Value,
    ) -> Result<(Value, Vec<Value>)> {
        let workspaces = self
            .workspace_refs(&json!({"currentWorkspace": current}))
            .await;
        let allowed = string_list(&bot["allowedWorkspaces"]);
        let next_allowed = normalize_configured_allowed_workspaces(&allowed, &workspaces);
        let mut next_bot = bot.clone();
        next_bot["allowedWorkspaces"] = json!(next_allowed);
        if next_allowed.join("\n") != allowed.join("\n") {
            let mut next = config.clone();
            if let Some(bots) = next["bots"].as_array_mut() {
                for item in bots.iter_mut() {
                    if text(item, "id") == text(bot, "id") {
                        *item = next_bot.clone();
                    }
                }
            }
            self.repo.write_config(&next).await?;
        }
        Ok((next_bot, workspaces))
    }

    /// `withAuthorizedContext`。
    pub(crate) async fn authorize(
        self: &Arc<Self>,
        inbound: &Inbound,
        command: &str,
    ) -> Result<Result<Auth, Vec<Value>>> {
        let locale = self.locale().await;
        let actor = &inbound.actor;
        let reply = |id: &str, values: &[(&str, &str)]| {
            Err(plain(actor, msg(locale.as_deref(), id, values)))
        };
        let config = self.repo.read_config().await?;
        // `findAuthorizedBot`：微信扫码登录没有用户 id，按 botId 找；其余通道按绑定的用户找。
        let bot = config["bots"].as_array().into_iter().flatten().find(|bot| {
            if actor.provider == "weixin" {
                bot["enabled"] == true
                    && text(bot, "provider") == Some("weixin")
                    && text(bot, "id") == Some(actor.bot_id.as_str())
            } else {
                bot["enabled"] == true
                    && text(bot, "provider") == Some(actor.provider.as_str())
                    && text(bot, "providerUserId") == Some(actor.provider_user_id.as_str())
            }
        });
        let Some(bot) = bot
            .filter(|bot| text(bot, "id") == Some(inbound.bot_id.as_str()))
            .cloned()
        else {
            return Ok(reply("botDisabled", &[]));
        };
        if actor.chat_type != "private" {
            return Ok(reply("privateChatOnly", &[]));
        }
        if actor.provider != "weixin"
            && text(&bot, "providerUserId") != Some(actor.provider_user_id.as_str())
        {
            return Ok(reply("userNotBound", &[]));
        }
        if !command_allowed(&bot, command) {
            return Ok(reply("commandNotAllowed", &[]));
        }
        let Some(context) = self.read_context(&bot).await? else {
            return Ok(reply("noWorkspaceAllowed", &[]));
        };
        let path = text(&context, "workspacePath")
            .unwrap_or_default()
            .to_owned();
        let mut current = Map::new();
        current.insert(
            "id".into(),
            json!(
                text(&context, "workspaceId")
                    .map(str::to_owned)
                    .unwrap_or_else(|| context_key(&context))
            ),
        );
        current.insert("label".into(), json!(workspace_label(&path)));
        current.insert("workspacePath".into(), json!(path));
        if let Some(identity) = text(&context, "workspaceIdentity") {
            current.insert("workspaceIdentity".into(), json!(identity));
        }
        let (bot, _) = self
            .normalize_workspace_config(&config, &bot, Value::Object(current))
            .await?;
        if let Some(id) = text(&context, "workspaceId")
            && !is_workspace_allowed(id, &string_list(&bot["allowedWorkspaces"]))
        {
            return Ok(reply("workspaceOutOfScope", &[]));
        }
        // 远端未连接时只放行不需要运行时的命令；恢复只能由显式 /reconnect 触发。
        if text(&context, "workspaceIdentity").is_some()
            && requires_remote_runtime(command)
            && !self.remote_connected(&context).await
        {
            return Ok(reply("remoteDisconnected", &[("workspacePath", &path)]));
        }
        self.send_inbound_typing(&bot, actor).await;
        Ok(Ok(Auth {
            config,
            user: bot.clone(),
            bot,
            context,
            locale,
        }))
    }

    /// `isContextActiveTaskRunning`：只有本进程发起、尚未观察到终态的任务才算运行中。
    pub(crate) async fn is_active_task_running(self: &Arc<Self>, context: &Value) -> bool {
        let Some(task_id) = text(context, "activeTaskId") else {
            return false;
        };
        if !guard(&self.running).contains(task_id) {
            return false;
        }
        if text(context, "workspaceIdentity").is_some() && !self.remote_connected(context).await {
            return false;
        }
        let target = context_target(context);
        let Ok(tasks) = self.tasks_for(&target) else {
            return true;
        };
        let status = match tasks.read_session(target, task_id.to_owned()).await {
            Ok(snapshot) => crate::tasks::meta::meta_from_snapshot(&snapshot)["status"].clone(),
            Err(_) => Value::Null,
        };
        if matches!(status.as_str(), Some("completed" | "error")) {
            guard(&self.running).remove(task_id);
            self.stop_typing(task_id);
            return false;
        }
        true
    }

    /// `readContextActiveTaskMeta`：先查 tasks-index，再读会话快照。
    async fn active_task_meta(&self, context: &Value) -> Option<Value> {
        let task_id = text(context, "activeTaskId")?;
        let target = context_target(context);
        let tasks = self.tasks_for(&target).ok()?;
        if let Some(task) = tasks
            .list(target.key())
            .await
            .into_iter()
            .find(|task| text(task, "taskId") == Some(task_id))
        {
            return Some(task);
        }
        let snapshot = tasks.read_session(target, task_id.to_owned()).await.ok()?;
        Some(crate::tasks::meta::meta_from_snapshot(&snapshot))
    }

    /// `requireActiveTask`：没有活动任务时返回 None（调用方回 noActiveTask）。
    pub(crate) async fn require_active_task(
        &self,
        _actor: &Actor,
        auth: &Auth,
    ) -> Result<Option<Active>> {
        let Some(task_id) = text(&auth.context, "activeTaskId").map(str::to_owned) else {
            return Ok(None);
        };
        let Some(task) = self.active_task_meta(&auth.context).await else {
            return Ok(None);
        };
        let target = context_target(&auth.context);
        let tasks = self.tasks_for(&target).map_err(anyhow::Error::msg)?;
        let config_options = tasks
            .config_options(target, task_id.clone())
            .await
            .map_err(anyhow::Error::msg)?;
        Ok(Some(Active {
            task_id,
            task,
            config_options,
        }))
    }

    /// `buildActiveTaskDraftOptions`：`/new` 继承当前任务的模型与思考档位，模式硬锁 yolo。
    async fn active_task_draft(&self, context: &Value) -> Value {
        let task = self.active_task_meta(context).await;
        let Some(task) = task.filter(|task| {
            text(task, "provider").is_some() && text(context, "activeTaskId").is_some()
        }) else {
            return self.initialized_draft(context).await;
        };
        let target = context_target(context);
        let options = match self.tasks_for(&target) {
            Ok(tasks) => tasks
                .config_options(
                    target,
                    text(context, "activeTaskId").unwrap_or_default().to_owned(),
                )
                .await
                .unwrap_or_else(|_| json!([])),
            Err(_) => json!([]),
        };
        let forced = select_option(&options, "mode")
            .and_then(|option| option["options"].as_array())
            .is_some_and(|items| items.iter().any(|item| item["value"] == BOT_FORCED_MODE));
        let current =
            select_current(&options, "model").or_else(|| text(&task, "model").map(str::to_owned));
        // 有意与 Node 不同：Node 用 parseBotModelOptionValue 解析 `provider/model$level`，会把档位后缀
        // 留在 modelId 里，下一条消息解析模型必然失败；这里先去掉 `$level`（档位另由 thoughtLevel 带上）。
        let parsed = current
            .filter(|value| !value.is_empty())
            .map(|value| match value.split_once('$') {
                Some((model, _)) if !value.starts_with("custom:") => model.to_owned(),
                _ => value,
            })
            .and_then(|value| parse_model_option(&value));
        let level = select_current(&options, "thoughtLevel").filter(|level| !level.is_empty());
        let mut draft = Map::new();
        draft.insert("provider".into(), json!(ZCODE_AGENT_PROVIDER));
        if let Some(mut selection) = parsed {
            if let Some(level) = level {
                selection["options"] = json!({"reasoningLevel": level});
            }
            draft.insert("modelSelection".into(), selection);
        }
        if forced {
            draft.insert("mode".into(), json!(BOT_FORCED_MODE));
        }
        Value::Object(draft)
    }

    /// `buildStatusText`。
    pub(crate) async fn status_text(
        &self,
        context: &Value,
        locale: Option<&str>,
    ) -> Result<String> {
        let workspaces = self.workspace_refs(&Value::Null).await;
        let workspace_id = text(context, "workspaceId");
        let workspace = workspaces
            .iter()
            .find(|workspace| workspace_id.is_some() && text(workspace, "id") == workspace_id)
            .and_then(|workspace| text(workspace, "label").map(str::to_owned))
            .unwrap_or_else(|| {
                text(context, "workspacePath")
                    .unwrap_or_default()
                    .to_owned()
            });
        let task_id = text(context, "activeTaskId");
        if !self.remote_connected(context).await {
            let draft_model = context["draftOptions"]
                .get("modelSelection")
                .and_then(selection_value);
            let task = task_id
                .map(str::to_owned)
                .unwrap_or_else(|| msg(locale, "statusDraft", &[]));
            return Ok([
                line(locale, "statusWorkspace", &workspace),
                line(
                    locale,
                    "statusModel",
                    &self.model_label(draft_model.as_deref(), context).await,
                ),
                "------".to_owned(),
                line(locale, "statusTask", &task),
                line(
                    locale,
                    "statusState",
                    &state_label(locale, "remote disconnected"),
                ),
                msg(
                    locale,
                    "remoteDisconnectedStatus",
                    &[(
                        "workspacePath",
                        text(context, "workspacePath").unwrap_or_default(),
                    )],
                ),
            ]
            .join("\n"));
        }
        let target = context_target(context);
        let tasks = self.tasks_for(&target).map_err(anyhow::Error::msg)?;
        let listed = tasks.list(target.key()).await;
        let active = task_id.and_then(|id| {
            listed
                .iter()
                .find(|task| text(task, "taskId") == Some(id))
                .cloned()
        });
        let snapshot = match task_id {
            Some(id) => tasks.read_session(target.clone(), id.to_owned()).await.ok(),
            None => None,
        };
        let status_task = active.or_else(|| {
            snapshot
                .as_ref()
                .map(crate::tasks::meta::meta_from_snapshot)
                .filter(|meta| meta["taskId"].is_string())
        });
        let progress = match (task_id, &status_task) {
            (Some(id), task)
                if task
                    .as_ref()
                    .is_none_or(|task| task_status(task) == "running") =>
            {
                guard(&self.live_progress)
                    .get(id)
                    .map(|(_, text)| text.clone())
                    .or_else(|| snapshot.as_ref().and_then(latest_progress))
            }
            _ => None,
        };
        let worked = status_task.as_ref().and_then(|task| {
            worked_duration(snapshot.as_ref().unwrap_or(&Value::Null), task, now_ms())
        });
        let options = match task_id {
            Some(id) => tasks
                .config_options(target.clone(), id.to_owned())
                .await
                .map_err(anyhow::Error::msg)?,
            None => json!([]),
        };
        let draft = if status_task.is_none() && is_draft(context) {
            Some(self.ensure_draft_options(context).await?)
        } else {
            None
        };
        let draft_effective = match &draft {
            Some(draft) => {
                let original = draft.get("modelSelection").filter(|s| s.is_object());
                let view = tasks
                    .model_view(original.map(|selection| json!({"selection": selection})))
                    .await;
                let key = if original.is_some() {
                    "effectiveSelection"
                } else {
                    "preferredSelection"
                };
                view.get(key).and_then(selection_value)
            }
            None => None,
        };
        let model = select_current(&options, "model")
            .filter(|value| !value.is_empty())
            .or_else(|| {
                status_task
                    .as_ref()
                    .and_then(|task| text(task, "model").map(str::to_owned))
            })
            .or(draft_effective)
            .unwrap_or_else(|| "-".into());
        let mut lines = vec![
            line(locale, "statusWorkspace", &workspace),
            line(
                locale,
                "statusModel",
                &self.model_label(Some(&model), context).await,
            ),
            "------".to_owned(),
            match &status_task {
                Some(task) => format!(
                    "{}: {} ({})",
                    msg(locale, "statusTask", &[]),
                    text(task, "title").unwrap_or_default(),
                    text(task, "taskId").unwrap_or_default()
                ),
                None => line(locale, "statusTask", &msg(locale, "statusDraft", &[])),
            },
            line(
                locale,
                "statusState",
                &state_label(
                    locale,
                    &status_task
                        .as_ref()
                        .map(task_status)
                        .unwrap_or_else(|| "draft".into()),
                ),
            ),
        ];
        if let Some(worked) = worked {
            lines.push(line(locale, "statusWorked", &format_duration(worked)));
        }
        if let Some(progress) = progress {
            lines.push(line(locale, "statusProgress", &progress));
        }
        Ok(lines.join("\n"))
    }

    /// `buildHelpText`。
    pub(crate) fn help_text(locale: Option<&str>, bot: &Value) -> String {
        let mut lines = vec![msg(locale, "helpTitle", &[])];
        for (command, id) in HELP_COMMANDS {
            if command != "help" && command != "bind" && bot["allowedCommands"][command] == false {
                continue;
            }
            lines.push(msg(locale, id, &[]));
        }
        lines.join("\n")
    }

    async fn handle_bind(&self, inbound: &Inbound, code: &str) -> Result<Vec<Value>> {
        let locale = self.locale().await;
        let locale = locale.as_deref();
        let actor = &inbound.actor;
        if actor.chat_type != "private" {
            return Ok(plain(actor, msg(locale, "bindPrivateOnly", &[])));
        }
        let key = code.trim().to_uppercase();
        let record = guard(&self.bind_codes).get(&key).map(|record| {
            (
                record.bot_id.clone(),
                record.allowed_workspaces.clone(),
                record.expires_at,
            )
        });
        let Some((bot_id, allowed, _)) = record
            .filter(|(bot_id, _, expires_at)| *expires_at > now_ms() && *bot_id == inbound.bot_id)
        else {
            return Ok(plain(actor, msg(locale, "bindCodeInvalid", &[])));
        };
        let config = self.repo.read_config().await?;
        let Some(bot) = find_bot(&config, &bot_id) else {
            return Ok(plain(actor, msg(locale, "bindBotMissing", &[])));
        };
        // 重新绑定覆盖旧 providerUserId：一个 Bot 同一时间只有一个沟通对象。
        let mut next = bot.clone();
        next["providerUserId"] = json!(actor.provider_user_id);
        match &actor.display_name {
            Some(name) => next["displayName"] = json!(name),
            None => {
                next.as_object_mut()
                    .map(|object| object.shift_remove("displayName"));
            }
        }
        next["allowedWorkspaces"] = json!(normalize_allowed_workspaces(&allowed));
        next["allowedCommands"] = normalize_command_policy(&bot["allowedCommands"]);
        next["replyMode"] = json!(normalize_reply_mode(
            text(&bot, "provider").unwrap_or_default(),
            text(&bot, "replyMode")
        ));
        super::service::validate(&config, &next).map_err(|error| anyhow!(error.message))?;
        let mut updated = config.clone();
        if let Some(bots) = updated["bots"].as_array_mut() {
            for item in bots.iter_mut() {
                if text(item, "id") == Some(bot_id.as_str()) {
                    *item = next.clone();
                }
            }
        }
        self.repo.write_config(&updated).await?;
        guard(&self.bind_codes).remove(&key);
        Ok(plain(
            actor,
            [
                msg(locale, "bindSuccess", &[]),
                Self::help_text(locale, &next),
            ]
            .join("\n\n"),
        ))
    }

    /// 微信首条普通消息只做激活（扫码登录拿不到用户 id，第一条入站消息建立会话目标）。
    async fn weixin_activation(
        self: &Arc<Self>,
        inbound: &Inbound,
        command: &Value,
    ) -> Result<Option<Vec<Value>>> {
        if inbound.actor.provider != "weixin" || command["type"] != "message" {
            return Ok(None);
        }
        let state = self.repo.read_state().await?;
        let existing = &state["bots"][&inbound.bot_id];
        let initialized = [
            "weixinActivatedAt",
            "draftOptions",
            "activeTaskId",
            "pendingPermissionOptions",
            "pendingElicitation",
        ]
        .iter()
        .any(|key| !existing[*key].is_null());
        if initialized {
            return Ok(None);
        }
        let auth = match self.authorize(inbound, "help").await? {
            Ok(auth) => auth,
            Err(reply) => return Ok(Some(reply)),
        };
        if !auth.context["weixinActivatedAt"].is_null() {
            return Ok(None);
        }
        let mut context = auth.context.clone();
        context["weixinActivatedAt"] = json!(now_ms());
        self.write_context(&context).await?;
        let locale = auth.locale.as_deref();
        Ok(Some(plain(
            &inbound.actor,
            [
                msg(locale, "weixinActivatedWelcome", &[]),
                Self::help_text(locale, &auth.bot),
            ]
            .join("\n\n"),
        )))
    }

    /// 入站命令分发（`handleInboundMessage`）；同一聊天上下文串行处理。
    pub async fn handle_inbound(
        self: &Arc<Self>,
        inbound: &Inbound,
    ) -> Result<Vec<Value>, RpcError> {
        let queue_key = self.actor_key(&inbound.actor);
        let queue = guard(&self.inbound_queues)
            .entry(queue_key)
            .or_default()
            .clone();
        let _turn = queue.lock().await;
        self.dispatch(inbound)
            .await
            .map_err(|error| super::service::error(format!("{error:#}")))
    }

    async fn dispatch(self: &Arc<Self>, inbound: &Inbound) -> Result<Vec<Value>> {
        let actor = &inbound.actor;
        if let Some(response) = &inbound.elicitation_response {
            return Ok(match self.authorize(inbound, "message").await? {
                Ok(auth) => {
                    self.handle_structured_elicitation(&auth, actor, response)
                        .await?
                }
                Err(reply) => reply,
            });
        }
        let parsed = model::parse_command(&inbound.text);
        let command = if parsed["type"] == "message" {
            self.resolve_pending_command(actor, text(&parsed, "text").unwrap_or_default())
                .unwrap_or(parsed)
        } else if parsed["type"] == "selection.cancel"
            && actor.provider != "weixin"
            && inbound.text.trim() == "0"
        {
            // 非微信通道里纯文本 "0" 不是取消，而是普通消息。
            self.clear_pending(actor);
            json!({"type": "message", "text": inbound.text})
        } else {
            parsed
        };
        if let Some(reply) = self.weixin_activation(inbound, &command).await? {
            return Ok(reply);
        }
        let kind = text(&command, "type").unwrap_or_default().to_owned();
        let value = text(&command, "value").unwrap_or_default().to_owned();
        macro_rules! authorized {
            ($command:expr) => {
                match self.authorize(inbound, $command).await? {
                    Ok(auth) => auth,
                    Err(reply) => return Ok(reply),
                }
            };
        }
        match kind.as_str() {
            "selection.cancel" => self.handle_selection_cancel(inbound).await,
            "bind" => {
                self.handle_bind(inbound, text(&command, "code").unwrap_or_default())
                    .await
            }
            "help" => {
                let auth = authorized!("help");
                Ok(plain(
                    actor,
                    Self::help_text(auth.locale.as_deref(), &auth.bot),
                ))
            }
            "status" => {
                let auth = authorized!("status");
                self.status_reply(actor, &auth.context, auth.locale.as_deref())
                    .await
            }
            "reconnect" => self.handle_reconnect(inbound).await,
            "new" => {
                let auth = authorized!("new");
                let locale = auth.locale.as_deref();
                if self.is_active_task_running(&auth.context).await {
                    return Ok(plain(actor, msg(locale, "taskRunning", &[])));
                }
                let draft = self.active_task_draft(&auth.context).await;
                let context = self.write_draft_context(&auth.context, Some(draft)).await?;
                self.status_reply(actor, &context, locale).await
            }
            "workspace.list" => {
                let auth = authorized!("workspace");
                self.handle_workspace_list(inbound, auth).await
            }
            "workspace.set" => {
                let auth = authorized!("workspace");
                self.handle_workspace_set(inbound, auth, &value).await
            }
            "model.list" => {
                let auth = authorized!("model");
                self.handle_model_list(inbound, auth).await
            }
            "model.provider.set" => {
                let auth = authorized!("model");
                self.handle_model_provider_set(inbound, auth, &value).await
            }
            "model.set" => {
                let auth = authorized!("model");
                self.handle_model_set(inbound, auth, &value).await
            }
            "mode.list" | "mode.set" => {
                // Bot 硬锁 yolo：不提供模式选择，也拒绝任何切换。
                let auth = authorized!("mode");
                Ok(plain(actor, msg(auth.locale.as_deref(), "modeLocked", &[])))
            }
            "thoughtLevel.list" => {
                let auth = authorized!("thoughtLevel");
                self.handle_thought_list(inbound, auth).await
            }
            "thoughtLevel.set" => {
                let auth = authorized!("thoughtLevel");
                self.handle_thought_set(inbound, auth, &value).await
            }
            "task.list" => {
                let auth = authorized!("task");
                self.handle_task_list(inbound, auth).await
            }
            "task.set" => {
                let auth = authorized!("task");
                self.handle_task_set(inbound, auth, &value).await
            }
            "reply.list" => {
                let auth = authorized!("reply");
                self.handle_reply_list(inbound, auth).await
            }
            "reply.set" => {
                let auth = authorized!("reply");
                self.handle_reply_set(inbound, auth, &value).await
            }
            "stop" => {
                let auth = authorized!("stop");
                self.handle_stop(inbound, auth).await
            }
            "permission.respond" => {
                let auth = authorized!("approve");
                self.handle_permission_respond(&auth, inbound, &value).await
            }
            "elicitation.respond" => {
                let auth = authorized!("message");
                self.handle_elicitation_value(&auth, actor, &value).await
            }
            "elicitation.submit" => {
                let auth = authorized!("message");
                if !auth.context["pendingElicitation"].is_object() || actor.provider != "weixin" {
                    // 非微信通道的“完成”必须从带 token 的按钮进入，避免误提交上一轮问答。
                    return Ok(plain(
                        actor,
                        msg(auth.locale.as_deref(), "elicitationExpired", &[]),
                    ));
                }
                self.submit_pending_elicitation(&auth, actor, "accept")
                    .await
            }
            "approve" => {
                let auth = authorized!("approve");
                self.handle_approve(
                    &auth,
                    inbound,
                    text(&command, "requestId").unwrap_or_default(),
                    text(&command, "optionId"),
                )
                .await
            }
            "deny" => {
                let auth = authorized!("approve");
                self.handle_approve(
                    &auth,
                    inbound,
                    text(&command, "requestId").unwrap_or_default(),
                    None,
                )
                .await
            }
            "message" => self.handle_message(inbound).await,
            _ => Ok(plain(
                actor,
                msg(
                    self.locale().await.as_deref(),
                    "unknownCommand",
                    &[("command", text(&command, "name").unwrap_or_default())],
                ),
            )),
        }
    }

    /// `handleSelectionCancel`。
    async fn handle_selection_cancel(self: &Arc<Self>, inbound: &Inbound) -> Result<Vec<Value>> {
        let actor = &inbound.actor;
        let pending = self.pending_selection(actor);
        if pending
            .as_ref()
            .is_some_and(|selection| selection["action"] == "elicitation.respond")
        {
            let auth = match self.authorize(inbound, "message").await? {
                Ok(auth) => auth,
                Err(reply) => return Ok(reply),
            };
            self.clear_pending(actor);
            if !auth.context["pendingElicitation"].is_object() {
                return Ok(plain(
                    actor,
                    msg(auth.locale.as_deref(), "elicitationExpired", &[]),
                ));
            }
            return self
                .submit_pending_elicitation(&auth, actor, "cancel")
                .await;
        }
        if pending.is_none() {
            if let Ok(auth) = self.authorize(inbound, "message").await?
                && auth.context["pendingElicitation"].is_object()
            {
                return self
                    .submit_pending_elicitation(&auth, actor, "cancel")
                    .await;
            }
            let locale = self.locale().await;
            return Ok(plain(
                actor,
                msg(locale.as_deref(), "unknownCommand", &[("command", "0")]),
            ));
        }
        self.clear_pending(actor);
        let auth = match self.authorize(inbound, "message").await? {
            Ok(auth) => auth,
            Err(reply) => return Ok(reply),
        };
        self.status_reply(actor, &auth.context, auth.locale.as_deref())
            .await
    }

    /// `/reconnect`（`handleReconnect`）：本地项目不需要重连；远端交给 [`super::RemoteBots`]。顺序与两层
    /// 幂等同 Node：provider 消息投递去重 → 同聊天同 workspace 在途合并、3 s 去重 → 已连接回状态 →
    /// “正在重连” → 重连。
    async fn handle_reconnect(self: &Arc<Self>, inbound: &Inbound) -> Result<Vec<Value>> {
        let actor = &inbound.actor;
        let auth = match self.authorize(inbound, "workspace").await? {
            Ok(auth) => auth,
            Err(reply) => return Ok(reply),
        };
        let locale = auth.locale.as_deref();
        let path = text(&auth.context, "workspacePath")
            .unwrap_or_default()
            .to_owned();
        if text(&auth.context, "workspaceIdentity").is_none() {
            return Ok(plain(actor, msg(locale, "remoteReconnectLocal", &[])));
        }
        if !self.remote.available() {
            return Ok(plain(
                actor,
                msg(
                    locale,
                    "remoteReconnectUnavailable",
                    &[("workspacePath", &path)],
                ),
            ));
        }
        let now = now_ms();
        let chat = format!(
            "{}::{}::{}",
            actor.bot_id,
            actor.provider,
            actor.reply_target()
        );
        // provider 会重投同一条消息；/reconnect 有副作用，执行前就按消息 id 登记，不随失败释放。
        let delivery_key = actor
            .provider_message_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(|id| format!("{chat}::{id}"));
        let command_key = format!("{chat}::{}", context_key(&auth.context));
        let pending = {
            let mut reconnects = guard(&self.reconnects);
            reconnects
                .delivered
                .retain(|_, at| now - *at < RECONNECT_DELIVERY_DEDUPE_TTL_MS);
            reconnects
                .recent
                .retain(|_, at| now - *at < RECONNECT_DEDUPE_TTL_MS);
            if let Some(key) = &delivery_key {
                if reconnects.delivered.contains_key(key) {
                    return Ok(Vec::new());
                }
                reconnects.delivered.insert(key.clone(), now);
            }
            reconnects
                .pending
                .get(&command_key)
                .map(|(_, pending)| pending.clone())
        };
        // 同一聊天与 workspace 已在重连：等它结束，不再重复回复（避免“已连接”和“正在重连”互相打架）。
        if let Some(mut pending) = pending {
            let _ = pending.wait_for(|done| *done).await;
            return Ok(Vec::new());
        }
        if guard(&self.reconnects).recent.contains_key(&command_key) {
            return Ok(Vec::new());
        }
        if self.remote_connected(&auth.context).await {
            return self.status_reply(actor, &auth.context, locale).await;
        }
        let registration = {
            let mut reconnects = guard(&self.reconnects);
            match reconnects.pending.get(&command_key) {
                Some((_, other)) => Err(other.clone()),
                None => {
                    reconnects.next_id += 1;
                    let id = reconnects.next_id;
                    let (done, receiver) = tokio::sync::watch::channel(false);
                    reconnects
                        .pending
                        .insert(command_key.clone(), (id, receiver));
                    Ok(PendingReconnect {
                        bots: self,
                        key: command_key.clone(),
                        id,
                        done,
                    })
                }
            }
        };
        let registration = match registration {
            Ok(registration) => registration,
            Err(mut other) => {
                let _ = other.wait_for(|done| *done).await;
                return Ok(Vec::new());
            }
        };
        let result = self.perform_reconnect(inbound, &auth, &path, locale).await;
        if result.is_ok() {
            guard(&self.reconnects).recent.insert(command_key, now_ms());
        }
        drop(registration);
        result
    }

    /// “正在重连”是即时提示：在真正重连前发出，发送失败不能中断重连。
    async fn perform_reconnect(
        self: &Arc<Self>,
        inbound: &Inbound,
        auth: &Auth,
        path: &str,
        locale: Option<&str>,
    ) -> Result<Vec<Value>> {
        let actor = &inbound.actor;
        let starting = plain(
            actor,
            msg(
                locale,
                "remoteReconnectStarting",
                &[("workspacePath", path)],
            ),
        );
        if let Err(error) = self.send_outbound(&auth.bot, &starting[0]).await {
            log::warn(format!(
                "send reconnect starting failed bot={}: {error:#}",
                inbound.bot_id
            ));
        }
        match self
            .remote
            .ensure_connected(context_target(&auth.context))
            .await
        {
            Err(message) => Ok(plain(
                actor,
                msg(
                    locale,
                    "remoteReconnectFailed",
                    &[("workspacePath", path), ("message", &message)],
                ),
            )),
            Ok(()) => {
                if is_draft(&auth.context) {
                    let mut context = auth.context.clone();
                    context["draftOptions"] = self.initialized_draft(&context).await;
                    self.write_context(&context).await?;
                }
                self.status_reply(actor, &auth.context, locale).await
            }
        }
    }

    async fn handle_stop(self: &Arc<Self>, inbound: &Inbound, auth: Auth) -> Result<Vec<Value>> {
        let actor = &inbound.actor;
        let locale = auth.locale.as_deref();
        let Some(task_id) = text(&auth.context, "activeTaskId").map(str::to_owned) else {
            return Ok(plain(actor, msg(locale, "noActiveTask", &[])));
        };
        let target = context_target(&auth.context);
        let result = match self.tasks_for(&target) {
            Ok(tasks) => tasks.stop(target, task_id.clone()).await,
            Err(message) => Err(message),
        };
        if let Err(message) = result {
            return Ok(plain(
                actor,
                msg(locale, "taskFailed", &[("message", &message)]),
            ));
        }
        guard(&self.running).remove(&task_id);
        self.stop_typing(&task_id);
        self.status_reply(actor, &auth.context, locale).await
    }

    // ── 普通消息 ──

    /// 附件字节：内嵌 base64、本地路径、通道下载、下载地址，依次尝试。
    async fn attachment_bytes(
        &self,
        bot: &Value,
        attachment: &Value,
        actor: &Actor,
    ) -> Result<Option<(Value, Vec<u8>)>> {
        if let Some(data) = text(attachment, "dataBase64") {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data.trim())
                .unwrap_or_default();
            return Ok(Some((attachment.clone(), bytes)));
        }
        if let Some(path) = text(attachment, "localPath") {
            return Ok(Some((attachment.clone(), tokio::fs::read(path).await?)));
        }
        if let Some(downloaded) = self.provider_download(bot, attachment, actor).await? {
            return Ok(Some((downloaded.attachment, downloaded.data)));
        }
        let Some(url) = text(attachment, "downloadUrl") else {
            return Ok(None);
        };
        let filename = text(attachment, "filename").unwrap_or_default();
        let data = self
            .providers
            .http
            .bytes(
                self.providers.http.client().get(url),
                &format!("download {filename} failed"),
                &format!("download {filename} timed out."),
            )
            .await?;
        Ok(Some((attachment.clone(), data)))
    }

    /// `prepareBotMessageContent`：下载附件、缓存到本地；图片与音频作为 prompt 附件，其余写入路径。
    async fn prepare_content(
        &self,
        bot: &Value,
        inbound: &Inbound,
        locale: Option<&str>,
    ) -> Result<(String, Vec<Value>)> {
        let raw: Vec<Value> = inbound
            .attachments
            .iter()
            .take(MAX_ATTACHMENTS)
            .cloned()
            .collect();
        let mut prompt_attachments = Vec::new();
        let mut lines = Vec::new();
        for attachment in &raw {
            let filename = text(attachment, "filename").unwrap_or_default().to_owned();
            let mime = text(attachment, "mimeType").unwrap_or_default().to_owned();
            let Some((resolved, data)) = self
                .attachment_bytes(bot, attachment, &inbound.actor)
                .await?
            else {
                lines.push(format!(
                    "附件：{filename} ({mime}, {})，未能下载。",
                    attachment_size(attachment["sizeBytes"].as_f64())
                ));
                continue;
            };
            if data.len() > MAX_ATTACHMENT_BYTES {
                return Err(anyhow!(
                    "{} exceeds 5MB.",
                    text(&resolved, "filename").unwrap_or_default()
                ));
            }
            let message_key = inbound
                .actor
                .provider_message_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("message-{}", now_ms()));
            let digest = sha256_hex(
                format!(
                    "{}:{message_key}:{}",
                    inbound.bot_id,
                    text(&resolved, "id").unwrap_or_default()
                )
                .as_bytes(),
            );
            let filename = text(&resolved, "filename").unwrap_or_default().to_owned();
            let path = self
                .attachments_dir
                .join(sanitize_filename(&inbound.bot_id))
                .join(sanitize_filename(&message_key))
                .join(format!(
                    "{}-{}",
                    &digest[..16],
                    sanitize_filename(&filename)
                ));
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&path, &data).await?;
            let local = path.to_string_lossy().into_owned();
            let mime = text(&resolved, "mimeType").unwrap_or_default().to_owned();
            let size = attachment_size(Some(data.len() as f64));
            let kind = text(&resolved, "kind").unwrap_or("file");
            if matches!(kind, "image" | "audio") {
                prompt_attachments.push(json!({
                    "kind": kind, "filename": filename, "mimeType": mime,
                    "dataBase64": base64::engine::general_purpose::STANDARD.encode(&data),
                    "localPath": local,
                }));
                // 附件已下载缓存：把本地路径写进 prompt，避免模型再去访问临时 URL。
                lines.push(format!(
                    "附件：{filename} ({mime}, {size})，已作为{}输入提供，并保存到：{local}。如需读取附件，请直接使用这个本地路径，不要下载或访问临时/远程 URL。",
                    if kind == "image" { "图片" } else { "音频" }
                ));
                continue;
            }
            lines.push(format!(
                "附件：{filename} ({mime}, {size})，已保存到：{local}"
            ));
        }
        let trimmed = inbound.text.trim();
        let base = if !trimmed.is_empty() {
            trimmed.to_owned()
        } else if !raw.is_empty() {
            msg(locale, "attachmentOnlyPrompt", &[])
        } else {
            String::new()
        };
        let content = std::iter::once(base)
            .chain(lines)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        Ok((content, prompt_attachments))
    }

    /// 普通消息：草稿态新建会话，任务态续用当前会话；先订阅回复流，再在后台发送输入。
    async fn handle_message(self: &Arc<Self>, inbound: &Inbound) -> Result<Vec<Value>> {
        let actor = &inbound.actor;
        let mut auth = match self.authorize(inbound, "message").await? {
            Ok(auth) => auth,
            Err(reply) => return Ok(reply),
        };
        let locale = auth.locale.clone();
        let locale = locale.as_deref();
        let target = context_target(&auth.context);
        let tasks = self.tasks_for(&target).map_err(anyhow::Error::msg)?;
        let mut deleted_task: Option<String> = None;
        if auth.context["mode"] == "task"
            && let Some(task_id) = text(&auth.context, "activeTaskId").map(str::to_owned)
            && tasks
                .deleted_ids(context_key(&auth.context))
                .await
                .contains(&task_id)
        {
            // 软删除的任务不能凭 activeTaskId 续跑：回到草稿，用当前草稿的有效选择新建。
            deleted_task = Some(task_id);
            auth.context = self.write_draft_context(&auth.context, None).await?;
        }
        if let Some(reply) = self
            .handle_elicitation_text(&auth, actor, &inbound.text)
            .await?
        {
            return Ok(reply);
        }
        if auth.context["mode"] == "task"
            && text(&auth.context, "activeTaskId").is_some()
            && self.is_active_task_running(&auth.context).await
        {
            return Ok(plain(actor, msg(locale, "taskRunning", &[])));
        }
        let (content, attachments) = match self.prepare_content(&auth.bot, inbound, locale).await {
            Ok(prepared) => prepared,
            Err(error) => {
                let reason = attachment_rejected_reason(&format!("{error:#}"), locale);
                return Ok(plain(
                    actor,
                    msg(locale, "attachmentRejected", &[("message", &reason)]),
                ));
            }
        };
        let (task_id, selection, context) = if is_draft(&auth.context) {
            let draft = match auth
                .context
                .get("draftOptions")
                .filter(|value| value.is_object())
            {
                Some(draft) => draft.clone(),
                None => self.initialized_draft(&auth.context).await,
            };
            // 只在首次创建前解析原意图；创建、配置和首发都固定这份结果。
            let original = draft
                .get("modelSelection")
                .filter(|s| s.is_object())
                .cloned();
            let view = tasks
                .model_view(
                    original
                        .clone()
                        .map(|selection| json!({"selection": selection})),
                )
                .await;
            let submission = if original.is_some() {
                view["effectiveSelection"].clone()
            } else {
                view["preferredSelection"].clone()
            };
            let issue = view
                .get("selectionIssue")
                .is_some_and(|issue| !issue.is_null());
            if !submission.is_object() || (original.is_some() && issue) {
                anyhow::bail!("Bot 无法从目标 Host 解析 Submission 模型");
            }
            let mode = text(&draft, "mode").map(str::to_owned);
            let task_id = tasks
                .create(target.clone(), submission.clone(), mode)
                .await
                .map_err(|message| anyhow!(message))?;
            let mut context = auth.context.as_object().cloned().unwrap_or_default();
            context.insert("mode".into(), json!("task"));
            context.insert("activeTaskId".into(), json!(task_id));
            context.shift_remove("draftOptions");
            let context = Value::Object(context);
            self.write_context(&context).await?;
            if let Some(old) = &deleted_task {
                log::info(format!(
                    "replaced deleted Bot task bot={} oldTask={old} newTask={task_id} workspace={}",
                    text(&auth.bot, "id").unwrap_or_default(),
                    context_key(&context)
                ));
                // 切换已经持久化：通知失败不能让回调释放去重记录并重跑原消息。
                if let Err(error) = self
                    .send_outbound(
                        &auth.bot,
                        &plain(actor, msg(locale, "deletedTaskReplaced", &[]))[0],
                    )
                    .await
                {
                    log::warn(format!(
                        "deleted task replacement notice failed task={task_id}: {error:#}"
                    ));
                }
            }
            (task_id, submission, context)
        } else {
            let task_id = text(&auth.context, "activeTaskId")
                .unwrap_or_default()
                .to_owned();
            // Bot 只是同一会话的输入端：以会话原选择解析本次输入，不改写会话。
            let original = tasks
                .resume(target.clone(), task_id.clone())
                .await
                .map_err(|message| anyhow!(message))?;
            let view = match &original {
                Some(selection) => {
                    tasks
                        .model_view(Some(json!({"selection": selection})))
                        .await
                }
                None => Value::Null,
            };
            let effective = view["effectiveSelection"].clone();
            let issue = view
                .get("selectionIssue")
                .is_some_and(|issue| !issue.is_null());
            if !effective.is_object() || issue {
                anyhow::bail!("{}", msg(locale, "sessionModelUnavailable", &[]));
            }
            (task_id, effective, auth.context.clone())
        };
        guard(&self.running).insert(task_id.clone());
        // 先订阅回复流（收到初始快照）再发送，快速完成的轮次也不会漏。
        self.watch_task_stream(&auth.bot, actor, &context, &auth.user)
            .await;
        let bots = self.clone();
        let bot = auth.bot.clone();
        let actor = actor.clone();
        let input = SendText {
            input_id: uuid_v7(),
            text: content,
            model_selection: Some(selection),
            bot_delivery_target: automation_target(&actor),
            ..SendText::default()
        };
        // 发送在后台进行：轮询通道是单循环顺序处理 update，等整轮结束会让按钮回调排队。
        tokio::spawn(async move {
            let result = if attachments.is_empty() {
                tasks.send(target, task_id.clone(), input).await
            } else {
                tasks
                    .send_with_attachments(target, task_id.clone(), input, attachments)
                    .await
            };
            if let Err(message) = result {
                guard(&bots.running).remove(&task_id);
                bots.stop_typing(&task_id);
                let locale = bots.locale().await;
                let locale = locale.as_deref();
                let body = if is_session_expired(&message) {
                    msg(locale, "sessionExpiredNewTaskHint", &[])
                } else {
                    msg(locale, "taskFailed", &[("message", &message)])
                };
                if let Err(error) = bots.send_outbound(&bot, &plain(&actor, body)[0]).await {
                    log::warn(format!(
                        "bots prompt failure notice failed task={task_id}: {error:#}"
                    ));
                }
            }
        });
        Ok(Vec::new())
    }

    // ── 通道回调 ──

    fn delivery_key(inbound: &Inbound) -> Option<String> {
        let id = inbound.actor.provider_message_id.as_deref()?.trim();
        (!id.is_empty()).then(|| {
            format!(
                "{}::{}::{}::{id}",
                inbound.actor.bot_id,
                inbound.actor.provider,
                inbound.actor.reply_target()
            )
        })
    }

    /// `markInboundDelivery`：2 分钟内同一消息只处理一次。
    fn mark_inbound(&self, inbound: &Inbound) -> bool {
        let now = now_ms();
        let mut recent = guard(&self.recent_inbound);
        recent.retain(|_, at| now - *at < INBOUND_DEDUPE_TTL_MS);
        let Some(key) = Self::delivery_key(inbound) else {
            return true;
        };
        if recent.contains_key(&key) {
            return false;
        }
        recent.insert(key, now);
        true
    }

    fn release_inbound(&self, inbound: &Inbound) {
        if let Some(key) = Self::delivery_key(inbound) {
            guard(&self.recent_inbound).remove(&key);
        }
    }

    /// `processProviderCallback`：解析、校验、去重、处理并投递回复。
    pub async fn process_callback(
        self: &Arc<Self>,
        provider: &str,
        payload: Value,
        _synchronous: bool,
    ) -> Value {
        let locale = self.locale().await;
        let locale = locale.as_deref();
        let config = match self.repo.read_config().await {
            Ok(config) => config,
            Err(error) => {
                log::warn(format!(
                    "provider callback failed provider={provider}: {error:#}"
                ));
                return json!({"ok": false, "replies": [], "status": 503});
            }
        };
        let payload = if is_feishu(provider) && payload.is_object() {
            let mut object = Map::new();
            object.insert("zcodeProvider".into(), json!(provider));
            object.extend(payload.as_object().cloned().unwrap_or_default());
            Value::Object(object)
        } else {
            payload
        };
        let Some(parsed) = Self::parse_callback(provider, &payload) else {
            return json!({"ok": false, "replies": [], "status": 400});
        };
        if is_feishu(provider) {
            log::debug(format!(
                "provider callback parsed provider={provider} count={}",
                parsed.len()
            ));
        }
        let mut replies: Vec<Value> = Vec::new();
        let mut business_failure = false;
        for inbound in parsed {
            let bot = find_bot(&config, &inbound.bot_id);
            if let Some(bot) = &bot {
                let secret = if text(bot, "provider") == Some("webhook") {
                    Some(text(&payload, "webhookSecret"))
                } else if is_feishu(text(bot, "provider").unwrap_or_default()) {
                    Some(text(&payload, "token").or_else(|| text(&payload["header"], "token")))
                } else {
                    None
                };
                if let Some(received) = secret
                    && let Some(expected) = self
                        .load_credential(text(bot, "webhookSecretRef"))
                        .await
                        .filter(|expected| !expected.is_empty())
                    && received != Some(expected.as_str())
                {
                    replies.push(
                        plain(&inbound.actor, msg(locale, "webhookSecretInvalid", &[])).remove(0),
                    );
                    continue;
                }
            }
            let mut inbound = inbound;
            if let Some(bot) = &bot
                && inbound.actor.display_name.is_none()
            {
                match self
                    .provider_actor_display_name(bot, &inbound.actor.provider_user_id)
                    .await
                {
                    Ok(Some(name)) if !name.trim().is_empty() => {
                        inbound.actor.display_name = Some(name.trim().to_owned())
                    }
                    Ok(_) => {}
                    // 昵称需要额外通讯录权限，缺权限时不能阻断消息处理和绑定。
                    Err(error) => log::debug(format!(
                        "resolve actor displayName failed provider={provider} bot={} user={}: {error:#}",
                        inbound.bot_id, inbound.actor.provider_user_id
                    )),
                }
            }
            if !self.mark_inbound(&inbound) {
                log::info(format!(
                    "provider callback duplicated provider={provider} bot={} user={} messageId={}",
                    inbound.bot_id,
                    inbound.actor.provider_user_id,
                    inbound
                        .actor
                        .provider_message_id
                        .as_deref()
                        .unwrap_or_default()
                ));
                continue;
            }
            log::info(format!(
                "provider callback provider={provider} bot={} user={}",
                inbound.bot_id, inbound.actor.provider_user_id
            ));
            let outbound_messages = match self.handle_inbound(&inbound).await {
                Ok(messages) => messages,
                Err(failure) => {
                    self.release_inbound(&inbound);
                    business_failure = true;
                    log::warn(format!(
                        "provider callback failed provider={provider} bot={} user={}: {}",
                        inbound.bot_id, inbound.actor.provider_user_id, failure.message
                    ));
                    let body = if is_session_expired(&failure.message) {
                        msg(locale, "sessionExpiredNewTaskHint", &[])
                    } else {
                        msg(locale, "callbackFailed", &[("message", &failure.message)])
                    };
                    let notice = plain(&inbound.actor, body).remove(0);
                    replies.push(notice.clone());
                    // 错误提示发送成功不等于消息已消费：不确认回调，ok=false 让外部游标重投。
                    if let Some(bot) = &bot {
                        if let Err(error) = self.send_outbound(bot, &notice).await {
                            log::warn(format!(
                                "provider callback failure notice failed provider={provider}: {error:#}"
                            ));
                        }
                        self.stop_inbound_typing(bot, &inbound.actor).await;
                    }
                    continue;
                }
            };
            replies.extend(outbound_messages.iter().cloned());
            let Some(bot) = &bot else {
                continue;
            };
            if let Err(error) = self
                .deliver_replies(
                    bot,
                    provider,
                    &payload,
                    &inbound,
                    &outbound_messages,
                    locale,
                )
                .await
            {
                self.release_inbound(&inbound);
                log::warn(format!(
                    "provider callback reply failed provider={provider}: {error:#}"
                ));
                return json!({"ok": false, "replies": replies, "status": 503});
            }
        }
        let mut result = json!({"ok": !business_failure, "replies": replies});
        if business_failure {
            result["status"] = json!(503);
        }
        result
    }

    /// 回调成功后的确认与投递（Telegram toast、飞书 card/update、临时交互卡片、普通发送）。
    async fn deliver_replies(
        &self,
        bot: &Value,
        provider: &str,
        payload: &Value,
        inbound: &Inbound,
        outbound_messages: &[Value],
        locale: Option<&str>,
    ) -> Result<()> {
        let key = self.actor_key(&inbound.actor);
        let transient = guard(&self.transient_cards).get(&key).cloned();
        let first = outbound_messages.first();
        let synchronous = is_feishu(provider)
            && payload["zcodeFeishuSynchronousCardAction"] == true
            && first.is_some();
        let first_elicitation = first.is_some_and(|message| message["elicitation"].is_object());
        let callback_text = first
            .and_then(|message| text(message, "text"))
            .map(str::to_owned)
            .unwrap_or_else(|| msg(locale, "received", &[]));
        let mut handled_by_update = synchronous;
        if !synchronous && (transient.is_none() || first_elicitation) {
            match tokio::time::timeout(
                std::time::Duration::from_secs(3),
                self.provider_acknowledge(bot, payload, &callback_text, first),
            )
            .await
            {
                Ok(Ok(handled)) => handled_by_update = handled,
                Ok(Err(error)) => log::warn(format!(
                    "provider callback acknowledge failed provider={provider}: {error:#}"
                )),
                Err(_) => log::warn(format!(
                    "provider callback acknowledge failed provider={provider}: Bot provider callback acknowledgement timed out after 3000ms."
                )),
            }
        }
        if let Some(card) = &transient
            && let Some(first) = first
        {
            if !synchronous && handled_by_update && first_elicitation {
                // card/update 返回成功后客户端仍可能停在旧题：再 PATCH 同一张卡强制刷新。
                if let Err(error) = self
                    .feishu_update_card(
                        &card.bot,
                        &card.message_id,
                        &super::feishu::cards::for_message(first),
                    )
                    .await
                {
                    log::warn(format!(
                        "refresh transient interaction card failed provider={provider}: {error:#}"
                    ));
                }
            }
            if handled_by_update && first["elicitation"]["status"] != "pending" {
                guard(&self.transient_cards).remove(&key);
            }
        }
        if !handled_by_update {
            for message in outbound_messages {
                if transient.is_some() {
                    if message["selection"].is_object()
                        || message["elicitation"]["status"] == "pending"
                    {
                        self.upsert_transient_card(
                            bot,
                            &inbound.actor,
                            &transient
                                .as_ref()
                                .map(|c| c.task_id.clone())
                                .unwrap_or_default(),
                            message,
                        )
                        .await?;
                        continue;
                    }
                    let interaction_command = ["/approve", "/deny"].iter().any(|command| {
                        inbound.text.strip_prefix(command).is_some_and(|rest| {
                            rest.is_empty() || rest.starts_with(char::is_whitespace)
                        })
                    });
                    if message["elicitation"].is_object() || interaction_command {
                        self.finalize_transient_card(&inbound.actor, message).await;
                        continue;
                    }
                }
                self.send_outbound(bot, message).await?;
            }
        }
        self.stop_inbound_typing(bot, &inbound.actor).await;
        Ok(())
    }

    /// `handleProviderCallback(Response)`：payload 必须自带 botId（Webhook 由 HTTP 入口按路由补上）。
    pub async fn provider_callback(
        self: &Arc<Self>,
        provider: &str,
        payload: Value,
    ) -> Result<Value, RpcError> {
        Ok(self.process_callback(provider, payload, false).await)
    }
}
