//! 问答与权限（botsService.ts 的 elicitation / permission 分支，飞书临时交互卡片）。
//!
//! 会话出现待处理交互时（V4 `pendingInteractions`），Bot 把它转成第三方可回答的选择；回答经 V4
//! `resolveInteraction` 提交。待答状态写在 `bot-state.v3.json` 的上下文里（与 Node 同格式）。
use std::sync::Arc;

use anyhow::{Result, anyhow};
use serde_json::{Map, Value, json};
use zcode_host_kit::log;

use super::feishu::{Actor, Inbound};
use super::inbound::Auth;
use super::model::{is_feishu, message as msg, normalize_reply_mode};
use super::now_ms;
use super::providers::{OutboundExtras, outbound};
use super::reply::permission_summary;
use super::selection::resolve_option;
use super::service::{Bots, TransientCard, guard};
use super::util::{decode_uri_component, sha256_hex, text};
use super::watch::{Watch, context_target, strip_pending};

const CUSTOM_OPTION_ID: &str = "__custom__";
const SUBMIT_OPTION_ID: &str = "__submit__";
const SKIP_OPTION_ID: &str = "__skip__";
const FORM_VALUE_PREFIX: &str = "__form__:";

fn questions(pending: &Value) -> Vec<Value> {
    pending["questions"].as_array().cloned().unwrap_or_default()
}

fn current_index(pending: &Value) -> usize {
    pending["currentQuestionIndex"].as_u64().unwrap_or(0) as usize
}

fn answer_values(pending: &Value, index: usize) -> Vec<String> {
    pending["answers"][index.to_string()]
        .as_array()
        .map(|values| values.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

/// `getPendingElicitationSelectionToken`：sha256(taskId::runId::requestId::题号) 前 12 位。
pub fn selection_token(pending: &Value) -> String {
    let source = format!(
        "{}::{}::{}::{}",
        text(pending, "taskId").unwrap_or_default(),
        text(pending, "runId").unwrap_or_default(),
        text(pending, "requestId").unwrap_or_default(),
        current_index(pending)
    );
    sha256_hex(source.as_bytes())[..12].to_owned()
}

fn skip_option_id(pending: &Value) -> String {
    format!("{SKIP_OPTION_ID}:{}", selection_token(pending))
}

fn plan_schema(pending: &Value) -> Option<Value> {
    (pending["renderContext"]["kind"] == "plan_approval")
        .then(|| json!({"interaction": "plan_approval", "plan": pending["renderContext"]["plan"]}))
}

/// `normalizeBotElicitationQuestions`（V4 `userInput` 交互）。
fn normalize_questions(payload: &Value, locale: Option<&str>) -> Vec<Value> {
    if payload["schema"]["interaction"] == "plan_approval" {
        return vec![json!({
            "question": msg(locale, "planApprovalTitle", &[]),
            "header": msg(locale, "planApprovalHeader", &[]),
            "options": [{
                "value": "approve",
                "label": msg(locale, "planApprovalApprove", &[]),
                "description": msg(locale, "planApprovalApproveDescription", &[]),
            }],
        })];
    }
    let source: Vec<Value> = match payload["questions"].as_array().filter(|q| !q.is_empty()) {
        Some(questions) => questions.clone(),
        None => {
            let prompt = text(payload, "prompt").unwrap_or_default();
            let options: Vec<Value> = payload["options"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|option| json!({"value": option["optionId"], "label": option["label"]}))
                .collect();
            vec![json!({"question": prompt, "header": prompt, "options": options})]
        }
    };
    source
        .iter()
        .map(|question| {
            let text_value = text(question, "question").unwrap_or_default();
            let header = text(question, "header").filter(|h| !h.is_empty()).unwrap_or(text_value);
            let options: Vec<Value> = question["options"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|option| {
                    let value = text(option, "value").unwrap_or_default();
                    let mut out = Map::new();
                    out.insert("value".into(), json!(value));
                    out.insert(
                        "label".into(),
                        json!(text(option, "label").filter(|l| !l.is_empty()).unwrap_or(value)),
                    );
                    if let Some(description) = text(option, "description") {
                        out.insert("description".into(), json!(description));
                    }
                    Value::Object(out)
                })
                .collect();
            let mut out = Map::new();
            out.insert("question".into(), json!(text_value));
            out.insert("header".into(), json!(header));
            out.insert("options".into(), json!(options));
            if question["multiSelect"] == true {
                out.insert("multiSelect".into(), json!(true));
            }
            Value::Object(out)
        })
        .collect()
}

/// `formatBotElicitationTitle`。
fn elicitation_title(pending: &Value, locale: Option<&str>) -> String {
    let index = current_index(pending);
    let all = questions(pending);
    let Some(question) = all.get(index) else {
        return text(pending, "requestId").unwrap_or_default().to_owned();
    };
    let expanded = pending["expandedCustomAnswerQuestionIndexes"]
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item.as_u64() == Some(index as u64)));
    if pending["renderContext"]["kind"] == "plan_approval" {
        let mut parts = vec![
            text(&pending["renderContext"], "plan").unwrap_or_default().to_owned(),
            "------".to_owned(),
            msg(locale, "planApprovalTitle", &[]),
        ];
        if expanded {
            parts.push(msg(locale, "elicitationCustomPlaceholder", &[]));
        }
        return parts.into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n\n");
    }
    let question_text = text(question, "question").unwrap_or_default();
    let header = text(question, "header").filter(|h| !h.is_empty() && *h != question_text);
    let mut parts: Vec<String> = Vec::new();
    if all.len() > 1 {
        parts.push(format!("{}/{}", index + 1, all.len()));
    }
    if let Some(header) = header {
        parts.push(header.to_owned());
    }
    parts.push(question_text.to_owned());
    if question["multiSelect"] == true {
        parts.push(msg(locale, "elicitationMultiSelectHint", &[]));
    }
    if expanded {
        parts.push(msg(locale, "elicitationCustomPlaceholder", &[]));
    }
    parts.push(msg(locale, "elicitationTextHint", &[]));
    parts.into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n")
}

/// `createBotElicitationSelection`。
fn elicitation_selection(pending: &Value, locale: Option<&str>) -> Value {
    let index = current_index(pending);
    let question = questions(pending).get(index).cloned();
    let selected = answer_values(pending, index);
    let plan = pending["renderContext"]["kind"] == "plan_approval";
    let multi = question.as_ref().is_some_and(|q| q["multiSelect"] == true);
    let mut options: Vec<Value> = question
        .as_ref()
        .and_then(|q| q["options"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .map(|option| {
            let value = text(option, "value").unwrap_or_default();
            let label = text(option, "label").unwrap_or_default();
            let mut out = Map::new();
            out.insert("id".into(), json!(value));
            out.insert(
                "label".into(),
                json!(if multi {
                    format!("{} {label}", if selected.iter().any(|v| v == value) { "[x]" } else { "[ ]" })
                } else {
                    label.to_owned()
                }),
            );
            if !plan && let Some(description) = text(option, "description") {
                out.insert("description".into(), json!(description));
            }
            Value::Object(out)
        })
        .collect();
    if plan {
        options.push(json!({"id": CUSTOM_OPTION_ID, "label": msg(locale, "elicitationCustomOption", &[])}));
    }
    if multi {
        options.push(json!({"id": SUBMIT_OPTION_ID, "label": msg(locale, "elicitationSubmitOption", &[])}));
    } else if question.is_some() {
        options.push(json!({"id": skip_option_id(pending), "label": msg(locale, "elicitationSkipOption", &[])}));
    }
    json!({
        "id": format!("elicitation-{}-{index}", text(pending, "requestId").unwrap_or_default()),
        "token": selection_token(pending),
        "title": elicitation_title(pending, locale),
        "action": "elicitation.respond",
        "options": options,
    })
}

fn elicitation_extras(pending: &Value, status: &str, index: usize, with_expanded: bool) -> Value {
    let mut out = Map::new();
    out.insert("requestId".into(), pending["requestId"].clone());
    out.insert("taskId".into(), pending["taskId"].clone());
    out.insert("runId".into(), pending["runId"].clone());
    out.insert("currentQuestionIndex".into(), json!(index));
    out.insert("questions".into(), pending["questions"].clone());
    out.insert("answers".into(), pending["answers"].clone());
    out.insert("status".into(), json!(status));
    if with_expanded
        && let Some(expanded) = pending["expandedCustomAnswerQuestionIndexes"].as_array().filter(|e| !e.is_empty())
    {
        out.insert("expandedCustomAnswerQuestionIndexes".into(), json!(expanded));
    }
    if let Some(schema) = plan_schema(pending) {
        out.insert("schema".into(), schema);
    }
    Value::Object(out)
}

/// `buildBotElicitationContent`：未回答的题目不写 key（缺 key 表示跳过）。
fn elicitation_content(pending: &Value, answers: &Value) -> Value {
    let all = questions(pending);
    let values_of = |index: usize| -> Vec<String> {
        answers[index.to_string()]
            .as_array()
            .map(|values| values.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default()
    };
    let mut answer_map = Map::new();
    for (index, question) in all.iter().enumerate() {
        let joined = values_of(index).join(", ").trim().to_owned();
        if !joined.is_empty() {
            answer_map.insert(text(question, "question").unwrap_or_default().to_owned(), json!(joined));
        }
    }
    let mut content = Map::new();
    content.insert("answers".into(), Value::Object(answer_map));
    for (index, question) in all.iter().enumerate() {
        let values = values_of(index);
        if !values.is_empty() {
            content.insert(
                format!("answer_{index}"),
                if question["multiSelect"] == true { json!(values) } else { json!(values[0]) },
            );
        }
    }
    if all.len() == 1 {
        let values = values_of(0);
        if !values.is_empty() {
            content.insert(
                "answer".into(),
                if all[0]["multiSelect"] == true { json!(values) } else { json!(values[0]) },
            );
        }
    }
    Value::Object(content)
}

/// `parseElicitationResponseValue`：`<12 位 token> <值>`。
fn parse_response_value(value: &str) -> (Option<String>, String) {
    let trimmed = value.trim();
    let mut parts = trimmed.split_whitespace();
    if let (Some(first), Some(_)) = (parts.next(), trimmed.split_whitespace().nth(1))
        && first.len() == 12
        && first.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        let rest: Vec<&str> = trimmed.split_whitespace().skip(1).collect();
        return (Some(first.to_lowercase()), rest.join(" "));
    }
    (None, trimmed.to_owned())
}

/// `parseElicitationFormValues`。
fn parse_form_values(value: &str) -> Option<Vec<String>> {
    let encoded = value.strip_prefix(FORM_VALUE_PREFIX)?;
    let parsed = decode_uri_component(encoded).and_then(|decoded| serde_json::from_str::<Value>(&decoded).ok());
    let Some(parsed) = parsed else {
        return Some(Vec::new());
    };
    let values = match parsed {
        Value::Array(items) => items,
        other => vec![other],
    };
    Some(
        values
            .iter()
            .map(|item| item.as_str().map(|s| s.trim().to_owned()).unwrap_or_default())
            .filter(|item| !item.is_empty())
            .collect(),
    )
}

/// `mergeElicitationFormValues`。
fn merge_form_values(question: &Value, selected: &[String], form: &[String]) -> Vec<String> {
    let custom: Vec<String> = form.iter().filter(|v| !v.is_empty()).cloned().collect();
    if question["multiSelect"] != true {
        return if custom.is_empty() {
            selected.iter().take(1).cloned().collect()
        } else {
            custom.into_iter().take(1).collect()
        };
    }
    let mut merged: Vec<String> = Vec::new();
    for value in selected.iter().chain(custom.iter()) {
        if !value.is_empty() && !merged.contains(value) {
            merged.push(value.clone());
        }
    }
    merged
}

/// `resolveElicitationQuestionValue`。
fn resolve_question_value(question: &Value, value: &str, include_submit: bool) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let multi = question["multiSelect"] == true;
    let normalized = trimmed.to_lowercase();
    if multi && include_submit && ["submit", "done", "完成", "提交", SUBMIT_OPTION_ID].contains(&normalized.as_str()) {
        return SUBMIT_OPTION_ID.into();
    }
    let items: Vec<Value> = question["options"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|option| json!({"id": option["value"], "label": option["label"]}))
        .collect();
    if let Some(option) = resolve_option(&items, trimmed) {
        return text(&option, "id").unwrap_or_default().to_owned();
    }
    if multi
        && include_submit
        && super::selection::is_index_value(trimmed)
        && trimmed.parse::<usize>().ok() == Some(items.len() + 1)
    {
        return SUBMIT_OPTION_ID.into();
    }
    trimmed.to_owned()
}

impl Bots {
    fn owned_by(&self, pending: &Value, actor: &Actor) -> bool {
        text(pending, "actorKey").is_none_or(|key| key == self.actor_key(actor))
    }

    /// `clearPendingElicitationSelection`。
    pub(crate) fn clear_pending_elicitation_selection(&self, pending: &Value) {
        let token = selection_token(pending);
        let prefix = format!("elicitation-{}-", text(pending, "requestId").unwrap_or_default());
        guard(&self.pending_selections).retain(|_, selection| {
            !(selection["action"] == "elicitation.respond"
                && (text(selection, "token") == Some(token.as_str())
                    || text(selection, "id").is_some_and(|id| id.starts_with(&prefix))))
        });
    }

    /// `createElicitationReply`。
    fn elicitation_reply(&self, actor: &Actor, pending: &Value, locale: Option<&str>) -> Vec<Value> {
        let extras = elicitation_extras(pending, "pending", current_index(pending), true);
        self.selection_reply(actor, elicitation_selection(pending, locale), locale, Some(extras))
    }

    /// `createCompletedElicitationOutbound`。
    pub(crate) fn completed_elicitation_outbound(
        &self,
        actor: &Actor,
        pending: &Value,
        locale: Option<&str>,
        action: &str,
    ) -> Value {
        let status = if action == "cancel" { "cancelled" } else { "completed" };
        let last = questions(pending).len().saturating_sub(1);
        outbound(
            actor,
            msg(
                locale,
                if action == "accept" { "elicitationSubmitted" } else { "elicitationCancelled" },
                &[],
            ),
            OutboundExtras {
                locale: locale.map(str::to_owned),
                elicitation: Some(elicitation_extras(pending, status, last, false)),
                ..OutboundExtras::default()
            },
        )
    }

    async fn write_pending(&self, context: &Value, pending: Option<&Value>) -> Result<Value> {
        let mut next = context.clone();
        match pending {
            Some(pending) => next["pendingElicitation"] = pending.clone(),
            None => {
                if let Some(object) = next.as_object_mut() {
                    object.remove("pendingElicitation");
                }
            }
        }
        self.write_context(&strip_pending(&next)).await?;
        Ok(next)
    }

    /// 交互已在别处回答（从待处理列表消失）：清掉对应的待答状态（`clearPendingElicitationForRequest`）。
    pub(crate) async fn clear_elicitation_for_request(&self, context: &mut Value, request_id: &str) -> Result<()> {
        let pending = context["pendingElicitation"].clone();
        if text(&pending, "requestId") != Some(request_id) {
            return Ok(());
        }
        self.clear_pending_elicitation_selection(&pending);
        *context = self.write_pending(context, None).await?;
        Ok(())
    }

    // ── 飞书临时交互卡片 ──

    fn uses_transient_card(bot: &Value, user: &Value) -> bool {
        let provider = text(bot, "provider").unwrap_or_default();
        is_feishu(provider) && normalize_reply_mode(provider, text(user, "replyMode")) == "streaming_card"
    }

    /// `upsertTransientInteractionCard`：同一聊天只有一张临时卡，交互推进时原地 PATCH。
    pub(crate) async fn upsert_transient_card(&self, bot: &Value, actor: &Actor, task: &str, message: &Value) -> Result<()> {
        let key = self.actor_key(actor);
        let existing = guard(&self.transient_cards).get(&key).cloned();
        if let Some(card) = existing {
            let payload = super::feishu::cards::for_message(message);
            return self.feishu_update_card(&card.bot, &card.message_id, &payload).await;
        }
        if let Some(message_id) = self.feishu_create_card(bot, message).await? {
            guard(&self.transient_cards).insert(
                key,
                TransientCard {
                    bot: bot.clone(),
                    task_id: task.to_owned(),
                    message_id,
                },
            );
        }
        Ok(())
    }

    /// `finalizeTransientInteractionCard`：终态只更新成无控件卡片并释放句柄，不撤回。
    pub(crate) async fn finalize_transient_card(&self, actor: &Actor, fallback: &Value) -> bool {
        let key = self.actor_key(actor);
        let Some(card) = guard(&self.transient_cards).remove(&key) else {
            return false;
        };
        let payload = super::feishu::cards::for_message(fallback);
        if let Err(error) = self.feishu_update_card(&card.bot, &card.message_id, &payload).await {
            log::warn(format!(
                "finalize interaction card failed bot={} task={}: {error:#}",
                text(&card.bot, "id").unwrap_or_default(),
                card.task_id
            ));
        }
        true
    }

    async fn deliver_interaction(&self, watch: &Watch, task: &str, reply: &Value) -> Result<()> {
        if Self::uses_transient_card(&watch.bot, &watch.user) {
            self.upsert_transient_card(&watch.bot, &watch.actor, task, reply).await
        } else {
            self.send_outbound(&watch.bot, reply).await
        }
    }

    /// `handleElicitationRequest`。
    pub(crate) async fn handle_elicitation_request(self: &Arc<Self>, watch: &mut Watch, interaction: &Value) -> Result<()> {
        let locale = self.locale().await;
        let locale = locale.as_deref();
        let payload = &interaction["payload"];
        let task = text(&watch.context, "activeTaskId").unwrap_or_default().to_owned();
        let request_id = text(interaction, "interactionId").unwrap_or_default();
        let mut pending = Map::new();
        pending.insert("taskId".into(), json!(task));
        pending.insert("requestId".into(), json!(request_id));
        pending.insert("runId".into(), json!(text(payload, "traceId").unwrap_or(request_id)));
        if payload["origin"].is_object() {
            pending.insert("origin".into(), payload["origin"].clone());
        }
        pending.insert("actorKey".into(), json!(self.actor_key(&watch.actor)));
        pending.insert("currentQuestionIndex".into(), json!(0));
        pending.insert("questions".into(), json!(normalize_questions(payload, locale)));
        pending.insert("answers".into(), json!({}));
        if payload["schema"]["interaction"] == "plan_approval"
            && let Some(plan) = text(&payload["schema"], "plan").map(str::trim).filter(|p| !p.is_empty())
        {
            pending.insert("renderContext".into(), json!({"kind": "plan_approval", "plan": plan}));
        }
        let pending = Value::Object(pending);
        let previous = watch.context["pendingElicitation"].clone();
        if previous.is_object() {
            self.clear_pending_elicitation_selection(&previous);
        }
        watch.context = self.write_pending(&watch.context, Some(&pending)).await?;
        for reply in self.elicitation_reply(&watch.actor, &pending, locale) {
            self.deliver_interaction(watch, &task, &reply).await?;
        }
        Ok(())
    }

    /// 权限请求（`permission_request` 分支）。Bot 会话硬锁 yolo，正常不会出现。
    pub(crate) async fn handle_permission_request(self: &Arc<Self>, watch: &mut Watch, interaction: &Value) -> Result<()> {
        let locale = self.locale().await;
        let locale = locale.as_deref();
        let payload = &interaction["payload"];
        let task = text(&watch.context, "activeTaskId").unwrap_or_default().to_owned();
        let request_id = text(interaction, "interactionId").unwrap_or_default().to_owned();
        let scope = if payload["detail"]["command"].is_string() {
            "command"
        } else if ["path", "file_path", "filePath"].iter().any(|key| payload["detail"][*key].is_string()) {
            "file"
        } else {
            "other"
        };
        let mut options: Vec<Value> = payload["options"].as_array().cloned().unwrap_or_default();
        options.sort_by_key(|option| permission_priority(option));
        let mut selection_options = Vec::new();
        let mut pending_options = Vec::new();
        for option in &options {
            let option_id = text(option, "optionId").unwrap_or_default();
            let kind = permission_kind(option);
            let deny = matches!(kind, "rejectOnce" | "rejectAlways");
            let label = permission_label(option, kind, locale);
            let mut entry = Map::new();
            entry.insert(
                "id".into(),
                json!(if deny { format!("/deny {request_id}") } else { format!("/approve {request_id} {option_id}") }),
            );
            entry.insert("label".into(), json!(label));
            if let Some(description) = permission_description(option, kind, scope, locale) {
                entry.insert("description".into(), json!(description));
            }
            selection_options.push(Value::Object(entry));
            let response = option
                .get("response")
                .filter(|response| response.is_object())
                .cloned()
                .unwrap_or_else(|| json!({"decision": if deny { "deny" } else { "allow" }}));
            pending_options.push(json!({
                "requestId": request_id, "optionId": option_id,
                "command": if deny { "deny" } else { "approve" },
                "label": label, "response": response,
            }));
        }
        watch.context["pendingPermissionOptions"] = json!(pending_options);
        self.write_context(&strip_pending(&watch.context)).await?;
        let selection = json!({
            "id": format!("permission-{request_id}"),
            "title": permission_summary(payload, text(&watch.context, "workspacePath").unwrap_or_default(), locale),
            "action": "permission.respond",
            "showCancel": false,
            "options": selection_options,
        });
        if let Some(reply) = self.selection_reply(&watch.actor, selection, locale, None).into_iter().next() {
            self.deliver_interaction(watch, &task, &reply).await?;
        }
        Ok(())
    }

    // ── 回答 ──

    /// `submitPendingElicitation`：V4 resolveInteraction ACK 之后才记 handledAt 并清掉待答状态。
    async fn submit_elicitation(
        self: &Arc<Self>,
        auth: &Auth,
        actor: &Actor,
        pending: &Value,
        action: &str,
        content: Option<Value>,
    ) -> Result<Vec<Value>> {
        let locale = auth.locale.as_deref();
        if !self.owned_by(pending, actor) {
            return Ok(vec![outbound(actor, msg(locale, "elicitationExpired", &[]), OutboundExtras::default())]);
        }
        if pending["handledAt"].is_number() {
            return Ok(vec![outbound(actor, msg(locale, "elicitationHandled", &[]), OutboundExtras::default())]);
        }
        let mut answer = Map::new();
        answer.insert("action".into(), json!(action));
        if let Some(content) = content.filter(|content| content.is_object()) {
            answer.insert("content".into(), content);
        }
        let target = context_target(&auth.context);
        let tasks = self.tasks_for(&target).map_err(anyhow::Error::msg)?;
        tasks
            .resolve_interaction(
                target,
                text(pending, "taskId").unwrap_or_default().to_owned(),
                text(pending, "requestId").unwrap_or_default().to_owned(),
                Value::Object(answer),
            )
            .await
            .map_err(anyhow::Error::msg)?;
        let mut handled = pending.clone();
        handled["handledAt"] = json!(now_ms());
        self.write_pending(&auth.context, Some(&handled)).await?;
        self.clear_pending_elicitation_selection(pending);
        self.write_pending(&auth.context, None).await?;
        if action == "accept" {
            self.start_typing(&auth.bot, actor, text(pending, "taskId").unwrap_or_default());
        }
        Ok(vec![self.completed_elicitation_outbound(actor, pending, locale, action)])
    }

    /// `advancePendingElicitation`：最后一题提交，否则进入下一题。
    async fn advance_elicitation(self: &Arc<Self>, auth: &Auth, actor: &Actor, pending: &Value, answers: Value) -> Result<Vec<Value>> {
        if current_index(pending) + 1 >= questions(pending).len() {
            let mut next = pending.clone();
            next["answers"] = answers.clone();
            let content = elicitation_content(pending, &answers);
            return self.submit_elicitation(auth, actor, &next, "accept", Some(content)).await;
        }
        let mut next = pending.clone();
        next["currentQuestionIndex"] = json!(current_index(pending) + 1);
        next["answers"] = answers;
        self.write_pending(&auth.context, Some(&next)).await?;
        Ok(self.elicitation_reply(actor, &next, auth.locale.as_deref()))
    }

    /// `handlePendingElicitationValue`（按钮 / `/elicitation <token> <值>`）。
    pub(crate) async fn handle_elicitation_value(self: &Arc<Self>, auth: &Auth, actor: &Actor, value: &str) -> Result<Vec<Value>> {
        let locale = auth.locale.as_deref();
        let expired = || vec![outbound(actor, msg(locale, "elicitationExpired", &[]), OutboundExtras::default())];
        let pending = auth.context["pendingElicitation"].clone();
        if !pending.is_object() || pending["taskId"] != auth.context["activeTaskId"] || !self.owned_by(&pending, actor) {
            return Ok(expired());
        }
        let index = current_index(&pending);
        let Some(question) = questions(&pending).get(index).cloned() else {
            return Ok(expired());
        };
        let (token, parsed) = parse_response_value(value);
        if actor.provider != "weixin" && token.as_deref() != Some(selection_token(&pending).as_str()) {
            // 旧按钮可能在新一轮问答后才送达：非微信通道必须带本轮 token。
            return Ok(expired());
        }
        if parsed == skip_option_id(&pending) {
            return self.advance_elicitation(auth, actor, &pending, pending["answers"].clone()).await;
        }
        let key = index.to_string();
        if let Some(form) = parse_form_values(&parsed) {
            let next_values = merge_form_values(&question, &answer_values(&pending, index), &form);
            if next_values.is_empty() {
                return Ok(self.elicitation_reply(actor, &pending, locale));
            }
            let mut answers = pending["answers"].clone();
            answers[key] = json!(next_values);
            return self.advance_elicitation(auth, actor, &pending, answers).await;
        }
        let option = self.resolve_pending_option(actor, "elicitation.respond", &parsed);
        if option.as_ref().and_then(|o| text(o, "id")) == Some(skip_option_id(&pending).as_str()) {
            return self.advance_elicitation(auth, actor, &pending, pending["answers"].clone()).await;
        }
        let selected = resolve_question_value(
            &question,
            option.as_ref().and_then(|o| text(o, "id")).unwrap_or(&parsed),
            true,
        );
        if selected.is_empty() {
            return Ok(self.elicitation_reply(actor, &pending, locale));
        }
        if selected == SUBMIT_OPTION_ID {
            return self.advance_elicitation(auth, actor, &pending, pending["answers"].clone()).await;
        }
        if selected == CUSTOM_OPTION_ID {
            let mut next = pending.clone();
            let mut expanded: Vec<u64> = pending["expandedCustomAnswerQuestionIndexes"]
                .as_array()
                .map(|items| items.iter().filter_map(Value::as_u64).collect())
                .unwrap_or_default();
            if let Some(position) = expanded.iter().position(|item| *item == index as u64) {
                expanded.remove(position);
            } else {
                expanded.push(index as u64);
            }
            expanded.sort_unstable();
            next["expandedCustomAnswerQuestionIndexes"] = json!(expanded);
            self.write_pending(&auth.context, Some(&next)).await?;
            return Ok(self.elicitation_reply(actor, &next, locale));
        }
        if question["multiSelect"] == true {
            let mut values = answer_values(&pending, index);
            if let Some(position) = values.iter().position(|item| *item == selected) {
                values.remove(position);
            } else {
                values.push(selected);
            }
            let mut next = pending.clone();
            next["answers"][key] = json!(values);
            self.write_pending(&auth.context, Some(&next)).await?;
            return Ok(self.elicitation_reply(actor, &next, locale));
        }
        let mut answers = pending["answers"].clone();
        answers[key] = json!([selected]);
        self.advance_elicitation(auth, actor, &pending, answers).await
    }

    /// `handlePendingElicitationText`：普通消息作为当前题的答案；没有待答问答时返回 None。
    pub(crate) async fn handle_elicitation_text(self: &Arc<Self>, auth: &Auth, actor: &Actor, body: &str) -> Result<Option<Vec<Value>>> {
        let pending = auth.context["pendingElicitation"].clone();
        if !pending.is_object() || pending["taskId"] != auth.context["activeTaskId"] || !self.owned_by(&pending, actor) {
            return Ok(None);
        }
        let locale = auth.locale.as_deref();
        let value = body.trim();
        if value.is_empty() {
            return Ok(Some(self.elicitation_reply(actor, &pending, locale)));
        }
        if value == skip_option_id(&pending) {
            return self.advance_elicitation(auth, actor, &pending, pending["answers"].clone()).await.map(Some);
        }
        let index = current_index(&pending);
        let Some(question) = questions(&pending).get(index).cloned() else {
            return Ok(Some(vec![outbound(actor, msg(locale, "elicitationExpired", &[]), OutboundExtras::default())]));
        };
        let option_count = question["options"].as_array().map_or(0, Vec::len);
        if question["multiSelect"] != true
            && super::selection::is_index_value(value)
            && value.parse::<usize>().ok() == Some(option_count + 1)
        {
            // 只有菜单显示的额外序号才是跳过；自由文本保留为用户数据。
            return self.advance_elicitation(auth, actor, &pending, pending["answers"].clone()).await.map(Some);
        }
        let values: Vec<String> = if question["multiSelect"] == true {
            value
                .split([',', '\n', '，', '、'])
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(|item| resolve_question_value(&question, item, false))
                .collect()
        } else {
            vec![resolve_question_value(&question, value, false)]
        };
        let mut answers = pending["answers"].clone();
        answers[index.to_string()] = json!(values);
        self.advance_elicitation(auth, actor, &pending, answers).await.map(Some)
    }

    /// `handleStructuredElicitationResponse`（Webhook 的结构化回执）。
    pub(crate) async fn handle_structured_elicitation(self: &Arc<Self>, auth: &Auth, actor: &Actor, response: &Value) -> Result<Vec<Value>> {
        let pending = auth.context["pendingElicitation"].clone();
        if !pending.is_object() || pending["requestId"] != response["requestId"] || !self.owned_by(&pending, actor) {
            return Ok(vec![outbound(
                actor,
                msg(auth.locale.as_deref(), "elicitationExpired", &[]),
                OutboundExtras::default(),
            )]);
        }
        self.submit_elicitation(
            auth,
            actor,
            &pending,
            text(response, "action").unwrap_or("cancel"),
            response.get("content").cloned(),
        )
        .await
    }

    /// `/cancel` 或 `/elicitation submit`（微信）提交当前问答。
    pub(crate) async fn submit_pending_elicitation(self: &Arc<Self>, auth: &Auth, actor: &Actor, action: &str) -> Result<Vec<Value>> {
        let pending = auth.context["pendingElicitation"].clone();
        if !pending.is_object() {
            return Ok(vec![outbound(
                actor,
                msg(auth.locale.as_deref(), "elicitationExpired", &[]),
                OutboundExtras::default(),
            )]);
        }
        let content = (action == "accept").then(|| elicitation_content(&pending, &pending["answers"]));
        self.submit_elicitation(auth, actor, &pending, action, content).await
    }

    /// `permission.respond`（按钮序号）。
    pub(crate) async fn handle_permission_respond(self: &Arc<Self>, auth: &Auth, inbound: &Inbound, value: &str) -> Result<Vec<Value>> {
        let actor = &inbound.actor;
        let locale = auth.locale.as_deref();
        let Some(task) = text(&auth.context, "activeTaskId").map(str::to_owned) else {
            return Ok(vec![outbound(actor, msg(locale, "noActiveTask", &[]), OutboundExtras::default())]);
        };
        let options = auth.context["pendingPermissionOptions"].as_array().cloned().unwrap_or_default();
        let option = super::selection::parse_index(value).and_then(|index| options.get(index).cloned());
        let Some(option) = option else {
            return Ok(vec![outbound(actor, msg(locale, "permissionExpired", &[]), OutboundExtras::default())]);
        };
        if option["handledAt"].is_number() {
            return Ok(vec![outbound(actor, msg(locale, "permissionHandled", &[]), OutboundExtras::default())]);
        }
        let request_id = text(&option, "requestId").unwrap_or_default().to_owned();
        self.resolve_permission(auth, &task, &request_id, text(&option, "optionId").unwrap_or_default())
            .await?;
        let handled_at = now_ms();
        let mut context = auth.context.clone();
        if let Some(items) = context["pendingPermissionOptions"].as_array_mut() {
            for item in items.iter_mut() {
                if item["requestId"] == request_id.as_str() {
                    item["handledAt"] = json!(handled_at);
                }
            }
        }
        self.write_context(&strip_pending(&context)).await?;
        self.start_typing(&auth.bot, actor, &task);
        let id = if option["command"] == "deny" { "permissionDenied" } else { "permissionSubmitted" };
        Ok(vec![outbound(actor, msg(locale, id, &[]), OutboundExtras::default())])
    }

    /// `/approve <requestId> <optionId>`、`/deny <requestId>`。
    pub(crate) async fn handle_approve(
        self: &Arc<Self>,
        auth: &Auth,
        inbound: &Inbound,
        request_id: &str,
        option_id: Option<&str>,
    ) -> Result<Vec<Value>> {
        let actor = &inbound.actor;
        let locale = auth.locale.as_deref();
        let Some(task) = text(&auth.context, "activeTaskId").map(str::to_owned) else {
            return Ok(vec![outbound(actor, msg(locale, "noActiveTask", &[]), OutboundExtras::default())]);
        };
        let options = auth.context["pendingPermissionOptions"].as_array().cloned().unwrap_or_default();
        let resolved_option = match option_id {
            Some(option_id) => {
                let found = options
                    .iter()
                    .any(|option| option["requestId"] == request_id && option["optionId"] == option_id);
                if !found {
                    return Ok(vec![outbound(actor, msg(locale, "permissionHandled", &[]), OutboundExtras::default())]);
                }
                option_id.to_owned()
            }
            None => options
                .iter()
                .find(|option| option["requestId"] == request_id && option["command"] == "deny")
                .and_then(|option| text(option, "optionId").map(str::to_owned))
                .unwrap_or_else(|| "deny".into()),
        };
        self.resolve_permission(auth, &task, request_id, &resolved_option).await?;
        self.start_typing(&auth.bot, actor, &task);
        let id = if option_id.is_some() { "permissionSubmitted" } else { "permissionDenied" };
        Ok(vec![outbound(actor, msg(locale, id, &[]), OutboundExtras::default())])
    }

    async fn resolve_permission(&self, auth: &Auth, task: &str, request_id: &str, option_id: &str) -> Result<()> {
        let target = context_target(&auth.context);
        let tasks = self.tasks_for(&target).map_err(anyhow::Error::msg)?;
        tasks
            .resolve_interaction(
                target,
                task.to_owned(),
                request_id.to_owned(),
                json!({"optionId": option_id}),
            )
            .await
            .map_err(|error| anyhow!(error))
    }
}

/// `getBotPermissionOptionDisplayKind`（V4 选项：optionId、kind、label）。
fn permission_kind(option: &Value) -> &'static str {
    static ALWAYS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\b(always|persistent|permanent|remember)\b|始终|永久|记住|不再询问").expect("valid regex")
    });
    static ALLOW: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\b(allow|approve|accept|yes)\b|允许|同意|批准").expect("valid regex")
    });
    static REJECT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\b(deny|reject|decline|no)\b|拒绝|不允许|否").expect("valid regex")
    });
    let source = format!(
        "{} {} {}",
        text(option, "optionId").unwrap_or_default(),
        text(option, "kind").unwrap_or_default(),
        text(option, "label").or_else(|| text(option, "name")).unwrap_or_default()
    )
    .to_lowercase();
    let always = ALWAYS.is_match(&source);
    if ALLOW.is_match(&source) {
        if always { "allowAlways" } else { "allowOnce" }
    } else if REJECT.is_match(&source) {
        if always { "rejectAlways" } else { "rejectOnce" }
    } else {
        "custom"
    }
}

fn permission_priority(option: &Value) -> u8 {
    match permission_kind(option) {
        "allowOnce" => 0,
        "allowAlways" => 1,
        "rejectOnce" => 2,
        "rejectAlways" => 3,
        _ => 4,
    }
}

/// `formatBotPermissionOptionLabel`。
fn permission_label(option: &Value, kind: &str, locale: Option<&str>) -> String {
    let english = locale == Some("en-US");
    let custom = || text(option, "label").or_else(|| text(option, "name")).unwrap_or_default().to_owned();
    match (kind, english) {
        ("allowOnce", true) => "Allow".into(),
        ("allowAlways", true) => "Always Allow".into(),
        ("rejectOnce", true) => "Deny".into(),
        ("rejectAlways", true) => "Always Deny".into(),
        ("allowOnce", false) => "允许".into(),
        ("allowAlways", false) => "始终允许".into(),
        ("rejectOnce", false) => "拒绝".into(),
        ("rejectAlways", false) => "始终拒绝".into(),
        _ => custom(),
    }
}

/// `formatBotPermissionOptionDescription`。
fn permission_description(option: &Value, kind: &str, scope: &str, locale: Option<&str>) -> Option<String> {
    if kind == "custom" {
        return text(option, "kind").map(str::to_owned);
    }
    let english = locale == Some("en-US");
    Some(
        match (kind, scope, english) {
            ("allowOnce", _, true) => "Allow this time only",
            ("rejectOnce", _, true) => "Reject this time",
            ("allowAlways", "command", true) => "Do not ask again for the same command",
            ("allowAlways", "file", true) => "Do not ask again for the same file operation",
            ("allowAlways", _, true) => "Do not ask again for the same permission request",
            (_, "command", true) => "Always reject the same command",
            (_, "file", true) => "Always reject the same file operation",
            (_, _, true) => "Always reject the same permission request",
            ("allowOnce", _, false) => "仅允许这一次",
            ("rejectOnce", _, false) => "这次先拒绝",
            ("allowAlways", "command", false) => "后续相同命令不再询问",
            ("allowAlways", "file", false) => "后续相同文件操作不再询问",
            ("allowAlways", _, false) => "后续相同权限请求不再询问",
            (_, "command", false) => "后续相同命令也会直接拒绝",
            (_, "file", false) => "后续相同文件操作也会直接拒绝",
            _ => "后续相同权限请求也会直接拒绝",
        }
        .to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elicitation_helpers_match_node() {
        let pending = json!({"taskId": "t", "runId": "r", "requestId": "q", "currentQuestionIndex": 0,
            "questions": [{"question": "Pick", "header": "Pick", "options": [{"value": "a", "label": "A"}], "multiSelect": true},
                          {"question": "Why", "header": "Reason", "options": []}],
            "answers": {"0": ["a"]}});
        let token = selection_token(&pending);
        assert_eq!(token.len(), 12);
        let selection = elicitation_selection(&pending, None);
        assert_eq!(selection["options"][0]["label"], "[x] A");
        assert_eq!(selection["options"][1]["id"], SUBMIT_OPTION_ID);
        assert_eq!(selection["title"], "1/2\nPick\n可多选；再次选择会取消，选择“完成”提交。\n也可以直接回复文本作为自定义答案。");
        let content = elicitation_content(&pending, &json!({"0": ["a"], "1": ["because"]}));
        assert_eq!(
            content,
            json!({"answers": {"Pick": "a", "Why": "because"}, "answer_0": ["a"], "answer_1": "because"})
        );
        assert_eq!(parse_response_value(&format!("{token} 2")), (Some(token.clone()), "2".into()));
        assert_eq!(parse_response_value("hello world"), (None, "hello world".into()));
        assert_eq!(parse_form_values("__form__:%5B%22x%22%2C%22%20y%20%22%5D"), Some(vec!["x".into(), "y".into()]));
        assert_eq!(parse_form_values("plain"), None);
        let question = &pending["questions"][0];
        assert_eq!(resolve_question_value(question, "done", true), SUBMIT_OPTION_ID);
        assert_eq!(resolve_question_value(question, "2", true), SUBMIT_OPTION_ID);
        assert_eq!(resolve_question_value(question, "1", true), "a");
        assert_eq!(resolve_question_value(question, "free", true), "free");
        let questions = normalize_questions(&json!({"prompt": "Continue?", "options": [{"optionId": "y", "label": "Yes"}]}), None);
        assert_eq!(questions, vec![json!({"question": "Continue?", "header": "Continue?", "options": [{"value": "y", "label": "Yes"}]})]);
        assert_eq!(permission_kind(&json!({"optionId": "allow_always", "kind": "allowAlways", "label": "Always allow"})), "allowAlways");
        assert_eq!(permission_kind(&json!({"optionId": "deny", "kind": "deny", "label": "Deny"})), "rejectOnce");
    }
}
