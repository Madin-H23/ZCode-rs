//! 入站消息处理（botsService.ts `handleInboundMessage` / `processProviderCallback` 的飞书子集）。
//!
//! 已实现：`/bind`、`/help`、`/status`、`/new`、`/stop`、`/cancel`、未知命令与普通消息（新建或续用
//! 会话并在终态回推）。选择类命令（`/workspace`、`/model`、`/mode`、`/think`、`/task`、`/reply`）、
//! 权限与问答回复、`/reconnect` 尚未移植，回 `unknownCommand`。
use std::sync::Arc;

use serde_json::{Map, Value, json};
use zcode_host_kit::{RpcError, log};

use super::feishu::{Actor, Inbound, parse_callback};
use super::model::{
    self, ZCODE_AGENT_PROVIDER, filter_allowed_workspaces, is_feishu, is_workspace_allowed,
    message as msg, normalize_allowed_workspaces, normalize_command_policy,
    normalize_configured_allowed_workspaces, normalize_reply_mode, string_list, workspace_key,
    workspace_label, workspace_ref,
};
use super::now_ms;
use super::service::{Bots, error, find_bot, guard, text};
use crate::agent::WorkspaceTarget;
use crate::automation::driver::{SendText, uuid_v7};
use crate::conversation_share::client::encode_uri_component;

const INBOUND_DEDUPE_TTL_MS: i64 = 2 * 60_000;
/// Bot 硬锁 yolo 运行模式。
const BOT_FORCED_MODE: &str = "yolo";

pub(crate) struct Auth {
    pub bot: Value,
    pub context: Value,
    pub locale: Option<String>,
}

/// `createOutbound`。
pub(crate) fn outbound(actor: &Actor, text: String, locale: Option<&str>) -> Value {
    let mut out = json!({
        "botId": actor.bot_id,
        "provider": actor.provider,
        "providerUserId": actor.reply_target(),
        "text": text,
    });
    if let Some(locale) = locale {
        out["locale"] = json!(locale);
    }
    out
}

/// `BotInboundMessage` JSON → [`Inbound`]。
pub fn inbound_from_value(value: &Value) -> Option<Inbound> {
    let actor = &value["actor"];
    let string = |value: &Value, key: &str| text(value, key).map(str::to_owned);
    Some(Inbound {
        bot_id: string(value, "botId")?,
        text: string(value, "text").unwrap_or_default(),
        has_attachment: value["attachments"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        actor: Actor {
            provider: string(actor, "provider")?,
            bot_id: string(actor, "botId")?,
            provider_user_id: string(actor, "providerUserId")?,
            display_name: string(actor, "displayName"),
            chat_type: string(actor, "chatType").unwrap_or_else(|| "private".into()),
            chat_id: string(actor, "chatId"),
            provider_message_id: string(actor, "providerMessageId"),
        },
    })
}

fn context_target(context: &Value) -> WorkspaceTarget {
    WorkspaceTarget {
        workspace_path: text(context, "workspacePath").unwrap_or_default().to_owned(),
        workspace_identity: text(context, "workspaceIdentity").map(str::to_owned),
        remote_session_id: None,
    }
}

fn context_key(context: &Value) -> String {
    workspace_key(
        text(context, "workspacePath").unwrap_or_default(),
        text(context, "workspaceIdentity"),
    )
}

/// `formatBotModelSelectionValue`。
fn selection_value(selection: &Value) -> Option<String> {
    let provider = text(selection, "providerId")?;
    let model = text(selection, "modelId")?;
    Some(if provider == ZCODE_AGENT_PROVIDER {
        model.to_owned()
    } else {
        format!(
            "custom:{}:{}",
            encode_uri_component(provider),
            encode_uri_component(model)
        )
    })
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(byte) = text
                .get(index + 1..index + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_owned())
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

impl Bots {
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
        if let Some(current) = params.get("currentWorkspace").filter(|value| value.is_object()) {
            push(current.clone());
        }
        let settings = self.settings.get().await;
        for entry in settings["lastWorkspaceSession"].as_array().into_iter().flatten() {
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
            && let Some(found) = workspaces.iter().find(|workspace| text(workspace, "id") == Some(id))
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

    /// `buildInitializedDraftOptions`：远端（Rust Host 尚无远端连接）只给最小默认值。
    fn initialized_draft(context: &Value) -> Value {
        if text(context, "workspaceIdentity").is_some() {
            json!({"provider": ZCODE_AGENT_PROVIDER})
        } else {
            json!({"provider": ZCODE_AGENT_PROVIDER, "mode": BOT_FORCED_MODE})
        }
    }

    /// `readContext`：已有状态时按最新 workspace 列表自愈身份；没有时取第一个允许的 workspace（不落盘）。
    async fn read_context(&self, bot: &Value) -> anyhow::Result<Option<Value>> {
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
                Some(identity) if !identity.is_null() => next["workspaceIdentity"] = identity.clone(),
                _ => {
                    next.as_object_mut().map(|object| object.shift_remove("workspaceIdentity"));
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
        let Some(workspace) = filter_allowed_workspaces(&workspaces, &allowed).into_iter().next() else {
            return Ok(None);
        };
        let mut context = json!({
            "botId": bot_id,
            "workspacePath": workspace["workspacePath"],
        });
        if let Some(identity) = text(&workspace, "workspaceIdentity") {
            context["workspaceIdentity"] = json!(identity);
        }
        context["workspaceId"] = workspace["id"].clone();
        context["mode"] = json!("draft");
        context["activeTaskId"] = Value::Null;
        context["draftOptions"] = Self::initialized_draft(&context);
        context["updatedAt"] = json!(now_ms());
        Ok(Some(context))
    }

    pub(crate) async fn write_context(&self, context: &Value) -> anyhow::Result<()> {
        let mut state = self.repo.read_state().await?;
        let mut next = context.clone();
        next["updatedAt"] = json!(now_ms());
        let id = text(context, "botId").unwrap_or_default().to_owned();
        if let Some(bots) = state["bots"].as_object_mut() {
            bots.insert(id, next);
        }
        self.repo.write_state(&state).await?;
        Ok(())
    }

    /// `writeDraftContext`：进入新任务草稿，清掉旧任务与待处理交互。
    async fn write_draft_context(&self, context: &Value, draft: Option<Value>) -> anyhow::Result<Value> {
        let mut next = context.as_object().cloned().unwrap_or_default();
        next.insert("mode".into(), json!("draft"));
        next.insert("activeTaskId".into(), Value::Null);
        for key in ["draftOptions", "pendingPermissionOptions", "pendingElicitation"] {
            next.shift_remove(key);
        }
        if let Some(draft) = draft {
            next.insert("draftOptions".into(), draft);
        }
        let next = Value::Object(next);
        self.write_context(&next).await?;
        Ok(next)
    }

    /// `normalizeBotWorkspaceConfig`：旧的 path-only 授权升级成身份 key（变化时写回配置）。
    async fn normalize_workspace_config(
        &self,
        config: &Value,
        bot: &Value,
        current: Value,
    ) -> anyhow::Result<Value> {
        let workspaces = self.workspace_refs(&json!({"currentWorkspace": current})).await;
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
        Ok(next_bot)
    }

    /// `withAuthorizedContext`。
    async fn authorize(&self, inbound: &Inbound, command: &str) -> anyhow::Result<Result<Auth, Vec<Value>>> {
        let locale = self.locale().await;
        let locale_ref = locale.as_deref();
        let reply = |id: &str, values: &[(&str, &str)]| {
            Err(vec![outbound(&inbound.actor, msg(locale_ref, id, values), None)])
        };
        let config = self.repo.read_config().await?;
        let actor = &inbound.actor;
        // `findAuthorizedBot`：启用、同通道、绑定的是这个用户。
        let bot = config["bots"].as_array().into_iter().flatten().find(|bot| {
            bot["enabled"] == true
                && text(bot, "provider") == Some(actor.provider.as_str())
                && text(bot, "providerUserId") == Some(actor.provider_user_id.as_str())
        });
        let Some(bot) = bot.filter(|bot| text(bot, "id") == Some(inbound.bot_id.as_str())).cloned() else {
            return Ok(reply("botDisabled", &[]));
        };
        if actor.chat_type != "private" {
            return Ok(reply("privateChatOnly", &[]));
        }
        let allowed = match command {
            "help" | "message" | "approve" | "task" | "stop" => true,
            "reconnect" => normalize_command_policy(&bot["allowedCommands"])["workspace"] != false,
            other => normalize_command_policy(&bot["allowedCommands"])[other] != false,
        };
        if !allowed {
            return Ok(reply("commandNotAllowed", &[]));
        }
        let Some(context) = self.read_context(&bot).await? else {
            return Ok(reply("noWorkspaceAllowed", &[]));
        };
        let path = text(&context, "workspacePath").unwrap_or_default();
        let mut current = Map::new();
        current.insert(
            "id".into(),
            json!(text(&context, "workspaceId").map(str::to_owned).unwrap_or_else(|| context_key(&context))),
        );
        current.insert("label".into(), json!(workspace_label(path)));
        current.insert("workspacePath".into(), json!(path));
        if let Some(identity) = text(&context, "workspaceIdentity") {
            current.insert("workspaceIdentity".into(), json!(identity));
        }
        let bot = self
            .normalize_workspace_config(&config, &bot, Value::Object(current))
            .await?;
        if let Some(id) = text(&context, "workspaceId")
            && !is_workspace_allowed(id, &string_list(&bot["allowedWorkspaces"]))
        {
            return Ok(reply("workspaceOutOfScope", &[]));
        }
        // Rust Host 还没有远端 workspace 连接：远端上下文视为未连接，只放行不需要运行时的命令。
        let needs_runtime = !matches!(command, "help" | "status" | "workspace" | "reconnect" | "reply");
        if text(&context, "workspaceIdentity").is_some() && needs_runtime {
            return Ok(reply("remoteDisconnected", &[("workspacePath", path)]));
        }
        Ok(Ok(Auth {
            bot,
            context,
            locale,
        }))
    }

    /// `isContextActiveTaskRunning`。
    async fn is_active_task_running(&self, context: &Value) -> bool {
        let Some(task_id) = text(context, "activeTaskId") else {
            return false;
        };
        if !guard(&self.running).contains(task_id) {
            return false;
        }
        if text(context, "workspaceIdentity").is_some() {
            return false;
        }
        let tasks = self.tasks.list(context_key(context)).await;
        let status = tasks
            .iter()
            .find(|task| text(task, "taskId") == Some(task_id))
            .and_then(|task| text(task, "status"));
        if matches!(status, Some("completed" | "error")) {
            guard(&self.running).remove(task_id);
            return false;
        }
        true
    }

    async fn model_label(&self, value: Option<String>) -> String {
        let Some(value) = value.filter(|value| !value.is_empty()) else {
            return "-".into();
        };
        let view = self.tasks.model_view(None).await;
        let label_of = |provider_id: &str| {
            view["providers"].as_array().into_iter().flatten().find_map(|provider| {
                (text(provider, "providerId") == Some(provider_id)
                    && provider["models"].as_array().is_some_and(|models| !models.is_empty()))
                .then(|| {
                    text(provider, "providerName")
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .unwrap_or(provider_id)
                        .to_owned()
                })
            })
        };
        if let Some(body) = value.strip_prefix("custom:") {
            let (provider, model) = match body.split_once(':') {
                Some((provider, model)) => (decode(provider), Some(decode(model))),
                None => (decode(body), None),
            };
            return match (label_of(&provider), model) {
                (Some(label), Some(model)) => format!("{label}/{model}"),
                (Some(label), None) => label,
                (None, _) => value,
            };
        }
        match value.split_once('/') {
            Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
                label_of(provider).map_or(value.clone(), |label| format!("{label}/{model}"))
            }
            _ => value,
        }
    }

    /// `buildStatusText`（不含已工作时长与实时进展）。
    async fn status_text(&self, context: &Value, locale: Option<&str>) -> String {
        let workspaces = self.workspace_refs(&Value::Null).await;
        let workspace_id = text(context, "workspaceId");
        let workspace = workspaces
            .iter()
            .find(|workspace| workspace_id.is_some() && text(workspace, "id") == workspace_id)
            .and_then(|workspace| text(workspace, "label").map(str::to_owned))
            .unwrap_or_else(|| text(context, "workspacePath").unwrap_or_default().to_owned());
        let task_id = text(context, "activeTaskId");
        if text(context, "workspaceIdentity").is_some() {
            let draft_model = context["draftOptions"]
                .get("modelSelection")
                .and_then(selection_value);
            let task = task_id
                .map(str::to_owned)
                .unwrap_or_else(|| msg(locale, "statusDraft", &[]));
            return [
                line(locale, "statusWorkspace", &workspace),
                line(locale, "statusModel", &self.model_label(draft_model).await),
                "------".to_owned(),
                line(locale, "statusTask", &task),
                line(locale, "statusState", &state_label(locale, "remote disconnected")),
                msg(
                    locale,
                    "remoteDisconnectedStatus",
                    &[("workspacePath", text(context, "workspacePath").unwrap_or_default())],
                ),
            ]
            .join("\n");
        }
        let task = match task_id {
            Some(id) => self
                .tasks
                .list(context_key(context))
                .await
                .into_iter()
                .find(|task| text(task, "taskId") == Some(id)),
            None => None,
        };
        let model_value = match &task {
            Some(task) => text(task, "model").map(str::to_owned),
            None => {
                let selection = context["draftOptions"].get("modelSelection").cloned();
                let view = self
                    .tasks
                    .model_view(selection.clone().map(|selection| json!({"selection": selection})))
                    .await;
                let effective = if selection.is_some() {
                    &view["effectiveSelection"]
                } else {
                    &view["preferredSelection"]
                };
                selection_value(effective)
            }
        };
        let task_line = match &task {
            Some(task) => format!(
                "{}: {} ({})",
                msg(locale, "statusTask", &[]),
                text(task, "title").unwrap_or_default(),
                text(task, "taskId").unwrap_or_default()
            ),
            None => line(locale, "statusTask", &msg(locale, "statusDraft", &[])),
        };
        let state = match &task {
            Some(task) => text(task, "status").unwrap_or("running").to_owned(),
            None => "draft".to_owned(),
        };
        [
            line(locale, "statusWorkspace", &workspace),
            line(locale, "statusModel", &self.model_label(model_value).await),
            "------".to_owned(),
            task_line,
            line(locale, "statusState", &state_label(locale, &state)),
        ]
        .join("\n")
    }

    /// `buildHelpText`。
    fn help_text(locale: Option<&str>, bot: &Value) -> String {
        let commands = &bot["allowedCommands"];
        let mut lines = vec![msg(locale, "helpTitle", &[])];
        for (command, id) in [
            ("help", "helpHelp"),
            ("status", "helpStatus"),
            ("new", "helpNew"),
            ("workspace", "helpWorkspace"),
            ("model", "helpModel"),
            ("mode", "helpMode"),
            ("thoughtLevel", "helpThoughtLevel"),
            ("reply", "helpReply"),
            ("bind", "helpBind"),
        ] {
            if command != "help" && command != "bind" && commands[command] == false {
                continue;
            }
            lines.push(msg(locale, id, &[]));
        }
        lines.join("\n")
    }

    async fn handle_bind(&self, inbound: &Inbound, code: &str) -> anyhow::Result<Vec<Value>> {
        let locale = self.locale().await;
        let locale = locale.as_deref();
        let actor = &inbound.actor;
        if actor.chat_type != "private" {
            return Ok(vec![outbound(actor, msg(locale, "bindPrivateOnly", &[]), None)]);
        }
        let key = code.trim().to_uppercase();
        let record = guard(&self.bind_codes)
            .get(&key)
            .map(|record| (record.bot_id.clone(), record.allowed_workspaces.clone(), record.expires_at));
        let Some((bot_id, allowed, _)) = record.filter(|(bot_id, _, expires_at)| {
            *expires_at > now_ms() && *bot_id == inbound.bot_id
        }) else {
            return Ok(vec![outbound(actor, msg(locale, "bindCodeInvalid", &[]), None)]);
        };
        let config = self.repo.read_config().await?;
        let Some(bot) = find_bot(&config, &bot_id) else {
            return Ok(vec![outbound(actor, msg(locale, "bindBotMissing", &[]), None)]);
        };
        // 重新绑定覆盖旧 providerUserId：一个 Bot 同一时间只有一个沟通对象。
        let mut next = bot.clone();
        next["providerUserId"] = json!(actor.provider_user_id);
        match &actor.display_name {
            Some(name) => next["displayName"] = json!(name),
            None => {
                next.as_object_mut().map(|object| object.shift_remove("displayName"));
            }
        }
        next["allowedWorkspaces"] = json!(normalize_allowed_workspaces(&allowed));
        next["allowedCommands"] = normalize_command_policy(&bot["allowedCommands"]);
        next["replyMode"] = json!(normalize_reply_mode(
            text(&bot, "provider").unwrap_or_default(),
            text(&bot, "replyMode")
        ));
        super::service::validate(&config, &next).map_err(|error| anyhow::anyhow!(error.message))?;
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
        let text = [msg(locale, "bindSuccess", &[]), Self::help_text(locale, &next)].join("\n\n");
        Ok(vec![outbound(actor, text, None)])
    }

    /// 入站命令分发（`handleInboundMessage`）；同一聊天上下文串行处理。
    pub async fn handle_inbound(self: &Arc<Self>, inbound: &Inbound) -> Result<Vec<Value>, RpcError> {
        let actor = &inbound.actor;
        let queue_key = format!(
            "{}::{}::{}",
            actor.bot_id,
            actor.provider,
            actor.chat_id.as_deref().map(str::trim).filter(|id| !id.is_empty()).unwrap_or(&actor.provider_user_id)
        );
        let queue = guard(&self.inbound_queues).entry(queue_key).or_default().clone();
        let _turn = queue.lock().await;
        self.dispatch(inbound).await.map_err(|error| super::service::error(format!("{error:#}")))
    }

    async fn dispatch(self: &Arc<Self>, inbound: &Inbound) -> anyhow::Result<Vec<Value>> {
        let actor = &inbound.actor;
        let mut command = model::parse_command(&inbound.text);
        // 非微信通道里纯文本 "0" 不是取消，而是普通消息。
        if command["type"] == "selection.cancel" && actor.provider != "weixin" && inbound.text.trim() == "0" {
            command = json!({"type": "message", "text": inbound.text});
        }
        let kind = text(&command, "type").unwrap_or_default().to_owned();
        let unknown = |name: &str, locale: Option<&str>| {
            vec![outbound(actor, msg(locale, "unknownCommand", &[("command", name)]), None)]
        };
        match kind.as_str() {
            "bind" => self.handle_bind(inbound, text(&command, "code").unwrap_or_default()).await,
            "help" => Ok(match self.authorize(inbound, "help").await? {
                Ok(auth) => vec![outbound(actor, Self::help_text(auth.locale.as_deref(), &auth.bot), None)],
                Err(reply) => reply,
            }),
            "status" => Ok(match self.authorize(inbound, "status").await? {
                Ok(auth) => {
                    let locale = auth.locale.as_deref();
                    vec![outbound(actor, self.status_text(&auth.context, locale).await, locale)]
                }
                Err(reply) => reply,
            }),
            "new" => {
                let auth = match self.authorize(inbound, "new").await? {
                    Ok(auth) => auth,
                    Err(reply) => return Ok(reply),
                };
                let locale = auth.locale.as_deref();
                if self.is_active_task_running(&auth.context).await {
                    return Ok(vec![outbound(actor, msg(locale, "taskRunning", &[]), None)]);
                }
                let draft = Self::initialized_draft(&auth.context);
                let context = self.write_draft_context(&auth.context, Some(draft)).await?;
                Ok(vec![outbound(actor, self.status_text(&context, locale).await, locale)])
            }
            "stop" => {
                let auth = match self.authorize(inbound, "stop").await? {
                    Ok(auth) => auth,
                    Err(reply) => return Ok(reply),
                };
                let locale = auth.locale.as_deref();
                let Some(task_id) = text(&auth.context, "activeTaskId").map(str::to_owned) else {
                    return Ok(vec![outbound(actor, msg(locale, "noActiveTask", &[]), None)]);
                };
                if let Err(message) = self.tasks.stop(context_target(&auth.context), task_id.clone()).await {
                    return Ok(vec![outbound(actor, msg(locale, "taskFailed", &[("message", &message)]), None)]);
                }
                guard(&self.running).remove(&task_id);
                Ok(vec![outbound(actor, self.status_text(&auth.context, locale).await, locale)])
            }
            // 没有待处理的选择（Rust Host 不发选择卡）：同 Node 回未知命令 0。
            "selection.cancel" => Ok(unknown("0", self.locale().await.as_deref())),
            "unknown" => Ok(unknown(text(&command, "name").unwrap_or_default(), self.locale().await.as_deref())),
            "message" => self.handle_message(inbound).await,
            _ => {
                let name = inbound
                    .text
                    .trim()
                    .trim_start_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_lowercase();
                log::info(format!("bots command not available in the Rust Host command={kind}"));
                Ok(unknown(&name, self.locale().await.as_deref()))
            }
        }
    }

    /// 普通消息：草稿态新建会话，任务态续用当前会话；输入在后台发送，终态回推。
    async fn handle_message(self: &Arc<Self>, inbound: &Inbound) -> anyhow::Result<Vec<Value>> {
        let actor = &inbound.actor;
        let mut auth = match self.authorize(inbound, "message").await? {
            Ok(auth) => auth,
            Err(reply) => return Ok(reply),
        };
        let locale = auth.locale.clone();
        let locale = locale.as_deref();
        let mut deleted_task: Option<String> = None;
        if auth.context["mode"] == "task"
            && let Some(task_id) = text(&auth.context, "activeTaskId").map(str::to_owned)
            && self.tasks.deleted_ids(context_key(&auth.context)).await.contains(&task_id)
        {
            // 软删除的任务不能凭 activeTaskId 续跑：回到草稿，用当前草稿的选择新建。
            deleted_task = Some(task_id);
            auth.context = self.write_draft_context(&auth.context, None).await?;
        }
        if auth.context["mode"] == "task"
            && text(&auth.context, "activeTaskId").is_some()
            && self.is_active_task_running(&auth.context).await
        {
            return Ok(vec![outbound(actor, msg(locale, "taskRunning", &[]), None)]);
        }
        if inbound.has_attachment {
            // Rust Host 还不下载 Bot 附件。
            let reason = msg(locale, "attachmentDownloadUnavailable", &[]);
            return Ok(vec![outbound(
                actor,
                msg(locale, "attachmentRejected", &[("message", &reason)]),
                None,
            )]);
        }
        let target = context_target(&auth.context);
        let bot_target = automation_target(actor);
        let draft_task = auth.context["mode"] == "draft" || text(&auth.context, "activeTaskId").is_none();
        let (task_id, selection, context) = if draft_task {
            let draft = match auth.context.get("draftOptions").filter(|value| value.is_object()) {
                Some(draft) => draft.clone(),
                None => Self::initialized_draft(&auth.context),
            };
            let original = draft.get("modelSelection").cloned();
            let view = self
                .tasks
                .model_view(original.clone().map(|selection| json!({"selection": selection})))
                .await;
            let submission = if original.is_some() {
                view["effectiveSelection"].clone()
            } else {
                view["preferredSelection"].clone()
            };
            let issue = view.get("selectionIssue").is_some_and(|issue| !issue.is_null());
            if !submission.is_object() || (original.is_some() && issue) {
                anyhow::bail!("Bot 无法从目标 Host 解析 Submission 模型");
            }
            let mode = text(&draft, "mode").map(str::to_owned);
            let task_id = self
                .tasks
                .create(target.clone(), submission.clone(), mode)
                .await
                .map_err(|message| anyhow::anyhow!(message))?;
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
                if let Err(error) = self
                    .send_outbound(&auth.bot, &outbound(actor, msg(locale, "deletedTaskReplaced", &[]), None))
                    .await
                {
                    log::warn(format!("deleted task replacement notice failed task={task_id}: {error:#}"));
                }
            }
            (task_id, submission, context)
        } else {
            let task_id = text(&auth.context, "activeTaskId").unwrap_or_default().to_owned();
            let original = self
                .tasks
                .resume(target.clone(), task_id.clone())
                .await
                .map_err(|message| anyhow::anyhow!(message))?;
            let view = match &original {
                Some(selection) => self.tasks.model_view(Some(json!({"selection": selection}))).await,
                None => Value::Null,
            };
            let effective = view["effectiveSelection"].clone();
            let issue = view.get("selectionIssue").is_some_and(|issue| !issue.is_null());
            if !effective.is_object() || issue {
                anyhow::bail!("{}", msg(locale, "sessionModelUnavailable", &[]));
            }
            (task_id, effective, auth.context.clone())
        };
        guard(&self.running).insert(task_id.clone());
        let input_id = uuid_v7();
        // 先订阅终态再发送，避免快速完成的轮次漏掉回推。
        self.watch_task(auth.bot.clone(), actor.clone(), context_target(&context), task_id.clone(), Some(input_id.clone()));
        let bots = self.clone();
        let bot = auth.bot.clone();
        let actor = actor.clone();
        let text_input = inbound.text.clone();
        tokio::spawn(async move {
            let input = SendText {
                input_id,
                text: text_input,
                model_selection: Some(selection),
                bot_delivery_target: bot_target,
                ..SendText::default()
            };
            if let Err(message) = bots.tasks.send(target, task_id.clone(), input).await {
                guard(&bots.running).remove(&task_id);
                let locale = bots.locale().await;
                let locale = locale.as_deref();
                let expired = message.contains("Session not found:") || message.contains("Session is not active:");
                let text = if expired {
                    msg(locale, "sessionExpiredNewTaskHint", &[])
                } else {
                    msg(locale, "taskFailed", &[("message", &message)])
                };
                if let Err(error) = bots.send_outbound(&bot, &outbound(&actor, text, None)).await {
                    log::warn(format!("bots prompt failure notice failed task={task_id}: {error:#}"));
                }
            }
        });
        Ok(Vec::new())
    }

    /// `processProviderCallback`（飞书 / Lark）：解析、去重、处理并投递回复。`synchronous_card` 为卡片
    /// 按钮回调：回复由长连接同步回包承载，不另发消息。
    pub async fn process_callback(self: &Arc<Self>, provider: &str, payload: Value, synchronous_card: bool) -> Value {
        let locale = self.locale().await;
        let Some(inbound) = parse_callback(&payload) else {
            log::debug(format!("provider callback parsed provider={provider} count=0"));
            return json!({"ok": true, "replies": []});
        };
        let config = match self.repo.read_config().await {
            Ok(config) => config,
            Err(error) => {
                log::warn(format!("provider callback failed provider={provider}: {error:#}"));
                return json!({"ok": false, "replies": [], "status": 503});
            }
        };
        let bot = find_bot(&config, &inbound.bot_id);
        if let Some(bot) = &bot
            && let Some(reference) = text(bot, "webhookSecretRef")
            && let Some(expected) = self.load_credential(Some(reference)).await
        {
            let token = text(&payload, "token").or_else(|| text(&payload["header"], "token"));
            if token != Some(expected.as_str()) {
                return json!({"ok": true, "replies": [outbound(&inbound.actor, msg(locale.as_deref(), "webhookSecretInvalid", &[]), None)]});
            }
        }
        if !self.mark_inbound(&inbound) {
            log::info(format!(
                "provider callback duplicated provider={provider} bot={} messageId={}",
                inbound.bot_id,
                inbound.actor.provider_message_id.as_deref().unwrap_or_default()
            ));
            return json!({"ok": true, "replies": []});
        }
        log::info(format!(
            "provider callback provider={provider} bot={} user={}",
            inbound.bot_id, inbound.actor.provider_user_id
        ));
        let replies = match self.handle_inbound(&inbound).await {
            Ok(replies) => replies,
            Err(failure) => {
                self.release_inbound(&inbound);
                log::warn(format!(
                    "provider callback failed provider={provider} bot={} user={}: {}",
                    inbound.bot_id, inbound.actor.provider_user_id, failure.message
                ));
                let expired = failure.message.contains("Session not found:")
                    || failure.message.contains("Session is not active:");
                let text = if expired {
                    msg(locale.as_deref(), "sessionExpiredNewTaskHint", &[])
                } else {
                    msg(locale.as_deref(), "callbackFailed", &[("message", &failure.message)])
                };
                let reply = outbound(&inbound.actor, text, None);
                if let Some(bot) = &bot
                    && let Err(error) = self.send_outbound(bot, &reply).await
                {
                    log::warn(format!("provider callback failure notice failed provider={provider}: {error:#}"));
                }
                return json!({"ok": false, "replies": [reply], "status": 503});
            }
        };
        if let Some(bot) = &bot
            && !(synchronous_card && !replies.is_empty())
        {
            for reply in &replies {
                if let Err(error) = self.send_outbound(bot, reply).await {
                    self.release_inbound(&inbound);
                    log::warn(format!("provider callback reply failed provider={provider}: {error:#}"));
                    return json!({"ok": false, "replies": replies, "status": 503});
                }
            }
        }
        json!({"ok": true, "replies": replies})
    }

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

    /// `handleProviderCallback(Response)`：只有飞书 / Lark 通道。
    pub async fn provider_callback(self: &Arc<Self>, provider: &str, payload: Value) -> Result<Value, RpcError> {
        if !is_feishu(provider) {
            return Err(super::service::not_implemented(provider));
        }
        let mut payload = payload;
        if payload.is_object() {
            let mut object = Map::new();
            object.insert("zcodeProvider".into(), json!(provider));
            object.extend(payload.as_object().cloned().unwrap_or_default());
            payload = Value::Object(object);
        }
        if text(&payload, "botId").is_none() {
            // `findCallbackBot`：没带 botId 时取该通道第一个启用的 Bot。
            let config = self.repo.read_config().await.map_err(|error| error_text(&error))?;
            if let Some(id) = config["bots"].as_array().into_iter().flatten().find_map(|bot| {
                (text(bot, "provider") == Some(provider) && bot["enabled"] == true)
                    .then(|| text(bot, "id").map(str::to_owned))
                    .flatten()
            }) {
                payload["botId"] = json!(id);
            }
        }
        Ok(self.process_callback(provider, payload, false).await)
    }
}

fn error_text(failure: &anyhow::Error) -> RpcError {
    error(failure.to_string())
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
