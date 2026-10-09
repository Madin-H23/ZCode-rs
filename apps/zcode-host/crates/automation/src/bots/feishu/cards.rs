//! 飞书 / Lark 卡片（providers/feishuProvider.ts 的卡片部分）：带选项按钮的交互卡片、问答卡片、
//! 流式回复卡片与按元素预算分段。所有卡片都是 Card JSON 2.0（按钮直接放在 body.elements，经
//! `behaviors.callback` 回传命令）。
use serde_json::{Map, Value, json};

use super::card_markdown;
use crate::bots::model::message as msg;
use crate::bots::providers::selection_command;
use crate::bots::util::{encode_uri_component, text};

/// 飞书 Card JSON 2.0 最多 200 个组件；留 20 个给状态行与服务端计数差异。
pub const STREAMING_CARD_TAGGED_ELEMENT_BUDGET: usize = 180;
pub const ELICITATION_FORM_FIELD_NAME: &str = "answer";
pub const ELICITATION_FORM_VALUE_PREFIX: &str = "__form__:";
pub const ELICITATION_CUSTOM_OPTION_ID: &str = "__custom__";

fn locale(message: &Value) -> Option<&str> {
    text(message, "locale")
}

fn plain_text(content: &str) -> Value {
    json!({"tag": "plain_text", "content": content})
}

fn markdown(content: &str) -> Value {
    json!({"tag": "markdown", "content": card_markdown(content)})
}

fn card(elements: Vec<Value>) -> Value {
    json!({"schema": "2.0", "config": {"wide_screen_mode": true}, "body": {"elements": elements}})
}

/// `buildFeishuButtonElement`。
pub fn button(label: &str, kind: &str, command: &str, original_text: &str) -> Value {
    json!({
        "tag": "button",
        "text": plain_text(label),
        "type": kind,
        "behaviors": [{"type": "callback", "value": {"command": command, "zcodeCardText": original_text}}],
    })
}

/// `buildFeishuInteractiveCardPayload`：正文 + 选项按钮 + 取消按钮。
pub fn interactive(message: &Value) -> Value {
    let body = text(message, "text").unwrap_or_default();
    let mut elements = vec![markdown(body)];
    let selection = &message["selection"];
    if selection.is_object() {
        for (index, option) in selection["options"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let label = text(option, "label")
                .filter(|label| !label.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| (index + 1).to_string());
            let command = selection_command(selection, text(option, "id").unwrap_or_default());
            elements.push(button(&label, "primary", &command, body));
        }
        if selection["showCancel"] != false {
            let cancel = text(selection, "cancelLabel")
                .map(str::to_owned)
                .unwrap_or_else(|| msg(locale(message), "selectionCancelOption", &[]));
            // 飞书走结构化选项卡，不复用微信纯文本菜单的 0 取消语义。
            elements.push(button(&cancel, "default", "/cancel", body));
        }
    }
    card(elements)
}

/// 出站消息对应的卡片：问答消息用问答卡，其余用交互卡片。
pub fn for_message(message: &Value) -> Value {
    if message["elicitation"].is_object() {
        elicitation(message)
    } else {
        interactive(message)
    }
}

/// `buildFeishuElicitationFormCommand`。
pub fn elicitation_form_command(token: &str, value: Option<&Value>) -> String {
    match value {
        None => format!("/elicitation {token} {ELICITATION_FORM_VALUE_PREFIX}"),
        Some(value) => format!(
            "/elicitation {token} {ELICITATION_FORM_VALUE_PREFIX}{}",
            encode_uri_component(&value.to_string())
        ),
    }
}

fn answer_values(message: &Value, index: usize) -> Vec<String> {
    message["elicitation"]["answers"][index.to_string()]
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn answer_label(message: &Value, index: usize) -> String {
    let question = &message["elicitation"]["questions"][index];
    let values = answer_values(message, index);
    if !question.is_object() || values.is_empty() {
        return String::new();
    }
    values
        .iter()
        .map(|value| {
            question["options"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|option| text(option, "value") == Some(value.as_str()))
                .and_then(|option| text(option, "label"))
                .unwrap_or(value)
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn plan_content(message: &Value) -> Option<String> {
    let schema = &message["elicitation"]["schema"];
    if schema["interaction"] != "plan_approval" {
        return None;
    }
    text(schema, "plan")
        .map(str::trim)
        .filter(|plan| !plan.is_empty())
        .map(str::to_owned)
}

fn option_command(message: &Value, option_id: &str) -> String {
    match text(&message["selection"], "token") {
        Some(token) => selection_command(
            &json!({"action": "elicitation.respond", "token": token}),
            option_id,
        ),
        None => option_id.to_owned(),
    }
}

fn custom_expanded(message: &Value, index: usize) -> bool {
    message["elicitation"]["expandedCustomAnswerQuestionIndexes"]
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item.as_u64() == Some(index as u64)))
}

fn custom_values(message: &Value, question: &Value, index: usize) -> Vec<String> {
    let options: Vec<&str> = question["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|option| text(option, "value"))
        .collect();
    answer_values(message, index)
        .into_iter()
        .filter(|value| !options.contains(&value.as_str()))
        .collect()
}

fn marker(multi: bool, selected: bool) -> &'static str {
    match (multi, selected) {
        (true, true) => "☑",
        (true, false) => "☐",
        (false, true) => "●",
        (false, false) => "○",
    }
}

fn elicitation_form(message: &Value, question: &Value, index: usize) -> Option<Value> {
    let token = text(&message["selection"], "token")?;
    let options_len = question["options"].as_array().map_or(0, Vec::len);
    let show_input = options_len == 0 || custom_expanded(message, index);
    let show_submit = question["multiSelect"] == true || show_input;
    if !show_submit {
        return None;
    }
    let mut elements = Vec::new();
    if show_input {
        elements.push(json!({
            "tag": "input",
            "name": ELICITATION_FORM_FIELD_NAME,
            "required": options_len == 0,
            "width": "fill",
            "input_type": "multiline_text",
            "rows": 2,
            "auto_resize": true,
            "max_rows": 5,
            "placeholder": plain_text(&msg(locale(message), "elicitationCustomPlaceholder", &[])),
            "default_value": custom_values(message, question, index).join(", "),
        }));
    }
    elements.push(json!({
        "tag": "button",
        "text": plain_text(&msg(locale(message), "elicitationSubmitOption", &[])),
        "type": "primary",
        "form_action_type": "submit",
        "name": "submit",
        "behaviors": [{"type": "callback", "value": {
            "command": elicitation_form_command(token, None),
            "zcodeCardText": text(message, "text").unwrap_or_default(),
        }}],
    }));
    Some(json!({
        "tag": "form",
        "name": format!("elicitation_form_{index}"),
        "direction": "vertical",
        "vertical_spacing": "8px",
        "elements": elements,
    }))
}

fn answered_elements(message: &Value) -> Vec<Value> {
    let elicitation = &message["elicitation"];
    let questions = elicitation["questions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let total = questions.len();
    let completed = matches!(text(elicitation, "status"), Some("completed" | "cancelled"));
    let current = elicitation["currentQuestionIndex"].as_u64().unwrap_or(0) as usize;
    let mut elements = Vec::new();
    for (index, question) in questions.iter().enumerate() {
        // 当前题的多选值只是未提交的草稿，进行中只列出此前已提交的题目。
        if !completed && index >= current {
            continue;
        }
        let answer = answer_label(message, index);
        if answer.is_empty() {
            continue;
        }
        elements.push(markdown(&format!(
            "#### {}/{total} {}",
            index + 1,
            text(question, "question").unwrap_or_default()
        )));
        elements.push(markdown(&answer));
    }
    elements
}

/// `buildFeishuElicitationCardPayload`。
pub fn elicitation(message: &Value) -> Value {
    let elicitation = &message["elicitation"];
    if !elicitation.is_object() {
        return interactive(message);
    }
    let locale = locale(message);
    let body_text = text(message, "text").unwrap_or_default();
    let current_index = elicitation["currentQuestionIndex"].as_u64().unwrap_or(0) as usize;
    let questions = elicitation["questions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let current = questions.get(current_index).or_else(|| questions.first());
    let completed = matches!(text(elicitation, "status"), Some("completed" | "cancelled"));
    let answers = answered_elements(message);
    let plan = plan_content(message);
    let mut elements = Vec::new();
    if completed && let Some(plan) = &plan {
        // 只读终态保留完整计划，用户才能在聊天历史中回看批准或取消的内容。
        elements.push(markdown(plan));
        if !answers.is_empty() {
            elements.push(json!({"tag": "hr"}));
        }
    }
    let has_answers = !answers.is_empty();
    elements.extend(answers);
    if !completed && let Some(question) = current {
        if has_answers {
            elements.push(json!({"tag": "hr"}));
        }
        if plan.is_none() {
            elements.push(markdown(&format!(
                "#### {}",
                msg(locale, "elicitationQuestionTitle", &[])
            )));
        }
        let content = match &plan {
            Some(plan) => plan.clone(),
            None => {
                let header = text(question, "header").filter(|header| {
                    !header.is_empty() && Some(*header) != text(question, "question")
                });
                [
                    header.map(|header| format!("**{header}**")),
                    text(question, "question")
                        .filter(|q| !q.is_empty())
                        .map(str::to_owned),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("\n\n")
            }
        };
        elements.push(markdown(&content));
        if plan.is_some() {
            elements.push(json!({"tag": "hr"}));
            elements.push(markdown(&format!(
                "**{}**",
                msg(locale, "planApprovalTitle", &[])
            )));
        }
        let multi = question["multiSelect"] == true;
        let selected = answer_values(message, current_index);
        for option in question["options"].as_array().into_iter().flatten() {
            let value = text(option, "value").unwrap_or_default();
            let label = if plan.is_some() {
                msg(locale, "planApprovalApprove", &[])
            } else {
                text(option, "label").unwrap_or_default().to_owned()
            };
            let is_selected = selected.iter().any(|item| item == value);
            elements.push(button(
                &format!("{} {label}", marker(multi, is_selected)),
                if is_selected { "primary" } else { "default" },
                &option_command(message, value),
                body_text,
            ));
        }
        let expanded = custom_expanded(message, current_index);
        let custom_selected =
            expanded || !custom_values(message, question, current_index).is_empty();
        elements.push(button(
            &format!(
                "{} {}",
                marker(multi, custom_selected),
                msg(locale, "elicitationCustomOption", &[])
            ),
            if custom_selected {
                "primary"
            } else {
                "default"
            },
            &option_command(message, ELICITATION_CUSTOM_OPTION_ID),
            body_text,
        ));
        if let Some(form) = elicitation_form(message, question, current_index) {
            elements.push(form);
        }
        let options_len = question["options"].as_array().map_or(0, Vec::len);
        let show_cancel = multi || options_len == 0 || expanded;
        let selection = &message["selection"];
        if selection.is_object() && show_cancel && selection["showCancel"] != false {
            let cancel = text(selection, "cancelLabel")
                .map(str::to_owned)
                .unwrap_or_else(|| msg(locale, "selectionCancelOption", &[]));
            let command = if expanded && options_len > 0 {
                option_command(message, ELICITATION_CUSTOM_OPTION_ID)
            } else {
                "/cancel".to_owned()
            };
            elements.push(button(&cancel, "default", &command, body_text));
        }
    } else if text(elicitation, "status") == Some("cancelled") {
        elements.push(markdown(&msg(locale, "elicitationCancelledCard", &[])));
    }
    card(elements)
}

// ── 流式回复卡片 ──

#[derive(Debug, Clone, PartialEq)]
pub enum StreamBlock {
    Message(String),
    Tools {
        summaries: Vec<String>,
        title: Option<String>,
        expanded: Option<bool>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamStatus {
    Running,
    Sealed,
    Completed,
    Error,
}

/// `BotStreamingReplyCardState`（providerUserId 由调用方单独传）。
#[derive(Debug, Clone, PartialEq)]
pub struct StreamState {
    pub locale: Option<String>,
    pub blocks: Vec<StreamBlock>,
    pub status: StreamStatus,
}

fn status_line(state: &StreamState) -> String {
    let id = match state.status {
        StreamStatus::Completed => "streamingStatusCompleted",
        StreamStatus::Error => "streamingStatusFailed",
        _ => "streamingStatusRunning",
    };
    msg(state.locale.as_deref(), id, &[])
}

fn tool_panel(summaries: &[String], expanded: bool, title: &str) -> Option<Value> {
    let summaries: Vec<&str> = summaries
        .iter()
        .map(|summary| summary.trim())
        .filter(|summary| !summary.is_empty())
        .collect();
    if summaries.is_empty() {
        return None;
    }
    Some(json!({
        "tag": "collapsible_panel",
        "expanded": expanded,
        "background_color": "grey-50",
        "border": {"color": "grey", "corner_radius": "8px"},
        "padding": "8px 8px 8px 8px",
        "header": {
            "title": {"tag": "plain_text", "content": format!("🛠️ {title} ({})", summaries.len())},
            "vertical_align": "center",
            "padding": "8px 8px 8px 8px",
            "icon": {"tag": "standard_icon", "token": "down-small-ccm_outlined", "color": "grey", "size": "16px 16px"},
            "icon_position": "right",
            "icon_expanded_angle": -180,
        },
        "elements": [{"tag": "markdown", "content": card_markdown(&summaries.join("\n"))}],
    }))
}

/// `buildFeishuStreamingCardPayload`。
pub fn streaming(state: &StreamState) -> Value {
    let mut elements = Vec::new();
    for block in &state.blocks {
        match block {
            StreamBlock::Message(body) => {
                let body = body.trim();
                if !body.is_empty() {
                    elements.push(markdown(body));
                }
            }
            StreamBlock::Tools {
                summaries,
                title,
                expanded,
            } => {
                let title = title
                    .as_deref()
                    .map(str::trim)
                    .filter(|title| !title.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| msg(state.locale.as_deref(), "streamingToolSummaries", &[]));
                let expanded = expanded.unwrap_or(state.status == StreamStatus::Running);
                if let Some(panel) = tool_panel(summaries, expanded, &title) {
                    elements.push(panel);
                }
            }
        }
    }
    if elements.is_empty() {
        elements.push(json!({"tag": "markdown", "content": " "}));
    }
    if state.status != StreamStatus::Sealed {
        elements.push(markdown(&format!("_{}_", status_line(state))));
    }
    card(elements)
}

fn count_tagged(value: &Value) -> usize {
    match value {
        Value::Array(items) => items.iter().map(count_tagged).sum(),
        Value::Object(object) => {
            usize::from(object.get("tag").is_some_and(Value::is_string))
                + object.values().map(count_tagged).sum::<usize>()
        }
        _ => 0,
    }
}

/// `countFeishuCardTaggedElements`。
pub fn tagged_elements(state: &StreamState) -> usize {
    count_tagged(&streaming(state))
}

/// `splitFeishuStreamingCardStates`：超过元素预算时分段，前面的段封存（sealed）。
pub fn split_streaming(state: &StreamState) -> Vec<StreamState> {
    let mut groups: Vec<Vec<StreamBlock>> = Vec::new();
    let mut current: Vec<StreamBlock> = Vec::new();
    for block in &state.blocks {
        let mut candidate = current.clone();
        candidate.push(block.clone());
        // 始终为状态行预留预算，使 running/completed 切换不会改变既有分段边界。
        let probe = StreamState {
            locale: state.locale.clone(),
            blocks: candidate.clone(),
            status: StreamStatus::Running,
        };
        if !current.is_empty() && tagged_elements(&probe) > STREAMING_CARD_TAGGED_ELEMENT_BUDGET {
            groups.push(std::mem::take(&mut current));
            current = vec![block.clone()];
        } else {
            current = candidate;
        }
    }
    groups.push(current);
    let last = groups.len() - 1;
    groups
        .into_iter()
        .enumerate()
        .map(|(index, blocks)| StreamState {
            locale: state.locale.clone(),
            blocks,
            status: if index == last {
                state.status
            } else {
                StreamStatus::Sealed
            },
        })
        .collect()
}

/// 卡片回调里用户填的表单值（测试与解析共用）。
pub fn form_value(answer: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert(ELICITATION_FORM_FIELD_NAME.into(), answer.clone());
    out
}
