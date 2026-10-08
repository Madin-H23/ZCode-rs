//! 选择命令（botsService.ts `handleInboundMessage` 的 workspace / model / mode / thoughtLevel / task /
//! reply 分支、`createSelectionReply`、待选状态）。
//!
//! 待选状态（`pendingSelectionsByContext` 等）只在内存，按聊天上下文（`botId::provider::chatId||userId`）
//! 存放；Host 退出即丢（同 Node）。
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::{Map, Value, json};

use super::feishu::{Actor, Inbound};
use super::inbound::Auth;
use super::model::{
    ZCODE_AGENT_PROVIDER, decode_custom_model, filter_allowed_workspaces, message as msg,
    normalize_reply_mode, string_list, workspace_key, workspace_label, workspace_ref,
};
use super::now_ms;
use super::providers::{OutboundExtras, outbound};
use super::service::{Bots, TaskEntry, guard};
use super::status::task_status;
use super::util::{encode_uri_component, text};
use super::watch::context_target;

/// `BOT_REPLY_GRANULARITY_OPTIONS`：(id, zh-CN, en-US, 别名)。
const REPLY_OPTIONS: [(&str, &str, &str, &[&str]); 4] = [
    (
        "assistant_changes",
        "标准回复",
        "Standard reply",
        &["assistant", "assistant_changes", "normal", "default", "standard", "标准回复"],
    ),
    (
        "assistant_toolcalls_changes",
        "完整回复",
        "Full reply",
        &["full", "tool", "toolcalls", "assistant_toolcalls_changes", "完整回复"],
    ),
    (
        "summary_changes",
        "摘要回复",
        "Summary reply",
        &["summary", "summary_changes", "latest", "摘要回复"],
    ),
    (
        "streaming_card",
        "流式卡片",
        "Streaming card",
        &["stream", "streaming", "streaming_card", "流式", "流式卡片"],
    ),
];

fn normalize_text(value: &str) -> String {
    value.trim().to_lowercase()
}

/// `isSelectionIndexValue`：`/^[1-9]\d*$/`。
pub fn is_index_value(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed
        .chars()
        .next()
        .is_some_and(|first| ('1'..='9').contains(&first))
        && trimmed.chars().all(|c| c.is_ascii_digit())
}

/// `resolveOptionByValue`：纯数字按序号，否则按 id 或 label（忽略大小写）。
pub fn resolve_option(items: &[Value], value: &str) -> Option<Value> {
    let trimmed = value.trim();
    if is_index_value(trimmed) {
        let index: usize = trimmed.parse().ok()?;
        return items.get(index - 1).cloned();
    }
    let normalized = normalize_text(trimmed);
    items
        .iter()
        .find(|item| {
            normalize_text(text(item, "id").unwrap_or_default()) == normalized
                || normalize_text(text(item, "label").unwrap_or_default()) == normalized
        })
        .cloned()
}

/// JS `Number.parseInt(value, 10)`：前导空白后的可选符号与数字。
pub fn parse_int(value: &str) -> Option<i64> {
    let trimmed = value.trim_start();
    let (sign, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
    digits[..end].parse::<i64>().ok().map(|number| sign * number)
}

/// `Number.parseInt(value, 10) - 1` 作为下标（权限按钮序号）。
pub fn parse_index(value: &str) -> Option<usize> {
    let index = parse_int(value)? - 1;
    usize::try_from(index).ok()
}

/// `getReplyGranularityOptions`。
pub fn reply_options(locale: Option<&str>, provider: &str) -> Vec<Value> {
    let english = locale == Some("en-US");
    REPLY_OPTIONS
        .iter()
        .filter(|(id, ..)| normalize_reply_mode(provider, Some(id)) == *id)
        .map(|(id, zh, en, _)| json!({"id": id, "label": if english { en } else { zh }}))
        .collect()
}

/// `resolveReplyGranularityByValue`。
fn resolve_reply(value: &str, locale: Option<&str>, provider: &str) -> Option<Value> {
    let options = reply_options(locale, provider);
    if let Some(index) = parse_int(value.trim()).filter(|index| *index > 0) {
        return options.get(index as usize - 1).cloned();
    }
    let normalized = normalize_text(value);
    let id = REPLY_OPTIONS
        .iter()
        .find(|(_, zh, en, aliases)| {
            aliases.contains(&normalized.as_str())
                || normalize_text(zh) == normalized
                || normalize_text(en) == normalized
        })
        .map(|(id, ..)| *id)?;
    options.into_iter().find(|option| text(option, "id") == Some(id))
}

fn reply_label(mode: Option<&str>, locale: Option<&str>, provider: &str) -> String {
    let current = normalize_reply_mode(provider, mode);
    reply_options(locale, provider)
        .iter()
        .find(|option| text(option, "id") == Some(current))
        .and_then(|option| text(option, "label").map(str::to_owned))
        .unwrap_or_else(|| current.to_owned())
}

/// `formatBotModelSelectionValue`：glm 模型直接用 modelId，其他 provider 编码成 `custom:p:m`。
pub fn selection_value(selection: &Value) -> Option<String> {
    let provider = text(selection, "providerId")?;
    let model = text(selection, "modelId")?;
    Some(if provider == ZCODE_AGENT_PROVIDER {
        model.to_owned()
    } else {
        encode_custom(provider, Some(model))
    })
}

/// `encodeCustomModelValue`。
pub fn encode_custom(provider: &str, model: Option<&str>) -> String {
    match model.filter(|model| !model.is_empty()) {
        Some(model) => format!(
            "custom:{}:{}",
            encode_uri_component(provider),
            encode_uri_component(model)
        ),
        None => format!("custom:{}", encode_uri_component(provider)),
    }
}

/// `parseBotModelOptionValue`。
pub fn parse_model_option(value: &str) -> Option<Value> {
    if let Some((provider, Some(model))) = decode_custom_model(value)
        && !provider.is_empty()
        && !model.is_empty()
    {
        return Some(json!({"providerId": provider, "modelId": model}));
    }
    if let Some(index) = value.find('/').filter(|index| *index > 0 && *index < value.len() - 1) {
        return Some(json!({"providerId": &value[..index], "modelId": &value[index + 1..]}));
    }
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| json!({"providerId": ZCODE_AGENT_PROVIDER, "modelId": trimmed}))
}

/// `completeNewModelSelection`：取模型档位列表的最后一个（最高档）。
pub fn complete_new_selection(view: &Value, identity: &Value) -> Option<Value> {
    let model = find_model(view, text(identity, "providerId")?, text(identity, "modelId")?)?;
    let level = model["config"]["optionSpecs"]["reasoningLevel"]["values"]
        .as_array()?
        .last()?
        .clone();
    Some(json!({
        "providerId": identity["providerId"], "modelId": identity["modelId"],
        "options": {"reasoningLevel": level},
    }))
}

fn find_model<'a>(view: &'a Value, provider: &str, model: &str) -> Option<&'a Value> {
    view["providers"]
        .as_array()?
        .iter()
        .find(|candidate| text(candidate, "providerId") == Some(provider))?["models"]
        .as_array()?
        .iter()
        .find(|candidate| text(candidate, "modelId") == Some(model))
}

/// `createModelSelectionProviderOption` + 过滤掉没有模型的 provider。
pub fn provider_options(view: &Value) -> Vec<Value> {
    view["providers"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|provider| {
            let id = text(provider, "providerId").unwrap_or_default();
            let label = text(provider, "providerName")
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(id);
            let models: Vec<Value> = provider["models"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|model| {
                    let model_id = text(model, "modelId").unwrap_or_default();
                    json!({"id": encode_custom(id, Some(model_id)), "label": model_id})
                })
                .collect();
            json!({"id": id, "label": label, "models": models})
        })
        .filter(|provider| provider["models"].as_array().is_some_and(|m| !m.is_empty()))
        .collect()
}

/// `findSelectConfigOption`。
pub fn select_option<'a>(options: &'a Value, config_id: &str) -> Option<&'a Value> {
    let category = if config_id == "thoughtLevel" {
        "thought_level"
    } else {
        config_id
    };
    options.as_array()?.iter().find(|item| {
        item["type"] == "select"
            && (item["category"] == category || item["id"] == category)
    })
}

/// `readConfigSelectCurrentValue`。
pub fn select_current(options: &Value, config_id: &str) -> Option<String> {
    select_option(options, config_id)?["currentValue"]
        .as_str()
        .map(str::to_owned)
}

/// `getModeDisplayLabel`。
fn mode_label(locale: Option<&str>, id: &str, label: &str) -> String {
    let english = locale == Some("en-US");
    match id {
        "default" => if english { "Default" } else { "默认" }.to_owned(),
        "yolo" => "Yolo".to_owned(),
        "plan" => if english { "Plan" } else { "计划" }.to_owned(),
        _ => label.to_owned(),
    }
}

/// `listConfigSelectOptions`。
pub fn select_options(options: &Value, config_id: &str, locale: Option<&str>) -> Vec<Value> {
    select_option(options, config_id)
        .and_then(|option| option["options"].as_array())
        .into_iter()
        .flatten()
        .map(|item| {
            let id = text(item, "value").unwrap_or_default();
            let name = text(item, "name").unwrap_or_default();
            let label = if config_id == "mode" {
                mode_label(locale, id, name)
            } else {
                name.to_owned()
            };
            let mut out = Map::new();
            out.insert("id".into(), json!(id));
            out.insert("label".into(), json!(label));
            if let Some(description) = text(item, "description") {
                out.insert("description".into(), json!(description));
            }
            Value::Object(out)
        })
        .collect()
}

fn select_label_for(options: &Value, config_id: &str, value: Option<&str>, locale: Option<&str>) -> Option<String> {
    let value = value.filter(|value| !value.is_empty())?;
    Some(
        select_options(options, config_id, locale)
            .iter()
            .find(|option| text(option, "id") == Some(value))
            .and_then(|option| text(option, "label").map(str::to_owned))
            .unwrap_or_else(|| value.to_owned()),
    )
}

/// `formatWorkspaceOptionLabel`。
fn workspace_option_label(workspace: &Value, locale: Option<&str>) -> String {
    let label = text(workspace, "label").unwrap_or_default();
    if text(workspace, "workspaceIdentity").is_none() {
        return label.to_owned();
    }
    let remote = if locale == Some("en-US") { "[Remote]" } else { "[远端]" };
    format!("{label} {remote}")
}

/// `formatSelectionFallback`（微信纯文本菜单）。
fn selection_fallback(selection: &Value, locale: Option<&str>) -> String {
    let lines: Vec<String> = selection["options"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, option)| {
            let description = text(option, "description")
                .filter(|d| !d.is_empty())
                .map(|d| format!(" {d}"))
                .unwrap_or_default();
            format!("{}. {}{description}", index + 1, text(option, "label").unwrap_or_default())
        })
        .collect();
    let title = text(selection, "title").unwrap_or_default();
    if selection["showCancel"] == false {
        return format!(
            "{title}\n{}\n\n{}",
            lines.join("\n"),
            msg(locale, "selectionTextHintNoCancel", &[])
        );
    }
    let cancel = text(selection, "cancelLabel")
        .map(str::to_owned)
        .unwrap_or_else(|| msg(locale, "selectionCancelOption", &[]));
    format!(
        "{title}\n0. {cancel}\n{}\n\n{}",
        lines.join("\n"),
        msg(locale, "selectionTextHint", &[])
    )
}

/// `markCurrentSelection`：当前项加 ` · 当前`，并带上取消文案。
fn mark_current(selection: &Value, locale: Option<&str>) -> Value {
    let mut marked = selection.clone();
    marked["cancelLabel"] = json!(msg(locale, "selectionCancelOption", &[]));
    let Some(current) = text(selection, "currentId").map(str::to_owned) else {
        return marked;
    };
    let suffix = if locale == Some("en-US") { "current" } else { "当前" };
    if let Some(options) = marked["options"].as_array_mut() {
        for option in options.iter_mut() {
            if text(option, "id") == Some(current.as_str()) {
                let label = format!("{} · {suffix}", text(option, "label").unwrap_or_default());
                option["label"] = json!(label);
            }
        }
    }
    marked
}

/// JSON 里去掉值为 null / 缺省的 currentId 等可选键。
fn prune(mut value: Value) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.retain(|_, field| !field.is_null());
    }
    value
}

impl Bots {
    /// `getActorContextKey`。
    pub(crate) fn actor_key(&self, actor: &Actor) -> String {
        let target = actor
            .chat_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .unwrap_or(&actor.provider_user_id);
        format!("{}::{}::{target}", actor.bot_id, actor.provider)
    }

    /// `clearPendingSelectionsForBot`。
    pub(crate) fn clear_pending_for_bot(&self, bot_id: &str) {
        let prefix = format!("{bot_id}::");
        let matches = |key: &String| key == bot_id || key.starts_with(&prefix);
        guard(&self.pending_selections).retain(|key, _| !matches(key));
        guard(&self.pending_tasks).retain(|key, _| !matches(key));
        guard(&self.pending_workspaces).retain(|key, _| !matches(key));
    }

    /// `clearPendingSelection`。
    pub(crate) fn clear_pending(&self, actor: &Actor) {
        let key = self.actor_key(actor);
        guard(&self.pending_selections).remove(&key);
        guard(&self.pending_tasks).remove(&key);
        guard(&self.pending_workspaces).remove(&key);
    }

    pub(crate) fn pending_selection(&self, actor: &Actor) -> Option<Value> {
        guard(&self.pending_selections).get(&self.actor_key(actor)).cloned()
    }

    /// `resolvePendingSelectionOption`：动作一致时按值解析，命中后清掉待选项。
    pub(crate) fn resolve_pending_option(&self, actor: &Actor, action: &str, value: &str) -> Option<Value> {
        let key = self.actor_key(actor);
        let selection = guard(&self.pending_selections).get(&key).cloned()?;
        if text(&selection, "action") != Some(action) {
            return None;
        }
        let options = selection["options"].as_array().cloned().unwrap_or_default();
        let option = resolve_option(&options, value)?;
        guard(&self.pending_selections).remove(&key);
        Some(option)
    }

    /// `resolvePendingSelectionCommand`：只有微信把纯数字回复当作待选项；其他通道普通文本清空待选。
    pub(crate) fn resolve_pending_command(&self, actor: &Actor, value: &str) -> Option<Value> {
        let key = self.actor_key(actor);
        let selection = guard(&self.pending_selections).get(&key).cloned()?;
        if actor.provider != "weixin" || !is_index_value(value) {
            self.clear_pending(actor);
            return None;
        }
        let options = selection["options"].as_array().cloned().unwrap_or_default();
        let Some(option) = resolve_option(&options, value) else {
            self.clear_pending(actor);
            return None;
        };
        guard(&self.pending_selections).remove(&key);
        let id = option["id"].clone();
        let action = text(&selection, "action").unwrap_or_default();
        Some(match action {
            "permission.respond" => json!({"type": action, "value": value}),
            _ => json!({"type": action, "value": id}),
        })
    }

    /// `createSelectionReply`：记下待选项；非微信通道发结构化选择，微信发纯文本编号菜单。
    pub(crate) fn selection_reply(
        &self,
        actor: &Actor,
        selection: Value,
        locale: Option<&str>,
        elicitation: Option<Value>,
    ) -> Vec<Value> {
        let selection = prune(selection);
        let marked = mark_current(&selection, locale);
        let structured = actor.provider != "weixin";
        let display = if structured {
            marked
        } else {
            // 微信第一层只列供应商，模型放下一层；标题负责说明当前状态，列表不重复“当前”标记。
            let mut plain = selection.clone();
            if text(&plain, "action") == Some("model.provider.set")
                && let Some(options) = plain["options"].as_array_mut()
            {
                for option in options.iter_mut() {
                    if let Some(object) = option.as_object_mut() {
                        object.remove("description");
                    }
                }
            }
            plain["cancelLabel"] = marked["cancelLabel"].clone();
            plain
        };
        guard(&self.pending_selections).insert(self.actor_key(actor), display.clone());
        let body = if structured {
            text(&display, "title").unwrap_or_default().to_owned()
        } else {
            selection_fallback(&display, locale)
        };
        vec![outbound(
            actor,
            body,
            OutboundExtras {
                selection: structured.then_some(display),
                elicitation,
                locale: locale.map(str::to_owned),
            },
        )]
    }

    /// `createStatusReply`。
    pub(crate) async fn status_reply(&self, actor: &Actor, context: &Value, locale: Option<&str>) -> Result<Vec<Value>> {
        let body = self.status_text(context, locale).await?;
        Ok(vec![outbound(
            actor,
            body,
            OutboundExtras {
                locale: locale.map(str::to_owned),
                ..OutboundExtras::default()
            },
        )])
    }

    /// `readModelSelectionView`（远端未连接时为空视图）。
    pub(crate) async fn model_view_for(&self, context: &Value, selection: Option<&Value>) -> Option<Value> {
        let tasks = self.tasks_for(&context_target(context)).ok()?;
        Some(
            tasks
                .model_view(selection.map(|selection| json!({"selection": selection})))
                .await,
        )
    }

    /// `listModelSelectionProviderOptions`。
    pub(crate) async fn provider_options_for(&self, context: &Value) -> Vec<Value> {
        self.model_view_for(context, None)
            .await
            .map(|view| provider_options(&view))
            .unwrap_or_default()
    }

    /// `formatStatusModelLabel`：provider 显示名 + 模型名。
    pub(crate) async fn model_label(&self, model: Option<&str>, context: &Value) -> String {
        let Some(model) = model.filter(|model| !model.is_empty()) else {
            return "-".into();
        };
        let label_of = |options: &[Value], id: &str| {
            options
                .iter()
                .find(|option| text(option, "id") == Some(id))
                .and_then(|option| text(option, "label").map(str::to_owned))
        };
        if let Some((provider, name)) = decode_custom_model(model)
            && !provider.is_empty()
        {
            let options = self.provider_options_for(context).await;
            let label = label_of(&options, &provider);
            return match (label, name.filter(|name| !name.is_empty())) {
                (Some(label), Some(name)) => format!("{label}/{name}"),
                (Some(label), None) => label,
                (None, _) => model.to_owned(),
            };
        }
        let Some(index) = model.find('/').filter(|index| *index > 0 && *index < model.len() - 1) else {
            return model.to_owned();
        };
        let options = self.provider_options_for(context).await;
        match label_of(&options, &model[..index]) {
            Some(label) => format!("{label}/{}", &model[index + 1..]),
            None => model.to_owned(),
        }
    }

    /// `readCurrentModelProviderId`。
    async fn current_provider_id(&self, model: Option<&str>, context: &Value) -> Option<String> {
        let model = model.filter(|model| !model.is_empty())?;
        if let Some((provider, _)) = decode_custom_model(model)
            && !provider.is_empty()
        {
            return Some(provider);
        }
        let options = self.provider_options_for(context).await;
        Some(
            options
                .iter()
                .find(|provider| {
                    provider["models"]
                        .as_array()
                        .is_some_and(|models| models.iter().any(|m| text(m, "id") == Some(model)))
                })
                .and_then(|provider| text(provider, "id").map(str::to_owned))
                .unwrap_or_else(|| format!("native:{ZCODE_AGENT_PROVIDER}")),
        )
    }

    /// `resolveDraftOptionsForDisplay`：只返回副本，查看菜单不写回原草稿。
    async fn draft_for_display(&self, context: &Value) -> Result<Value> {
        let original = self.ensure_draft_options(context).await?;
        let view = self
            .model_view_for(context, original.get("modelSelection").filter(|s| !s.is_null()))
            .await
            .unwrap_or(Value::Null);
        let key = if original["modelSelection"].is_object() {
            "effectiveSelection"
        } else {
            "preferredSelection"
        };
        let mut draft = original.clone();
        match view.get(key).filter(|selection| selection.is_object()) {
            Some(selection) => draft["modelSelection"] = selection.clone(),
            None => {
                if let Some(object) = draft.as_object_mut() {
                    object.remove("modelSelection");
                }
            }
        }
        Ok(draft)
    }

    /// `listDraftConfigOptions`：草稿只有思考档位一个可选项（来自模型规格）。
    fn draft_config_options(view: &Value, draft: &Value) -> Value {
        let key = if draft["modelSelection"].is_object() {
            "effectiveSelection"
        } else {
            "preferredSelection"
        };
        let selection = &view[key];
        let (Some(provider), Some(model)) = (text(selection, "providerId"), text(selection, "modelId")) else {
            return json!([]);
        };
        let Some(values) = find_model(view, provider, model)
            .and_then(|model| model["config"]["optionSpecs"]["reasoningLevel"]["values"].as_array())
        else {
            return json!([]);
        };
        json!([{
            "id": "thought_level",
            "name": "Reasoning",
            "category": "thought_level",
            "type": "select",
            "currentValue": selection["options"]["reasoningLevel"].as_str().unwrap_or(""),
            "options": values.iter().map(|value| json!({"value": value, "name": value})).collect::<Vec<_>>(),
        }])
    }

    // ── 命令 ──

    pub(crate) async fn handle_workspace_list(self: &Arc<Self>, inbound: &Inbound, auth: Auth) -> Result<Vec<Value>> {
        let actor = &inbound.actor;
        let locale = auth.locale.as_deref();
        if self.is_active_task_running(&auth.context).await {
            return Ok(vec![outbound(actor, msg(locale, "taskRunning", &[]), OutboundExtras::default())]);
        }
        let current = context_ref(&auth.context);
        let (bot, workspaces) = self
            .normalize_workspace_config(&auth.config, &auth.bot, current)
            .await?;
        let visible = filter_allowed_workspaces(&workspaces, &string_list(&bot["allowedWorkspaces"]));
        if visible.is_empty() {
            guard(&self.pending_workspaces).remove(&self.actor_key(actor));
            return Ok(vec![outbound(actor, msg(locale, "workspaceMissing", &[]), OutboundExtras::default())]);
        }
        guard(&self.pending_workspaces).insert(
            self.actor_key(actor),
            visible
                .iter()
                .map(|workspace| (text(workspace, "id").unwrap_or_default().to_owned(), workspace.clone()))
                .collect(),
        );
        let current_id = text(&auth.context, "workspaceId");
        let label = visible
            .iter()
            .find(|workspace| current_id.is_some() && text(workspace, "id") == current_id)
            .and_then(|workspace| text(workspace, "label").map(str::to_owned))
            .unwrap_or_else(|| text(&auth.context, "workspacePath").unwrap_or_default().to_owned());
        let options: Vec<Value> = visible
            .iter()
            .map(|workspace| json!({"id": workspace["id"], "label": workspace_option_label(workspace, locale)}))
            .collect();
        Ok(self.selection_reply(
            actor,
            json!({
                "id": format!("workspace-{}", now_ms()),
                "title": msg(locale, "workspaceSelectTitle", &[("workspace", &label)]),
                "currentId": current_id,
                "action": "workspace.set",
                "options": options,
            }),
            locale,
            None,
        ))
    }

    pub(crate) async fn handle_workspace_set(self: &Arc<Self>, inbound: &Inbound, auth: Auth, value: &str) -> Result<Vec<Value>> {
        let actor = &inbound.actor;
        let locale = auth.locale.as_deref();
        if self.is_active_task_running(&auth.context).await {
            return Ok(vec![outbound(actor, msg(locale, "taskRunning", &[]), OutboundExtras::default())]);
        }
        let (bot, workspaces) = self
            .normalize_workspace_config(&auth.config, &auth.bot, context_ref(&auth.context))
            .await?;
        let pending = self.resolve_pending_workspace(actor, value);
        let workspace = pending.or_else(|| {
            resolve_workspace_by_value(&workspaces, value, &string_list(&bot["allowedWorkspaces"]))
        });
        let Some(workspace) = workspace else {
            return Ok(vec![outbound(actor, msg(locale, "workspaceMissing", &[]), OutboundExtras::default())]);
        };
        let mut context = auth.context.clone();
        context["workspacePath"] = workspace["workspacePath"].clone();
        match text(&workspace, "workspaceIdentity") {
            Some(identity) => context["workspaceIdentity"] = json!(identity),
            None => {
                if let Some(object) = context.as_object_mut() {
                    object.remove("workspaceIdentity");
                }
            }
        }
        context["workspaceId"] = workspace["id"].clone();
        let draft = self.initialized_draft(&context).await;
        let context = self.write_draft_context(&context, Some(draft)).await?;
        guard(&self.pending_workspaces).remove(&self.actor_key(actor));
        self.status_reply(actor, &context, locale).await
    }

    /// `resolvePendingWorkspaceSelectionEntry`。
    fn resolve_pending_workspace(&self, actor: &Actor, value: &str) -> Option<Value> {
        let key = self.actor_key(actor);
        if let Some(found) = guard(&self.pending_workspaces)
            .get(&key)
            .and_then(|entries| entries.get(value.trim()).cloned())
        {
            return Some(found);
        }
        let option = self.resolve_pending_option(actor, "workspace.set", value)?;
        guard(&self.pending_workspaces)
            .get(&key)
            .and_then(|entries| entries.get(text(&option, "id").unwrap_or_default()).cloned())
    }

    pub(crate) async fn handle_model_list(self: &Arc<Self>, inbound: &Inbound, auth: Auth) -> Result<Vec<Value>> {
        let actor = &inbound.actor;
        let locale = auth.locale.as_deref();
        if self.is_active_task_running(&auth.context).await {
            return Ok(vec![outbound(actor, msg(locale, "taskRunning", &[]), OutboundExtras::default())]);
        }
        let missing = || vec![outbound(actor, msg(locale, "modelProviderMissing", &[]), OutboundExtras::default())];
        let (current_value, label_context) = if is_draft(&auth.context) {
            let draft = self.draft_for_display(&auth.context).await?;
            (selection_value(&draft["modelSelection"]), auth.context.clone())
        } else {
            let Some(active) = self.require_active_task(actor, &auth).await? else {
                return Ok(vec![outbound(actor, msg(locale, "noActiveTask", &[]), OutboundExtras::default())]);
            };
            (active.current_model(), active.context())
        };
        let providers = self.provider_options_for(&label_context).await;
        if providers.is_empty() {
            return Ok(missing());
        }
        let current_id = self.current_provider_id(current_value.as_deref(), &label_context).await;
        let label = self.model_label(current_value.as_deref(), &label_context).await;
        Ok(self.selection_reply(
            actor,
            json!({
                "id": format!("model-{}", now_ms()),
                "title": msg(locale, "modelProviderSelectTitle", &[("model", &label)]),
                "currentId": current_id,
                "action": "model.provider.set",
                "options": providers,
            }),
            locale,
            None,
        ))
    }

    pub(crate) async fn handle_model_provider_set(self: &Arc<Self>, inbound: &Inbound, auth: Auth, value: &str) -> Result<Vec<Value>> {
        let actor = &inbound.actor;
        let locale = auth.locale.as_deref();
        if self.is_active_task_running(&auth.context).await {
            return Ok(vec![outbound(actor, msg(locale, "taskRunning", &[]), OutboundExtras::default())]);
        }
        let (current_value, label_context, title_value) = if is_draft(&auth.context) {
            let draft = self.draft_for_display(&auth.context).await?;
            let current = selection_value(&draft["modelSelection"]);
            (current.clone(), auth.context.clone(), current)
        } else {
            let Some(active) = self.require_active_task(actor, &auth).await? else {
                return Ok(vec![outbound(actor, msg(locale, "noActiveTask", &[]), OutboundExtras::default())]);
            };
            let current = active.current_model();
            (current.clone(), active.context(), current)
        };
        let providers = self.provider_options_for(&label_context).await;
        let provider = self
            .resolve_pending_option(actor, "model.provider.set", value)
            .or_else(|| resolve_option(&providers, value));
        let Some(provider) = provider.filter(|provider| text(provider, "id").is_some()) else {
            return Ok(vec![outbound(actor, msg(locale, "modelProviderMissing", &[]), OutboundExtras::default())]);
        };
        let mut options: Vec<Value> = provider["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|model| model["id"].is_string() && model["label"].is_string())
            .cloned()
            .collect();
        if options.is_empty() {
            options = providers
                .iter()
                .find(|candidate| candidate["id"] == provider["id"])
                .and_then(|candidate| candidate["models"].as_array().cloned())
                .unwrap_or_default();
        }
        if options.is_empty() {
            return Ok(vec![outbound(actor, msg(locale, "modelMissing", &[]), OutboundExtras::default())]);
        }
        let current_id = current_value
            .filter(|current| options.iter().any(|option| text(option, "id") == Some(current.as_str())));
        Ok(self.selection_reply(
            actor,
            json!({
                "id": format!("model-{}", now_ms()),
                "title": msg(locale, "modelModelSelectTitle", &[("model", title_value.as_deref().unwrap_or("-"))]),
                "currentId": current_id,
                "action": "model.set",
                "options": options,
            }),
            locale,
            None,
        ))
    }

    pub(crate) async fn handle_model_set(self: &Arc<Self>, inbound: &Inbound, auth: Auth, value: &str) -> Result<Vec<Value>> {
        let actor = &inbound.actor;
        let locale = auth.locale.as_deref();
        let missing = || vec![outbound(actor, msg(locale, "modelMissing", &[]), OutboundExtras::default())];
        if self.is_active_task_running(&auth.context).await {
            return Ok(vec![outbound(actor, msg(locale, "taskRunning", &[]), OutboundExtras::default())]);
        }
        if is_draft(&auth.context) {
            let draft = self.ensure_draft_options(&auth.context).await?;
            let all: Vec<Value> = self
                .provider_options_for(&auth.context)
                .await
                .iter()
                .flat_map(|provider| provider["models"].as_array().cloned().unwrap_or_default())
                .collect();
            let model = self
                .resolve_pending_option(actor, "model.set", value)
                .or_else(|| resolve_option(&all, value));
            let Some(model) = model else {
                return Ok(missing());
            };
            let identity = parse_model_option(text(&model, "id").unwrap_or_default());
            let view = self.model_view_for(&auth.context, None).await;
            // 主动选模也取目标最高档；旧菜单失效或读取失败不能清掉已保存的选择。
            let Some(selection) = view
                .as_ref()
                .zip(identity.as_ref())
                .and_then(|(view, identity)| complete_new_selection(view, identity))
            else {
                return Ok(missing());
            };
            let mut next_draft = draft.clone();
            next_draft["modelSelection"] = selection;
            let context = self.write_draft_options(&auth.context, next_draft).await?;
            return self.status_reply(actor, &context, locale).await;
        }
        let Some(active) = self.require_active_task(actor, &auth).await? else {
            return Ok(vec![outbound(actor, msg(locale, "noActiveTask", &[]), OutboundExtras::default())]);
        };
        let all: Vec<Value> = self
            .provider_options_for(&active.context())
            .await
            .iter()
            .flat_map(|provider| provider["models"].as_array().cloned().unwrap_or_default())
            .collect();
        let model = self
            .resolve_pending_option(actor, "model.set", value)
            .or_else(|| resolve_option(&all, value));
        let Some(model) = model else {
            return Ok(missing());
        };
        let next = text(&model, "id").unwrap_or_default();
        let identity = match decode_custom_model(next) {
            Some((provider, Some(name))) if !name.is_empty() => json!({"providerId": provider, "modelId": name}),
            Some(_) => return Ok(missing()),
            None => json!({"providerId": ZCODE_AGENT_PROVIDER, "modelId": next}),
        };
        let view = self.model_view_for(&active.context(), None).await;
        let Some(selection) = view.as_ref().and_then(|view| complete_new_selection(view, &identity)) else {
            return Ok(missing());
        };
        let tasks = self.tasks_for(&context_target(&auth.context)).map_err(anyhow::Error::msg)?;
        tasks
            .set_model(context_target(&auth.context), active.task_id.clone(), selection)
            .await
            .map_err(anyhow::Error::msg)?;
        self.status_reply(actor, &auth.context, locale).await
    }

    pub(crate) async fn handle_thought_list(self: &Arc<Self>, inbound: &Inbound, auth: Auth) -> Result<Vec<Value>> {
        let actor = &inbound.actor;
        let locale = auth.locale.as_deref();
        if self.is_active_task_running(&auth.context).await {
            return Ok(vec![outbound(actor, msg(locale, "taskRunning", &[]), OutboundExtras::default())]);
        }
        let (source, current, current_label) = if is_draft(&auth.context) {
            let draft = self.ensure_draft_options(&auth.context).await?;
            let view = self
                .model_view_for(&auth.context, draft.get("modelSelection").filter(|s| s.is_object()))
                .await
                .unwrap_or(Value::Null);
            let source = Self::draft_config_options(&view, &draft);
            let current = select_current(&source, "thoughtLevel");
            let label = select_label_for(&source, "thoughtLevel", current.as_deref(), locale);
            (source, current, label)
        } else {
            let Some(active) = self.require_active_task(actor, &auth).await? else {
                return Ok(vec![outbound(actor, msg(locale, "noActiveTask", &[]), OutboundExtras::default())]);
            };
            let current = select_current(&active.config_options, "thoughtLevel");
            let label = select_label_for(&active.config_options, "thoughtLevel", current.as_deref(), locale);
            (active.config_options, current, label)
        };
        let options = select_options(&source, "thoughtLevel", locale);
        if options.is_empty() {
            return Ok(vec![outbound(actor, msg(locale, "thoughtLevelMissing", &[]), OutboundExtras::default())]);
        }
        let id = select_option(&source, "thoughtLevel")
            .and_then(|option| text(option, "id"))
            .unwrap_or("thoughtLevel")
            .to_owned();
        Ok(self.selection_reply(
            actor,
            json!({
                "id": format!("{id}-{}", now_ms()),
                "title": msg(locale, "thoughtLevelSelectTitle", &[("level", current_label.as_deref().unwrap_or("-"))]),
                "currentId": current.filter(|value| !value.is_empty()),
                "action": "thoughtLevel.set",
                "options": options,
            }),
            locale,
            None,
        ))
    }

    pub(crate) async fn handle_thought_set(self: &Arc<Self>, inbound: &Inbound, auth: Auth, value: &str) -> Result<Vec<Value>> {
        let actor = &inbound.actor;
        let locale = auth.locale.as_deref();
        let missing = || vec![outbound(actor, msg(locale, "thoughtLevelMissing", &[]), OutboundExtras::default())];
        if self.is_active_task_running(&auth.context).await {
            return Ok(vec![outbound(actor, msg(locale, "taskRunning", &[]), OutboundExtras::default())]);
        }
        if is_draft(&auth.context) {
            let original = self.ensure_draft_options(&auth.context).await?;
            let view = self
                .model_view_for(&auth.context, original.get("modelSelection").filter(|s| s.is_object()))
                .await
                .unwrap_or(Value::Null);
            let source = Self::draft_config_options(&view, &original);
            // 同一个快照给出候选与当前模型；失效的原意图不能因副本为空退回 preferred。
            let key = if original["modelSelection"].is_object() {
                "effectiveSelection"
            } else {
                "preferredSelection"
            };
            let mut draft = original.clone();
            match view.get(key).filter(|selection| selection.is_object()) {
                Some(selection) => draft["modelSelection"] = selection.clone(),
                None => {
                    if let Some(object) = draft.as_object_mut() {
                        object.remove("modelSelection");
                    }
                }
            }
            let display = select_options(&source, "thoughtLevel", locale);
            let option = self
                .resolve_pending_option(actor, "thoughtLevel.set", value)
                .or_else(|| resolve_option(&display, value));
            let Some(option) = option.filter(|option| display.iter().any(|candidate| candidate["id"] == option["id"])) else {
                return Ok(missing());
            };
            if draft["modelSelection"].is_object() {
                draft["modelSelection"]["options"]["reasoningLevel"] = option["id"].clone();
            }
            let context = self.write_draft_options(&auth.context, draft).await?;
            return self.status_reply(actor, &context, locale).await;
        }
        let Some(active) = self.require_active_task(actor, &auth).await? else {
            return Ok(vec![outbound(actor, msg(locale, "noActiveTask", &[]), OutboundExtras::default())]);
        };
        let display = select_options(&active.config_options, "thoughtLevel", locale);
        let option = self
            .resolve_pending_option(actor, "thoughtLevel.set", value)
            .or_else(|| resolve_option(&display, value));
        let (Some(option), Some(_)) = (option, select_option(&active.config_options, "thoughtLevel").and_then(|o| text(o, "id"))) else {
            return Ok(missing());
        };
        let tasks = self.tasks_for(&context_target(&auth.context)).map_err(anyhow::Error::msg)?;
        tasks
            .set_thought_level(
                context_target(&auth.context),
                active.task_id.clone(),
                text(&option, "id").unwrap_or_default().to_owned(),
            )
            .await
            .map_err(anyhow::Error::msg)?;
        self.status_reply(actor, &auth.context, locale).await
    }

    /// `listContextTaskSelectionEntries`：当前 workspace（及同路径的授权 workspace）的任务。
    async fn task_entries(&self, context: &Value, user: &Value) -> Vec<TaskEntry> {
        let path = text(context, "workspacePath").unwrap_or_default();
        let identity = text(context, "workspaceIdentity");
        let current = workspace_ref(path, identity);
        let current_key = workspace_key(path, identity);
        let refs = self.workspace_refs(&json!({"currentWorkspace": current})).await;
        let allowed = filter_allowed_workspaces(&refs, &string_list(&user["allowedWorkspaces"]));
        let candidates: Vec<Value> = allowed
            .into_iter()
            .filter(|workspace| {
                let key = workspace_key(
                    text(workspace, "workspacePath").unwrap_or_default(),
                    text(workspace, "workspaceIdentity"),
                );
                key == current_key || (identity.is_none() && text(workspace, "workspacePath") == Some(path))
            })
            .collect();
        let workspaces = if candidates.is_empty() { vec![current] } else { candidates };
        let mut entries: Vec<(String, TaskEntry)> = Vec::new();
        for workspace in workspaces {
            let workspace_path = text(&workspace, "workspacePath").unwrap_or_default().to_owned();
            let workspace_identity = text(&workspace, "workspaceIdentity").map(str::to_owned);
            let target = context_target(&workspace);
            let Ok(tasks) = self.tasks_for(&target) else {
                continue;
            };
            for task in tasks.list(target.key()).await {
                let key = format!("{}:{}", target.key(), text(&task, "taskId").unwrap_or_default());
                let entry = TaskEntry {
                    task,
                    workspace_path: workspace_path.clone(),
                    workspace_identity: workspace_identity.clone(),
                };
                match entries.iter_mut().find(|(existing, _)| *existing == key) {
                    Some(slot) => slot.1 = entry,
                    None => entries.push((key, entry)),
                }
            }
        }
        entries.into_iter().map(|(_, entry)| entry).collect()
    }

    pub(crate) async fn handle_task_list(self: &Arc<Self>, inbound: &Inbound, auth: Auth) -> Result<Vec<Value>> {
        let actor = &inbound.actor;
        let locale = auth.locale.as_deref();
        if self.is_active_task_running(&auth.context).await {
            // 运行中展示 /task 会让用户继续点选其它任务，留下误导性的待选项。
            return Ok(vec![outbound(actor, msg(locale, "taskRunning", &[]), OutboundExtras::default())]);
        }
        let entries: Vec<TaskEntry> = self.task_entries(&auth.context, &auth.user).await.into_iter().take(10).collect();
        let active_id = text(&auth.context, "activeTaskId");
        let active = entries
            .iter()
            .find(|entry| active_id.is_some() && text(&entry.task, "taskId") == active_id);
        let task_label = active
            .map(|entry| {
                format!(
                    "{} ({})",
                    text(&entry.task, "title").unwrap_or_default(),
                    text(&entry.task, "taskId").unwrap_or_default()
                )
            })
            .unwrap_or_else(|| "draft".into());
        let key = self.actor_key(actor);
        if entries.is_empty() {
            guard(&self.pending_tasks).remove(&key);
            return Ok(vec![outbound(actor, msg(locale, "noHistoryTasks", &[]), OutboundExtras::default())]);
        }
        guard(&self.pending_tasks).insert(
            key,
            entries
                .iter()
                .map(|entry| (text(&entry.task, "taskId").unwrap_or_default().to_owned(), entry.clone()))
                .collect::<HashMap<_, _>>(),
        );
        let options: Vec<Value> = entries
            .iter()
            .map(|entry| {
                json!({"id": entry.task["taskId"], "label": entry.task["title"], "description": task_status(&entry.task)})
            })
            .collect();
        Ok(self.selection_reply(
            actor,
            json!({
                "id": format!("task-{}", now_ms()),
                "title": msg(locale, "taskSelectTitle", &[("task", &task_label)]),
                "currentId": active_id,
                "action": "task.set",
                "options": options,
            }),
            locale,
            None,
        ))
    }

    /// `resolveTaskSelectionEntry`。
    async fn resolve_task_entry(&self, actor: &Actor, context: &Value, user: &Value, value: &str) -> Option<TaskEntry> {
        let key = self.actor_key(actor);
        if let Some(entry) = guard(&self.pending_tasks).get(&key).and_then(|entries| entries.get(value.trim()).cloned()) {
            return Some(entry);
        }
        if let Some(option) = self.resolve_pending_option(actor, "task.set", value)
            && let Some(entry) = guard(&self.pending_tasks)
                .get(&key)
                .and_then(|entries| entries.get(text(&option, "id").unwrap_or_default()).cloned())
        {
            return Some(entry);
        }
        let entries = self.task_entries(context, user).await;
        let items: Vec<Value> = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| json!({"id": entry.task["taskId"], "label": entry.task["title"], "index": index}))
            .collect();
        if let Some(found) = resolve_option(&items, value) {
            return entries.get(found["index"].as_u64()? as usize).cloned();
        }
        let target = context_target(context);
        let tasks = self.tasks_for(&target).ok()?;
        let snapshot = tasks.read_session(target, value.trim().to_owned()).await.ok()?;
        Some(TaskEntry {
            task: crate::tasks::meta::meta_from_snapshot(&snapshot),
            workspace_path: text(context, "workspacePath").unwrap_or_default().to_owned(),
            workspace_identity: text(context, "workspaceIdentity").map(str::to_owned),
        })
    }

    pub(crate) async fn handle_task_set(self: &Arc<Self>, inbound: &Inbound, auth: Auth, value: &str) -> Result<Vec<Value>> {
        let actor = &inbound.actor;
        let locale = auth.locale.as_deref();
        let Some(entry) = self.resolve_task_entry(actor, &auth.context, &auth.user, value).await else {
            return Ok(vec![outbound(actor, msg(locale, "taskMissing", &[]), OutboundExtras::default())]);
        };
        let task_id = text(&entry.task, "taskId").unwrap_or_default().to_owned();
        if text(&auth.context, "activeTaskId") != Some(task_id.as_str()) && self.is_active_task_running(&auth.context).await {
            // 运行中的旧任务已经订阅了回复流：此时改写 activeTaskId 会让两个任务的输出串线。
            return Ok(vec![outbound(actor, msg(locale, "taskRunning", &[]), OutboundExtras::default())]);
        }
        let mut context = auth.context.clone();
        context["workspacePath"] = json!(entry.workspace_path);
        match &entry.workspace_identity {
            Some(identity) => context["workspaceIdentity"] = json!(identity),
            None => {
                if let Some(object) = context.as_object_mut() {
                    object.remove("workspaceIdentity");
                }
            }
        }
        context["workspaceId"] = json!(workspace_key(&entry.workspace_path, entry.workspace_identity.as_deref()));
        context["mode"] = json!("task");
        context["activeTaskId"] = json!(task_id);
        self.write_context(&context).await?;
        guard(&self.pending_tasks).remove(&self.actor_key(actor));
        self.status_reply(actor, &context, locale).await
    }

    pub(crate) async fn handle_reply_list(&self, inbound: &Inbound, auth: Auth) -> Result<Vec<Value>> {
        let locale = auth.locale.as_deref();
        let provider = text(&auth.bot, "provider").unwrap_or_default();
        let label = reply_label(text(&auth.bot, "replyMode"), locale, provider);
        Ok(self.selection_reply(
            &inbound.actor,
            json!({
                "id": format!("reply-{}", now_ms()),
                "title": msg(locale, "replySelectTitle", &[("mode", &label)]),
                "currentId": normalize_reply_mode(provider, text(&auth.bot, "replyMode")),
                "action": "reply.set",
                "options": reply_options(locale, provider),
            }),
            locale,
            None,
        ))
    }

    pub(crate) async fn handle_reply_set(self: &Arc<Self>, inbound: &Inbound, auth: Auth, value: &str) -> Result<Vec<Value>> {
        let actor = &inbound.actor;
        let locale = auth.locale.as_deref();
        let provider = text(&auth.bot, "provider").unwrap_or_default();
        let Some(option) = resolve_reply(value, locale, provider) else {
            return Ok(vec![outbound(actor, msg(locale, "replyMissing", &[]), OutboundExtras::default())]);
        };
        let mut bot = auth.bot.clone();
        bot["replyMode"] = option["id"].clone();
        self.save_bot(&json!({"bot": bot}))
            .await
            .map_err(|error| anyhow::anyhow!(error.message))?;
        self.status_reply(actor, &auth.context, locale).await
    }
}

/// 草稿态（`context.mode === "draft" || !context.activeTaskId`）。
pub fn is_draft(context: &Value) -> bool {
    context["mode"] == "draft" || text(context, "activeTaskId").is_none()
}

/// 当前上下文的 workspace 引用（`createCurrentWorkspaceRef`）。
pub fn context_ref(context: &Value) -> Value {
    let path = text(context, "workspacePath").unwrap_or_default();
    let mut workspace = workspace_ref(path, text(context, "workspaceIdentity"));
    if let Some(id) = text(context, "workspaceId") {
        workspace["id"] = json!(id);
    }
    workspace["label"] = json!(workspace_label(path));
    workspace
}

/// `resolveWorkspaceByValue`：序号（`parseInt`）、id / label（忽略大小写）、路径或身份 key。
pub fn resolve_workspace_by_value(workspaces: &[Value], value: &str, allowed: &[String]) -> Option<Value> {
    let trimmed = value.trim();
    let visible = filter_allowed_workspaces(workspaces, allowed);
    if let Some(index) = parse_int(trimmed).filter(|index| *index > 0) {
        return visible.get(index as usize - 1).cloned();
    }
    let normalized = normalize_text(trimmed);
    visible.into_iter().find(|workspace| {
        normalize_text(text(workspace, "id").unwrap_or_default()) == normalized
            || normalize_text(text(workspace, "label").unwrap_or_default()) == normalized
            || text(workspace, "workspacePath") == Some(trimmed)
            || workspace_key(
                text(workspace, "workspacePath").unwrap_or_default(),
                text(workspace, "workspaceIdentity"),
            ) == trimmed
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_helpers_match_node() {
        let items = vec![json!({"id": "a", "label": "Alpha"}), json!({"id": "b", "label": "Beta"})];
        assert_eq!(resolve_option(&items, "2").unwrap()["id"], "b");
        assert_eq!(resolve_option(&items, " alpha ").unwrap()["id"], "a");
        assert!(resolve_option(&items, "0").is_none());
        assert!(resolve_option(&items, "3").is_none());
        assert!(is_index_value("12") && !is_index_value("01") && !is_index_value("1a"));
        assert_eq!(parse_int("2abc"), Some(2));
        assert_eq!(parse_int("abc"), None);
        assert_eq!(selection_value(&json!({"providerId": "glm", "modelId": "GLM-5"})).as_deref(), Some("GLM-5"));
        assert_eq!(
            selection_value(&json!({"providerId": "my p", "modelId": "m/1"})).as_deref(),
            Some("custom:my%20p:m%2F1")
        );
        assert_eq!(parse_model_option("custom:my%20p:m%2F1"), Some(json!({"providerId": "my p", "modelId": "m/1"})));
        assert_eq!(parse_model_option("zai/GLM-5"), Some(json!({"providerId": "zai", "modelId": "GLM-5"})));
        assert_eq!(parse_model_option(" x "), Some(json!({"providerId": "glm", "modelId": "x"})));
        let reply = reply_options(None, "telegram");
        assert_eq!(reply.len(), 3);
        assert_eq!(resolve_reply("2", None, "telegram").unwrap()["id"], "assistant_toolcalls_changes");
        assert_eq!(resolve_reply("summary", None, "telegram").unwrap()["id"], "summary_changes");
        assert!(resolve_reply("stream", None, "telegram").is_none());
        assert_eq!(reply_options(Some("en-US"), "feishu"), vec![json!({"id": "streaming_card", "label": "Streaming card"})]);
        let marked = mark_current(&json!({"currentId": "b", "options": items}), None);
        assert_eq!(marked["options"][1]["label"], "Beta · 当前");
        assert_eq!(marked["cancelLabel"], "取消");
        let fallback = selection_fallback(&json!({"title": "T", "cancelLabel": "取消", "options": [{"id": "a", "label": "A", "description": "d"}]}), None);
        assert_eq!(fallback, "T\n0. 取消\n1. A d\n\n回复数字选择，0 取消。");
        let view = json!({"providers": [{"providerId": "p", "providerName": " P ", "models": [{"modelId": "m",
            "config": {"optionSpecs": {"reasoningLevel": {"values": ["low", "high"]}}}}]}, {"providerId": "empty", "models": []}]});
        assert_eq!(provider_options(&view), vec![json!({"id": "p", "label": "P", "models": [{"id": "custom:p:m", "label": "m"}]})]);
        assert_eq!(
            complete_new_selection(&view, &json!({"providerId": "p", "modelId": "m"})),
            Some(json!({"providerId": "p", "modelId": "m", "options": {"reasoningLevel": "high"}}))
        );
    }
}
