//! Bot 配置/状态的数据规则（packages/shared/src/bots.ts、bots/config.ts、botConfigHelpers.ts、
//! workspaceHelpers.ts、storageMigration.ts）。
//!
//! 配置与状态以 JSON 原样保存（`Value`），这里提供与 zod schema 等价的解析（strict 对象拒绝未知键，
//! 非 strict 对象丢弃未知键，输出按 schema 键序重建）以及 Node 侧的归一化函数。
use serde_json::{Map, Value, json};

use crate::tasks_index::automation_repo::parse_model_selection;

pub const ALL_BOT_WORKSPACES: &str = "*";
pub const BOT_BIND_CODE_TTL_MS: i64 = 30_000;
/// `ZCODE_AGENT_PROVIDER`。
pub const ZCODE_AGENT_PROVIDER: &str = "glm";
pub const PROVIDERS: [&str; 7] = [
    "telegram", "webhook", "feishu", "lark", "weixin", "discord", "wecom",
];
const REPLY_GRANULARITIES: [&str; 4] = [
    "assistant_changes",
    "assistant_toolcalls_changes",
    "summary_changes",
    "streaming_card",
];
/// `DEFAULT_BOT_COMMANDS` 的键序。
pub(crate) const DEFAULT_COMMANDS: [&str; 7] = [
    "status",
    "new",
    "workspace",
    "model",
    "mode",
    "thoughtLevel",
    "reply",
];

pub fn is_feishu(provider: &str) -> bool {
    matches!(provider, "feishu" | "lark")
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// `getWorkspaceKey`。
pub fn workspace_key(path: &str, identity: Option<&str>) -> String {
    identity
        .map(str::trim)
        .filter(|identity| !identity.is_empty())
        .unwrap_or(path)
        .to_owned()
}

/// `getWorkspaceLabel`。
pub fn workspace_label(path: &str) -> String {
    path.split(['/', '\\'])
        .rfind(|part| !part.is_empty())
        .unwrap_or(path)
        .to_owned()
}

/// `createWorkspaceRef`。
pub fn workspace_ref(path: &str, identity: Option<&str>) -> Value {
    let mut object = Map::new();
    object.insert("id".into(), json!(workspace_key(path, identity)));
    object.insert("label".into(), json!(workspace_label(path)));
    object.insert("workspacePath".into(), json!(path));
    if let Some(identity) = identity {
        object.insert("workspaceIdentity".into(), json!(identity));
    }
    Value::Object(object)
}

fn all_workspaces_allowed(allowed: &[String]) -> bool {
    allowed.is_empty() || allowed.iter().any(|item| item == ALL_BOT_WORKSPACES)
}

/// `normalizeAllowedWorkspaces`。
pub fn normalize_allowed_workspaces(allowed: &[String]) -> Vec<String> {
    let ids: Vec<String> = allowed
        .iter()
        .map(|item| item.trim().to_owned())
        .filter(|item| !item.is_empty())
        .collect();
    if all_workspaces_allowed(&ids) {
        return vec![ALL_BOT_WORKSPACES.to_owned()];
    }
    let mut out: Vec<String> = Vec::new();
    for id in ids {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

pub fn string_list(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// `isWorkspaceAllowed`。
pub fn is_workspace_allowed(id: &str, allowed: &[String]) -> bool {
    all_workspaces_allowed(allowed) || allowed.iter().any(|item| item == id)
}

/// `filterAllowedWorkspaces`。
pub fn filter_allowed_workspaces(workspaces: &[Value], allowed: &[String]) -> Vec<Value> {
    workspaces
        .iter()
        .filter(|workspace| {
            all_workspaces_allowed(allowed)
                || allowed
                    .iter()
                    .any(|id| Some(id.as_str()) == str_field(workspace, "id"))
        })
        .cloned()
        .collect()
}

/// `resolveCanonicalWorkspaceId`。
fn canonical_workspace_id(value: &str, workspaces: &[Value]) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let same_path: Vec<&Value> = workspaces
        .iter()
        .filter(|workspace| str_field(workspace, "workspacePath") == Some(trimmed))
        .collect();
    if let Some(exact) = workspaces
        .iter()
        .find(|workspace| str_field(workspace, "id") == Some(trimmed))
    {
        let identity_candidates: Vec<&&Value> = same_path
            .iter()
            .filter(|workspace| {
                str_field(workspace, "workspaceIdentity").is_some_and(|id| !id.is_empty())
            })
            .collect();
        if str_field(exact, "workspaceIdentity").is_none_or(str::is_empty)
            && identity_candidates.len() == 1
        {
            return str_field(identity_candidates[0], "id").map(str::to_owned);
        }
        return str_field(exact, "id").map(str::to_owned);
    }
    Some(if same_path.len() == 1 {
        str_field(same_path[0], "id").unwrap_or(trimmed).to_owned()
    } else {
        trimmed.to_owned()
    })
}

/// `normalizeConfiguredAllowedWorkspaces`。
pub fn normalize_configured_allowed_workspaces(
    allowed: &[String],
    workspaces: &[Value],
) -> Vec<String> {
    let normalized = normalize_allowed_workspaces(allowed);
    if all_workspaces_allowed(&normalized) {
        return vec![ALL_BOT_WORKSPACES.to_owned()];
    }
    let mut out: Vec<String> = Vec::new();
    for id in normalized {
        let id = canonical_workspace_id(&id, workspaces).unwrap_or(id);
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// `normalizeBotReplyGranularity`：飞书只支持流式卡片，其余通道不支持。
pub fn normalize_reply_mode(provider: &str, reply_mode: Option<&str>) -> &'static str {
    let candidate = reply_mode.unwrap_or("assistant_changes");
    let supported: Vec<&'static str> = if is_feishu(provider) {
        vec!["streaming_card"]
    } else {
        REPLY_GRANULARITIES
            .iter()
            .copied()
            .filter(|mode| *mode != "streaming_card")
            .collect()
    };
    supported
        .iter()
        .copied()
        .find(|mode| *mode == candidate)
        .unwrap_or(supported[0])
}

/// `normalizeBotCommandPolicy`：补默认值，丢弃旧 `cli`。
pub fn normalize_command_policy(commands: &Value) -> Value {
    let mut out = Map::new();
    for key in DEFAULT_COMMANDS {
        let value = commands.get(key).and_then(Value::as_bool).unwrap_or(true);
        out.insert(key.into(), json!(value));
    }
    for key in ["sandboxMode", "approvalPolicy"] {
        if let Some(value) = commands.get(key).filter(|value| !value.is_null()) {
            out.insert(key.into(), value.clone());
        }
    }
    Value::Object(out)
}

fn truthy_string(value: Option<&Value>) -> Option<Value> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(|text| json!(text))
}

/// `normalizeBotCurrentOptions`。
pub fn normalize_current_options(options: &Value) -> Value {
    let mut out = Map::new();
    if let Some(selection) = options
        .get("modelSelection")
        .and_then(parse_model_selection)
    {
        out.insert("modelSelection".into(), selection);
    }
    for key in ["mode", "sandboxMode", "approvalPolicy"] {
        if let Some(value) = truthy_string(options.get(key)) {
            out.insert(key.into(), value);
        }
    }
    Value::Object(out)
}

/// `normalizeBotDraftOptions`（config.ts）。
pub fn normalize_draft_options(provider: &str, options: &Value) -> Value {
    let mut out = Map::new();
    out.insert("provider".into(), json!(provider));
    if let Some(selection) = options
        .get("modelSelection")
        .and_then(parse_model_selection)
    {
        out.insert("modelSelection".into(), selection);
    }
    if let Some(mode) = truthy_string(options.get("mode")) {
        out.insert("mode".into(), mode);
    }
    Value::Object(out)
}

/// `normalizeBotConfig`：输入键序保留，归一化字段原位覆盖；微信通道去掉 webhookUrl。
pub fn normalize_bot(bot: &Value) -> Value {
    let mut out = bot.as_object().cloned().unwrap_or_default();
    let provider = str_field(bot, "provider").unwrap_or_default().to_owned();
    out.insert(
        "allowedWorkspaces".into(),
        json!(normalize_allowed_workspaces(&string_list(
            &bot["allowedWorkspaces"]
        ))),
    );
    out.insert(
        "allowedCommands".into(),
        normalize_command_policy(&bot["allowedCommands"]),
    );
    out.insert(
        "currentOptions".into(),
        normalize_current_options(&bot["currentOptions"]),
    );
    out.insert(
        "replyMode".into(),
        json!(normalize_reply_mode(&provider, str_field(bot, "replyMode"))),
    );
    if provider == "weixin" {
        out.remove("webhookUrl");
    }
    Value::Object(out)
}

/// 删除值为 null 的可选键（TS 里写成 `undefined` 的字段在 JSON 中不存在）。
pub fn strip_nulls(value: &mut Value, keys: &[&str]) {
    if let Some(object) = value.as_object_mut() {
        for key in keys {
            if object.get(*key).is_some_and(Value::is_null) {
                object.remove(*key);
            }
        }
    }
}

// ── schema（zod 等价解析） ──

type Parsed = Result<Value, String>;

fn issue(path: &str, message: &str) -> String {
    format!("{path}: {message}")
}

/// strict 对象：拒绝未知键，按 `keys` 顺序取值；`field` 返回 None 表示缺省（可选键）。
fn strict_object(
    value: &Value,
    path: &str,
    keys: &[&str],
    mut field: impl FnMut(&str, Option<&Value>) -> Result<Option<Value>, String>,
) -> Parsed {
    let object = value
        .as_object()
        .ok_or_else(|| issue(path, "expected object"))?;
    if let Some(unknown) = object.keys().find(|key| !keys.contains(&key.as_str())) {
        return Err(issue(path, &format!("unrecognized key \"{unknown}\"")));
    }
    let mut out = Map::new();
    for key in keys {
        if let Some(parsed) = field(key, object.get(*key))? {
            out.insert((*key).into(), parsed);
        }
    }
    Ok(Value::Object(out))
}

/// 非 strict 对象：未知键丢弃。
fn loose_object(
    value: &Value,
    path: &str,
    keys: &[&str],
    mut field: impl FnMut(&str, Option<&Value>) -> Result<Option<Value>, String>,
) -> Parsed {
    let object = value
        .as_object()
        .ok_or_else(|| issue(path, "expected object"))?;
    let mut out = Map::new();
    for key in keys {
        if let Some(parsed) = field(key, object.get(*key))? {
            out.insert((*key).into(), parsed);
        }
    }
    Ok(Value::Object(out))
}

fn req<'a>(value: Option<&'a Value>, path: &str, key: &str) -> Result<&'a Value, String> {
    value
        .filter(|value| !value.is_null())
        .ok_or_else(|| issue(&format!("{path}.{key}"), "required"))
}

fn string(value: &Value, path: &str, min: usize) -> Result<Value, String> {
    match value.as_str() {
        Some(text) if text.chars().count() >= min => Ok(json!(text)),
        Some(_) => Err(issue(path, "too short")),
        None => Err(issue(path, "expected string")),
    }
}

fn opt_string(value: Option<&Value>, path: &str, min: usize) -> Result<Option<Value>, String> {
    match value {
        None => Ok(None),
        Some(value) => string(value, path, min).map(Some),
    }
}

fn boolean(value: &Value, path: &str) -> Result<Value, String> {
    value
        .as_bool()
        .map(Value::Bool)
        .ok_or_else(|| issue(path, "expected boolean"))
}

fn number(value: &Value, path: &str) -> Result<Value, String> {
    if value.is_number() {
        Ok(value.clone())
    } else {
        Err(issue(path, "expected number"))
    }
}

fn int_min0(value: &Value, path: &str) -> Result<Value, String> {
    match value.as_f64() {
        Some(number) if number.fract() == 0.0 && number >= 0.0 => Ok(value.clone()),
        _ => Err(issue(path, "expected non-negative integer")),
    }
}

fn one_of(value: &Value, path: &str, options: &[&str]) -> Result<Value, String> {
    match value.as_str() {
        Some(text) if options.contains(&text) => Ok(json!(text)),
        _ => Err(issue(path, &format!("expected one of {options:?}"))),
    }
}

fn model_selection(value: &Value, path: &str) -> Result<Value, String> {
    parse_model_selection(value).ok_or_else(|| issue(path, "invalid model selection"))
}

fn is_url(text: &str) -> bool {
    url::Url::parse(text).is_ok()
}

fn allowed_commands(value: &Value, path: &str) -> Parsed {
    let keys = [
        "status",
        "new",
        "workspace",
        "model",
        "mode",
        "thoughtLevel",
        "sandboxMode",
        "approvalPolicy",
        "cli",
        "reply",
    ];
    strict_object(value, path, &keys, |key, field| {
        let field_path = format!("{path}.{key}");
        match key {
            "mode" | "sandboxMode" | "approvalPolicy" | "cli" => {
                field.map(|value| boolean(value, &field_path)).transpose()
            }
            _ => boolean(req(field, path, key)?, &field_path).map(Some),
        }
    })
}

fn current_options(value: &Value, path: &str) -> Parsed {
    let keys = [
        "modelSelection",
        "mode",
        "sandboxMode",
        "approvalPolicy",
        "cli",
    ];
    strict_object(value, path, &keys, |key, field| {
        let field_path = format!("{path}.{key}");
        match key {
            "modelSelection" => field
                .map(|value| model_selection(value, &field_path))
                .transpose(),
            "cli" => field
                .map(|value| one_of(value, &field_path, &[ZCODE_AGENT_PROVIDER]))
                .transpose(),
            _ => opt_string(field, &field_path, 1),
        }
    })
}

/// `botConfigSchema`。
pub fn parse_bot(value: &Value, path: &str) -> Parsed {
    let keys = [
        "id",
        "name",
        "provider",
        "enabled",
        "credentialRef",
        "webhookSecretRef",
        "webhookUrl",
        "webhookAuthHeaderName",
        "feishuAppId",
        "providerUserId",
        "displayName",
        "allowedWorkspaces",
        "allowedCommands",
        "currentOptions",
        "replyMode",
    ];
    strict_object(value, path, &keys, |key, field| {
        let field_path = format!("{path}.{key}");
        Ok(match key {
            "id" => Some(string(req(field, path, key)?, &field_path, 1)?),
            "name" => Some(string(req(field, path, key)?, &field_path, 0)?),
            "provider" => Some(one_of(req(field, path, key)?, &field_path, &PROVIDERS)?),
            "enabled" => Some(boolean(req(field, path, key)?, &field_path)?),
            "webhookUrl" => match field {
                None => None,
                Some(value) => {
                    let text = string(value, &field_path, 0)?;
                    if !text.as_str().is_some_and(is_url) {
                        return Err(issue(&field_path, "invalid url"));
                    }
                    Some(text)
                }
            },
            "displayName" => opt_string(field, &field_path, 0)?,
            "allowedWorkspaces" => {
                let items = req(field, path, key)?
                    .as_array()
                    .ok_or_else(|| issue(&field_path, "expected array"))?;
                let mut out = Vec::new();
                for (index, item) in items.iter().enumerate() {
                    out.push(string(item, &format!("{field_path}.{index}"), 1)?);
                }
                Some(Value::Array(out))
            }
            "allowedCommands" => Some(allowed_commands(req(field, path, key)?, &field_path)?),
            "currentOptions" => Some(current_options(req(field, path, key)?, &field_path)?),
            "replyMode" => Some(one_of(
                req(field, path, key)?,
                &field_path,
                &REPLY_GRANULARITIES,
            )?),
            _ => opt_string(field, &field_path, 1)?,
        })
    })
}

/// `botsConfigFileSchema`。
pub fn parse_config_file(value: &Value) -> Parsed {
    strict_object(value, "config", &["version", "bots"], |key, field| {
        Ok(Some(match key {
            "version" => match req(field, "config", key)?.as_i64() {
                Some(3) => json!(3),
                _ => return Err(issue("config.version", "expected 3")),
            },
            _ => {
                let bots = req(field, "config", key)?
                    .as_array()
                    .ok_or_else(|| issue("config.bots", "expected array"))?;
                let mut out = Vec::new();
                for (index, bot) in bots.iter().enumerate() {
                    out.push(parse_bot(bot, &format!("config.bots.{index}"))?);
                }
                Value::Array(out)
            }
        }))
    })
}

fn draft_options(value: &Value, path: &str) -> Parsed {
    strict_object(
        value,
        path,
        &["provider", "modelSelection", "mode"],
        |key, field| {
            let field_path = format!("{path}.{key}");
            match key {
                "provider" => {
                    one_of(req(field, path, key)?, &field_path, &[ZCODE_AGENT_PROVIDER]).map(Some)
                }
                "modelSelection" => field
                    .map(|value| model_selection(value, &field_path))
                    .transpose(),
                _ => opt_string(field, &field_path, 1),
            }
        },
    )
}

/// `zcodePermissionResponseSchema`：键集合 strict；`modifiedInput` / `permissionUpdates` 原样保留。
fn permission_response(value: &Value, path: &str) -> Parsed {
    strict_object(
        value,
        path,
        &["decision", "reason", "modifiedInput", "permissionUpdates"],
        |key, field| {
            let field_path = format!("{path}.{key}");
            match key {
                "decision" => one_of(
                    req(field, path, key)?,
                    &field_path,
                    &["allow", "deny", "escalate", "modify"],
                )
                .map(Some),
                "reason" => opt_string(field, &field_path, 0),
                "permissionUpdates" => match field {
                    None => Ok(None),
                    Some(Value::Array(items)) => Ok(Some(Value::Array(items.clone()))),
                    Some(_) => Err(issue(&field_path, "expected array")),
                },
                _ => Ok(field.cloned()),
            }
        },
    )
}

fn pending_permission(value: &Value, path: &str) -> Parsed {
    loose_object(
        value,
        path,
        &[
            "requestId",
            "optionId",
            "command",
            "label",
            "response",
            "handledAt",
        ],
        |key, field| {
            let field_path = format!("{path}.{key}");
            Ok(match key {
                "command" => Some(one_of(
                    req(field, path, key)?,
                    &field_path,
                    &["approve", "deny"],
                )?),
                "response" => Some(permission_response(req(field, path, key)?, &field_path)?),
                "handledAt" => field.map(|value| number(value, &field_path)).transpose()?,
                _ => Some(string(req(field, path, key)?, &field_path, 1)?),
            })
        },
    )
}

fn elicitation_question(value: &Value, path: &str) -> Parsed {
    strict_object(
        value,
        path,
        &["question", "header", "options", "multiSelect"],
        |key, field| {
            let field_path = format!("{path}.{key}");
            Ok(match key {
                "options" => {
                    let items = req(field, path, key)?
                        .as_array()
                        .ok_or_else(|| issue(&field_path, "expected array"))?;
                    let mut out = Vec::new();
                    for (index, item) in items.iter().enumerate() {
                        let item_path = format!("{field_path}.{index}");
                        out.push(strict_object(
                            item,
                            &item_path,
                            &["value", "label", "description"],
                            |key, field| {
                                let path = format!("{item_path}.{key}");
                                if key == "description" {
                                    opt_string(field, &path, 0)
                                } else {
                                    string(req(field, &item_path, key)?, &path, 0).map(Some)
                                }
                            },
                        )?);
                    }
                    Some(Value::Array(out))
                }
                "multiSelect" => field.map(|value| boolean(value, &field_path)).transpose()?,
                _ => Some(string(req(field, path, key)?, &field_path, 0)?),
            })
        },
    )
}

/// `botPendingElicitationSchema`（strict）；`origin` 只校验为对象。
fn pending_elicitation(value: &Value, path: &str) -> Parsed {
    let keys = [
        "taskId",
        "requestId",
        "runId",
        "origin",
        "actorKey",
        "currentQuestionIndex",
        "questions",
        "answers",
        "renderContext",
        "expandedCustomAnswerQuestionIndexes",
        "handledAt",
    ];
    strict_object(value, path, &keys, |key, field| {
        let field_path = format!("{path}.{key}");
        Ok(match key {
            "taskId" | "requestId" | "runId" => {
                Some(string(req(field, path, key)?, &field_path, 1)?)
            }
            "actorKey" => opt_string(field, &field_path, 1)?,
            "origin" => match field {
                None => None,
                Some(value) if value.is_object() => Some(value.clone()),
                Some(_) => return Err(issue(&field_path, "expected object")),
            },
            "currentQuestionIndex" => Some(int_min0(req(field, path, key)?, &field_path)?),
            "questions" => {
                let items = req(field, path, key)?
                    .as_array()
                    .ok_or_else(|| issue(&field_path, "expected array"))?;
                let mut out = Vec::new();
                for (index, item) in items.iter().enumerate() {
                    out.push(elicitation_question(
                        item,
                        &format!("{field_path}.{index}"),
                    )?);
                }
                Some(Value::Array(out))
            }
            "answers" => {
                let object = req(field, path, key)?
                    .as_object()
                    .ok_or_else(|| issue(&field_path, "expected record"))?;
                for (answer_key, answers) in object {
                    let ok = answers
                        .as_array()
                        .is_some_and(|items| items.iter().all(Value::is_string));
                    if !ok {
                        return Err(issue(
                            &format!("{field_path}.{answer_key}"),
                            "expected string[]",
                        ));
                    }
                }
                Some(Value::Object(object.clone()))
            }
            "renderContext" => match field {
                None => None,
                Some(value) => Some(strict_object(
                    value,
                    &field_path,
                    &["kind", "plan"],
                    |key, field| {
                        let path = format!("{field_path}.{key}");
                        if key == "kind" {
                            one_of(req(field, &field_path, key)?, &path, &["plan_approval"])
                                .map(Some)
                        } else {
                            string(req(field, &field_path, key)?, &path, 1).map(Some)
                        }
                    },
                )?),
            },
            "expandedCustomAnswerQuestionIndexes" => match field {
                None => None,
                Some(value) => {
                    let items = value
                        .as_array()
                        .ok_or_else(|| issue(&field_path, "expected array"))?;
                    for item in items {
                        int_min0(item, &field_path)?;
                    }
                    Some(value.clone())
                }
            },
            _ => field.map(|value| number(value, &field_path)).transpose()?,
        })
    })
}

fn bot_state(value: &Value, path: &str) -> Parsed {
    let keys = [
        "botId",
        "workspacePath",
        "workspaceIdentity",
        "workspaceId",
        "mode",
        "activeTaskId",
        "draftOptions",
        "pendingPermissionOptions",
        "pendingElicitation",
        "telegramOffset",
        "weixinGetUpdatesBuf",
        "weixinActivatedAt",
        "updatedAt",
    ];
    loose_object(value, path, &keys, |key, field| {
        let field_path = format!("{path}.{key}");
        Ok(match key {
            "botId" | "workspacePath" => Some(string(req(field, path, key)?, &field_path, 1)?),
            "workspaceIdentity" | "workspaceId" => opt_string(field, &field_path, 1)?,
            "mode" => Some(one_of(
                req(field, path, key)?,
                &field_path,
                &["draft", "task"],
            )?),
            "activeTaskId" => match field {
                Some(Value::Null) => Some(Value::Null),
                Some(value) => Some(string(value, &field_path, 1)?),
                None => return Err(issue(&field_path, "required")),
            },
            "draftOptions" => field
                .map(|value| draft_options(value, &field_path))
                .transpose()?,
            "pendingPermissionOptions" => match field {
                None => None,
                Some(value) => {
                    let items = value
                        .as_array()
                        .ok_or_else(|| issue(&field_path, "expected array"))?;
                    let mut out = Vec::new();
                    for (index, item) in items.iter().enumerate() {
                        out.push(pending_permission(item, &format!("{field_path}.{index}"))?);
                    }
                    Some(Value::Array(out))
                }
            },
            "pendingElicitation" => field
                .map(|value| pending_elicitation(value, &field_path))
                .transpose()?,
            "weixinGetUpdatesBuf" => opt_string(field, &field_path, 0)?,
            "updatedAt" => Some(number(req(field, path, key)?, &field_path)?),
            _ => field.map(|value| number(value, &field_path)).transpose()?,
        })
    })
}

/// `botsStateFileSchema`。
pub fn parse_state_file(value: &Value) -> Parsed {
    strict_object(value, "state", &["version", "bots"], |key, field| {
        Ok(Some(match key {
            "version" => match req(field, "state", key)?.as_i64() {
                Some(3) => json!(3),
                _ => return Err(issue("state.version", "expected 3")),
            },
            _ => {
                let bots = req(field, "state", key)?
                    .as_object()
                    .ok_or_else(|| issue("state.bots", "expected record"))?;
                let mut out = Map::new();
                for (id, state) in bots {
                    out.insert(id.clone(), bot_state(state, &format!("state.bots.{id}"))?);
                }
                Value::Object(out)
            }
        }))
    })
}

// ── 旧版导入（storageMigration.ts） ──

const LEGACY_PROVIDERS: [(&str, &str); 6] = [
    ("builtin:bigmodel", "bigmodel-api"),
    ("builtin:zai", "zai-api"),
    ("builtin:bigmodel-start-plan", "account:bigmodel-start-plan"),
    ("builtin:zai-start-plan", "account:zai-start-plan"),
    (
        "builtin:bigmodel-coding-plan",
        "account:bigmodel-individual-coding-plan",
    ),
    (
        "builtin:zai-coding-plan",
        "account:zai-individual-coding-plan",
    ),
];

const OFFICIAL_GLM_IDS: [&str; 22] = [
    "GLM-5.3",
    "GLM-5.3-Flash",
    "GLM-5V-Turbo",
    "GLM-5.2",
    "GLM-5.1",
    "GLM-5.1-Highspeed",
    "GLM-5",
    "GLM-5-Turbo",
    "GLM-4.7",
    "GLM-4.7-FlashX",
    "GLM-4.7-Flash",
    "GLM-4.6",
    "GLM-4.5-Air",
    "GLM-4.5",
    "GLM-4.6V",
    "GLM-4.6V-Flash",
    "GLM-4.6V-FlashX",
    "GLM-4.1V-Thinking-FlashX",
    "GLM-4.1V-Thinking-Flash",
    "GLM-4-FlashX-250414",
    "GLM-4-Flash-250414",
    "GLM-4V-Flash",
];

/// `migrateLegacyModelProviderId`。
fn migrate_provider_id(provider_id: &str) -> Option<String> {
    if let Some((_, next)) = LEGACY_PROVIDERS
        .iter()
        .find(|(legacy, _)| *legacy == provider_id)
    {
        return Some((*next).to_owned());
    }
    (!provider_id.starts_with("builtin:")).then(|| provider_id.to_owned())
}

/// `migrateLegacyOfficialGlmModelId`。
fn migrate_model_id(provider_id: &str, model_id: &str) -> String {
    let official = matches!(
        provider_id,
        "builtin:zai"
            | "builtin:bigmodel"
            | "builtin:zai-start-plan"
            | "builtin:bigmodel-start-plan"
            | "builtin:zai-coding-plan"
            | "builtin:bigmodel-coding-plan"
            | "account:zai-start-plan"
            | "account:bigmodel-start-plan"
            | "account:zai-individual-coding-plan"
            | "account:bigmodel-individual-coding-plan"
            | "account:zai-team-coding-plan"
            | "account:bigmodel-team-coding-plan"
    );
    if !official {
        return model_id.to_owned();
    }
    let lower = model_id.to_lowercase();
    OFFICIAL_GLM_IDS
        .iter()
        .find(|id| id.to_lowercase() == lower)
        .map(|id| (*id).to_owned())
        .unwrap_or_else(|| model_id.to_owned())
}

/// `safeDecodeUriComponent`：JS `decodeURIComponent`，非法序列时原样返回。
fn decode_component(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            out.push(bytes[index]);
            index += 1;
            continue;
        }
        let Some(byte) = text
            .get(index + 1..index + 3)
            .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        else {
            return text.to_owned();
        };
        out.push(byte);
        index += 3;
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_owned())
}

/// `decodeCustomModelValue`。
pub(crate) fn decode_custom_model(value: &str) -> Option<(String, Option<String>)> {
    let body = value.strip_prefix("custom:")?;
    let Some(separator) = body.find(':') else {
        return Some((decode_component(body), None));
    };
    let parts: Vec<&str> = body.split(':').collect();
    if parts.len() >= 3 && parts[0] == "builtin" {
        return Some((
            format!("{}:{}", parts[0], parts[1]),
            Some(decode_component(&parts[2..].join(":"))),
        ));
    }
    Some((
        decode_component(&body[..separator]),
        Some(decode_component(&body[separator + 1..])),
    ))
}

/// `migrateSelection`。
fn migrate_selection(options: &Map<String, Value>) -> Option<Value> {
    if options.contains_key("modelSelection") {
        let selection = parse_model_selection(&options["modelSelection"])?;
        let provider = selection["providerId"].as_str().unwrap_or_default();
        if !provider.starts_with("builtin:") {
            return Some(selection);
        }
        let next = migrate_provider_id(provider)?;
        let mut out = selection.clone();
        out["modelId"] = json!(migrate_model_id(
            provider,
            selection["modelId"].as_str().unwrap_or_default()
        ));
        out["providerId"] = json!(next);
        return Some(out);
    }
    let value = options
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    let custom = decode_custom_model(value);
    let separator = value.find('/').filter(|index| *index > 0);
    let old_provider = custom
        .as_ref()
        .map(|(provider, _)| provider.clone())
        .or_else(|| separator.map(|index| value[..index].to_owned()))?;
    let model_id = match &custom {
        Some((_, model)) => model.clone(),
        None => separator.map(|index| value[index + 1..].to_owned()),
    }?;
    if old_provider.is_empty() || model_id.is_empty() {
        return None;
    }
    let provider_id = migrate_provider_id(&old_provider)?;
    let mut out = Map::new();
    out.insert("providerId".into(), json!(provider_id));
    out.insert(
        "modelId".into(),
        json!(migrate_model_id(&old_provider, &model_id)),
    );
    let level = options
        .get("thoughtLevel")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if !level.is_empty() {
        out.insert("options".into(), json!({"reasoningLevel": level}));
    }
    Some(Value::Object(out))
}

/// `importLegacyBotConfig`。
pub fn import_legacy_config(value: &Value) -> Value {
    let Some(object) = value.as_object() else {
        return value.clone();
    };
    let Some(bots) = object.get("bots").and_then(Value::as_array) else {
        return value.clone();
    };
    let mut out = object.clone();
    out.insert("version".into(), json!(3));
    let bots: Vec<Value> = bots
        .iter()
        .map(|bot| {
            let Some(bot_object) = bot.as_object() else {
                return bot.clone();
            };
            let options = bot_object
                .get("currentOptions")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let mut merged = options.clone();
            match migrate_selection(&options) {
                Some(selection) => merged.insert("modelSelection".into(), selection),
                None => merged.remove("modelSelection"),
            };
            let mut next = bot_object.clone();
            next.insert(
                "currentOptions".into(),
                normalize_current_options(&Value::Object(merged)),
            );
            Value::Object(next)
        })
        .collect();
    out.insert("bots".into(), Value::Array(bots));
    Value::Object(out)
}

/// `importLegacyBotState`。
pub fn import_legacy_state(value: &Value) -> Value {
    let Some(object) = value.as_object() else {
        return value.clone();
    };
    let Some(bots) = object.get("bots").and_then(Value::as_object) else {
        return value.clone();
    };
    let mut out = object.clone();
    out.insert("version".into(), json!(3));
    let bots: Map<String, Value> = bots
        .iter()
        .map(|(id, state)| {
            let Some(options) = state.get("draftOptions").and_then(Value::as_object) else {
                return (id.clone(), state.clone());
            };
            let mut draft = Map::new();
            if let Some(selection) = migrate_selection(options) {
                draft.insert("modelSelection".into(), selection);
            }
            if let Some(mode) = options.get("mode").filter(|mode| mode.is_string()) {
                draft.insert("mode".into(), mode.clone());
            }
            let mut next = state.as_object().cloned().unwrap_or_default();
            next.insert(
                "draftOptions".into(),
                normalize_draft_options(ZCODE_AGENT_PROVIDER, &Value::Object(draft)),
            );
            (id.clone(), Value::Object(next))
        })
        .collect();
    out.insert("bots".into(), Value::Object(bots));
    Value::Object(out)
}

// ── 命令解析（commandParser.ts） ──

/// `parseBotCommand` 的结果，形状与 TS 的 `BotCommand` 联合类型相同。
pub fn parse_command(text: &str) -> Value {
    let trimmed = text.trim();
    let Some(body) = trimmed.strip_prefix('/') else {
        return if trimmed == "0" {
            json!({"type": "selection.cancel"})
        } else {
            json!({"type": "message", "text": text})
        };
    };
    let (name, rest) = match body.find(char::is_whitespace) {
        None => (body.to_lowercase(), String::new()),
        Some(index) => (
            body[..index].to_lowercase(),
            body[index..]
                .chars()
                .skip(1)
                .collect::<String>()
                .trim()
                .to_owned(),
        ),
    };
    let unknown = || json!({"type": "unknown", "name": name, "raw": text});
    let with_rest = |kind: &str, list: &str| {
        if rest.is_empty() {
            json!({"type": list})
        } else {
            json!({"type": kind, "value": rest})
        }
    };
    match name.as_str() {
        "bind" if !rest.is_empty() => json!({"type": "bind", "code": rest}),
        "help" | "帮助" => json!({"type": "help"}),
        "cancel" | "取消" => json!({"type": "selection.cancel"}),
        "status" | "状态" => json!({"type": "status"}),
        "new" | "clear" | "新建" => json!({"type": "new"}),
        "reconnect" | "重连" => json!({"type": "reconnect"}),
        "workspace" | "project" | "项目" => with_rest("workspace.set", "workspace.list"),
        "model" | "模型" => {
            if rest.is_empty() {
                json!({"type": "model.list"})
            } else if let Some(value) = rest.strip_prefix("provider ") {
                json!({"type": "model.provider.set", "value": value.trim()})
            } else if let Some(value) = rest.strip_prefix("model ") {
                json!({"type": "model.set", "value": value.trim()})
            } else {
                json!({"type": "model.set", "value": rest})
            }
        }
        "mode" | "模式" => with_rest("mode.set", "mode.list"),
        "thoughtlevel" | "thought_level" | "thought-level" | "think" | "思考" => {
            with_rest("thoughtLevel.set", "thoughtLevel.list")
        }
        "task" => with_rest("task.set", "task.list"),
        "reply" | "回复" => with_rest("reply.set", "reply.list"),
        "stop" | "停止" => json!({"type": "stop"}),
        "permission" if !rest.is_empty() => {
            json!({"type": "permission.respond", "value": rest})
        }
        "elicitation" | "answer" | "回答" if !rest.is_empty() => {
            if ["submit", "done", "完成", "提交"].contains(&rest.to_lowercase().as_str()) {
                json!({"type": "elicitation.submit"})
            } else {
                json!({"type": "elicitation.respond", "value": rest})
            }
        }
        "approve" => {
            let mut parts = rest.split_whitespace();
            match (parts.next(), parts.next()) {
                (Some(request), Some(option)) => {
                    json!({"type": "approve", "requestId": request, "optionId": option})
                }
                _ => unknown(),
            }
        }
        "deny" if !rest.is_empty() => json!({"type": "deny", "requestId": rest}),
        _ => unknown(),
    }
}

// ── 文案（messages.ts，由 bots-fixtures.ts 导出） ──

static MESSAGES: std::sync::LazyLock<Value> = std::sync::LazyLock::new(|| {
    serde_json::from_str(include_str!("fixtures/messages.json")).unwrap_or(Value::Null)
});

/// `formatBotMessage`：只有 en-US 用英文，其余 locale 用中文。
pub fn message(locale: Option<&str>, id: &str, values: &[(&str, &str)]) -> String {
    let locale = if locale == Some("en-US") {
        "en-US"
    } else {
        "zh-CN"
    };
    let mut text = MESSAGES[locale][id].as_str().unwrap_or(id).to_owned();
    for (key, value) in values {
        text = text.replace(&format!("{{{key}}}"), value);
    }
    text
}
