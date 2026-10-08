//! lifecycle 成员的实现（createTask、sendPrompt、resumeTask、closeTask、命令队列、交互回执）。
use std::sync::Arc;

use serde_json::{Map, Value, json};
use zcode_host_kit::{RpcError, log};

use super::agent_calls::{
    self as calls, ack_ok, envelope, is_session_missing, read_session, send_cas, send_command,
};
use super::config_options::{parse_model_picker_value, settings_to_config_options, to_zcode_mode};
use super::context::{invalid, repo_error, target_of};
use super::lifecycle::{TaskLifecycle, now_ms, task_key};
use super::meta;
use crate::agent::WorkspaceTarget;

fn str_param<'a>(params: &'a Value, name: &str) -> Result<&'a str, RpcError> {
    params[name]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| invalid(name))
}

pub(super) fn unsupported(name: &str) -> RpcError {
    RpcError::new(
        "Error",
        format!("ZCode task service adapter does not support IZCodeTaskService.{name} yet."),
    )
    .with_code("ZCODE_AGENT_UNSUPPORTED_LEGACY_TASK_METHOD")
}

/// 显式带 workspacePath 时直接用，否则按登记的 target（stop / compact / goal / 交互回执）。
async fn explicit_or_registered(
    lifecycle: &TaskLifecycle,
    params: &Value,
    task_id: &str,
) -> Result<WorkspaceTarget, RpcError> {
    if params["workspacePath"]
        .as_str()
        .is_some_and(|path| !path.is_empty())
    {
        Ok(target_of(params))
    } else {
        lifecycle.context.task_target(task_id).await
    }
}

impl TaskLifecycle {
    /// `syncTaskIndexSnapshot`：subagent 子会话不入索引。
    pub(super) async fn sync_snapshot(
        &self,
        snapshot: &Value,
        extra: Option<Value>,
    ) -> Result<Value, RpcError> {
        let mut meta = meta::meta_from_snapshot(snapshot);
        if snapshot["session"]["sessionKind"] == "subagent_child" {
            return Ok(meta);
        }
        if let (Some(meta), Some(Value::Object(extra))) = (meta.as_object_mut(), extra) {
            meta.extend(extra);
        }
        let persisted = self
            .context
            .index
            .sync_task_meta(meta, None)
            .await
            .map_err(repo_error)?;
        self.context.remember(&persisted);
        Ok(persisted)
    }

    fn emit_config(&self, target: &WorkspaceTarget, settings: &Value) {
        let mut event = Map::new();
        event.insert("type".into(), json!("workspace_config_options_update"));
        event.insert("workspacePath".into(), json!(target.workspace_path));
        if let Some(identity) = &target.workspace_identity {
            event.insert("workspaceIdentity".into(), json!(identity));
        }
        event.insert("configOptions".into(), settings_to_config_options(settings));
        self.context.emit(target, Value::Object(event));
    }

    /// `resumeSnapshot`：`session/resume`（thoughtLevel、mcpServers）。
    pub(super) async fn resume_snapshot(
        &self,
        target: &WorkspaceTarget,
        task_id: &str,
        thought_level: Option<&str>,
        mcp_servers: Option<&Value>,
    ) -> Result<Value, RpcError> {
        self.context.remember_target(task_id, target);
        let thought_level = thought_level
            .map(str::trim)
            .filter(|level| !level.is_empty());
        calls::resume_session(
            &self.context.runtime,
            target,
            task_id,
            thought_level,
            mcp_servers,
        )
        .await
    }

    pub async fn initialize(&self, params: &Value) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        let result = self.context.runtime.initialize(&target).await;
        let mut out = Map::new();
        out.insert("available".into(), result["available"].clone());
        if let Some(name) = result["protocolName"].as_str() {
            let version = result["protocolVersion"].as_i64().unwrap_or(1);
            out.insert("version".into(), json!(format!("{name}/{version}")));
        }
        Ok(Value::Object(out))
    }

    pub async fn create_task(&self, params: &Value) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        let runtime = &self.context.runtime;
        let requested = match (
            params.get("modelSelection").filter(|v| v.is_object()),
            params["model"].as_str(),
        ) {
            (Some(selection), _) => Some(selection.clone()),
            (None, Some(model)) => {
                let mut selection = parse_model_picker_value(model)
                    .map_err(|message| RpcError::new("Error", message))?;
                if let Some(level) = params["thoughtLevel"].as_str().filter(|l| !l.is_empty()) {
                    selection["options"] = json!({"reasoningLevel": level});
                }
                Some(selection)
            }
            _ => None,
        };
        let reasoning = requested
            .as_ref()
            .and_then(|s| s["options"]["reasoningLevel"].as_str())
            .map(str::to_owned);
        // resolveProductMcpServers：空数组视为未指定（Rust Host 没有 CUA resolver）。
        let mcp_servers = params
            .get("mcpServers")
            .filter(|v| v.as_array().is_some_and(|a| !a.is_empty()))
            .cloned();
        let mut snapshot: Option<Value> = None;
        let draft = params["draftSessionId"]
            .as_str()
            .map(str::trim)
            .filter(|d| !d.is_empty());
        if let (Some(draft), None) = (draft, &mcp_servers) {
            match self
                .reuse_draft(&target, draft, requested.as_ref(), reasoning.as_deref())
                .await
            {
                Ok(reused) => snapshot = Some(reused),
                Err(error) if is_session_missing(&error) => log::warn(format!(
                    "手机 replayable draft session 已失效，降级创建新 task draftSessionId={draft} workspacePath={}",
                    target.workspace_path
                )),
                Err(error) => return Err(error),
            }
        }
        let snapshot = match snapshot {
            Some(snapshot) => snapshot,
            None if params["v4Create"] == true => {
                let mut config = Map::new();
                if let Some(selection) = &requested {
                    config.insert("provider".into(), selection["providerId"].clone());
                    config.insert("model".into(), selection["modelId"].clone());
                }
                if let Some(level) = &reasoning {
                    config.insert("thought".into(), json!(level));
                }
                if let Some(mode) = to_zcode_mode(params["mode"].as_str()) {
                    config.insert("mode".into(), json!(mode));
                }
                let mut payload = Map::new();
                payload.insert("workspaceId".into(), json!(target.key()));
                payload.insert("config".into(), Value::Object(config));
                if let Some(servers) = &mcp_servers {
                    payload.insert("mcpServers".into(), servers.clone());
                }
                // V4 createSession 绕过 legacy 参数构造：工具面参数在信封处同源注入
                // （buildConversationCommandEnvelope；docs/specs/rust-host-integration.md §2.4）。
                payload.extend(runtime.extensions().session_tool_flags(&target).await);
                let ack = send_command(
                    runtime,
                    &target,
                    envelope(
                        "createSession",
                        Value::Object(payload),
                        None,
                        None,
                        None,
                        None,
                    ),
                )
                .await?;
                let ack = ack_ok(
                    "createSession",
                    ack,
                    &format!("workspace={}", target.workspace_path),
                )?;
                let session_id = (ack["result"]["type"] == "createSession")
                    .then(|| ack["result"]["sessionId"].as_str())
                    .flatten()
                    .ok_or_else(|| {
                        RpcError::new(
                            "Error",
                            "v4 createSession accepted without sessionId result",
                        )
                    })?
                    .to_owned();
                read_session(runtime, &target, &session_id).await?
            }
            None => {
                let mut create = Map::new();
                create.insert("workspace".into(), target.workspace_ref());
                if let Some(mode) = to_zcode_mode(params["mode"].as_str()) {
                    create.insert("mode".into(), json!(mode));
                }
                if let Some(selection) = &requested {
                    create.insert("model".into(), selection.clone());
                }
                let automation = params["automationId"].as_str().filter(|id| !id.is_empty());
                if automation.is_some() || params["deferPersistenceUntilFirstPrompt"] == true {
                    // automation / 闲时首发前要先让 V4 admission 持久化 session 主记录。
                    create.insert("persistence".into(), json!("deferred"));
                }
                if let Some(level) = &reasoning {
                    create.insert("thoughtLevel".into(), json!(level));
                }
                if automation.is_some() {
                    create.insert("titleGenerationEnabled".into(), json!(false));
                }
                if let Some(servers) = &mcp_servers {
                    create.insert("mcpServers".into(), servers.clone());
                }
                create.extend(runtime.extensions().session_tool_flags(&target).await);
                calls::request(runtime, &target, "session/create", Value::Object(create)).await?
            }
        };
        let mut extra = Map::new();
        if let Some(automation) = params["automationId"].as_str().filter(|id| !id.is_empty()) {
            extra.insert("cronAutomationId".into(), json!(automation));
        }
        if let Some(off_peak) = params["offPeakTaskId"].as_str().filter(|id| !id.is_empty()) {
            extra.insert("offPeakTaskId".into(), json!(off_peak));
        }
        let mut base = meta::meta_from_snapshot(&snapshot);
        if let Some(base) = base.as_object_mut() {
            base.extend(extra);
        }
        let meta = self
            .context
            .index
            .sync_task_meta(base, None)
            .await
            .map_err(repo_error)?;
        self.context.remember(&meta);
        let task_id = meta["taskId"].as_str().unwrap_or_default().to_owned();
        let meta_target = target_of(&meta);
        self.context
            .index
            .initialize_grouped_task_at_top(meta_target.key(), task_id.clone())
            .await
            .map_err(repo_error)?;
        self.emit_config(&target, &snapshot["settings"]);
        // 远控创建没有本地乐观插入：必须保留 task_created 语义并携带 meta。
        self.context
            .emit_list_changed(&target, Some(&task_id), "task_created", Some(&meta));
        let mut result = meta.as_object().cloned().unwrap_or_default();
        result.insert(
            "initialSlashCommands".into(),
            snapshot
                .get("slashCommands")
                .filter(|v| !v.is_null())
                .cloned()
                .unwrap_or_else(|| json!([])),
        );
        Ok(Value::Object(result))
    }

    async fn reuse_draft(
        &self,
        target: &WorkspaceTarget,
        draft: &str,
        requested: Option<&Value>,
        reasoning: Option<&str>,
    ) -> Result<Value, RpcError> {
        let runtime = &self.context.runtime;
        let mut snapshot = read_session(runtime, target, draft).await?;
        if let Some(selection) = requested {
            let current = &snapshot["settings"]["model"]["current"];
            let same = current["providerId"] == selection["providerId"]
                && current["modelId"] == selection["modelId"]
                && current["options"]["reasoningLevel"] == selection["options"]["reasoningLevel"];
            if !same {
                snapshot = calls::request(
                    runtime,
                    target,
                    "session/setModel",
                    json!({"sessionId": draft, "model": selection}),
                )
                .await?;
            }
        }
        if let Some(level) = reasoning
            && snapshot["settings"]["thoughtLevel"]["current"].as_str() != Some(level)
        {
            snapshot = calls::request(
                runtime,
                target,
                "session/setThoughtLevel",
                json!({"sessionId": draft, "thoughtLevel": level}),
            )
            .await?;
        }
        Ok(snapshot)
    }

    pub async fn send_prompt(&self, params: &Value) -> Result<(), RpcError> {
        let task_id = str_param(params, "taskId")?;
        str_param(params, "traceId")?;
        let mut target = self.context.task_target(task_id).await?;
        if let Some(remote) = params["remoteSessionId"].as_str().filter(|r| !r.is_empty()) {
            target.remote_session_id = Some(remote.to_owned());
        }
        self.send_prompt_to_agent(&target, task_id, params).await
    }

    pub async fn resume_task(&self, params: &Value) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        let task_id = str_param(params, "taskId")?;
        let explicit_model = params["model"]
            .as_str()
            .map(str::trim)
            .filter(|m| !m.is_empty());
        let explicit_level = params["thoughtLevel"]
            .as_str()
            .map(str::trim)
            .filter(|l| !l.is_empty());
        let hint_level = if explicit_model.is_some() && explicit_level.is_some() {
            None
        } else {
            // resolveTaskIndexResumeHints：读取失败只记日志，按无 hint 恢复。
            match self
                .context
                .index
                .get_task_meta(&target.key(), task_id)
                .await
            {
                Ok(meta) => {
                    meta.and_then(|meta| meta["thoughtLevel"].as_str().map(|l| l.trim().to_owned()))
                }
                Err(error) => {
                    log::warn(format!("读取 task index resume hint 失败: {error:#}"));
                    None
                }
            }
        };
        let thought_level = explicit_level.map(str::to_owned).or(hint_level);
        let mcp_servers = params
            .get("mcpServers")
            .filter(|v| v.as_array().is_some_and(|a| !a.is_empty()));
        let snapshot = self
            .resume_snapshot(&target, task_id, thought_level.as_deref(), mcp_servers)
            .await?;
        self.emit_config(&target, &snapshot["settings"]);
        let mut meta = self.sync_snapshot(&snapshot, None).await?;
        let mut extra = Map::new();
        if let Some(automation) = params["automationId"].as_str().filter(|id| !id.is_empty()) {
            extra.insert("cronAutomationId".into(), json!(automation));
        }
        if let Some(off_peak) = params["offPeakTaskId"].as_str().filter(|id| !id.is_empty()) {
            extra.insert("offPeakTaskId".into(), json!(off_peak));
        }
        if !extra.is_empty() {
            if let Some(object) = meta.as_object_mut() {
                object.extend(extra);
            }
            meta = self
                .context
                .index
                .sync_task_meta(meta, None)
                .await
                .map_err(repo_error)?;
            self.context.remember(&meta);
        }
        Ok(meta)
    }

    pub async fn close_task(&self, params: &Value) -> Result<(), RpcError> {
        let task_id = str_param(params, "taskId")?;
        let target = self.context.task_target(task_id).await?;
        calls::request(
            &self.context.runtime,
            &target,
            "session/close",
            json!({"sessionId": task_id}),
        )
        .await?;
        let repo = self.context.tasks.repo();
        let marked = repo
            .mark_deleted(target.key(), task_id.to_owned())
            .await
            .map_err(repo_error)?;
        if marked.is_none() {
            // 行缺失：先从 CLI 回源写入，再标记删除（updateIndexedTaskState 的兜底）。
            let snapshot = self.resume_snapshot(&target, task_id, None, None).await?;
            self.sync_snapshot(&snapshot, None).await?;
            repo.mark_deleted(target.key(), task_id.to_owned())
                .await
                .map_err(repo_error)?;
        }
        self.context
            .emit_list_changed(&target, Some(task_id), "task_meta_changed", None);
        Ok(())
    }

    pub async fn stop_generation(&self, params: &Value) -> Result<(), RpcError> {
        let task_id = str_param(params, "taskId")?;
        let target = explicit_or_registered(self, params, task_id).await?;
        let ack = send_command(
            &self.context.runtime,
            &target,
            envelope("stop", json!({}), Some(task_id), None, None, None),
        )
        .await?;
        ack_ok("stop", ack, &format!("session={task_id}")).map(|_| ())
    }

    pub async fn respond_interaction(
        &self,
        params: &Value,
        elicitation: bool,
    ) -> Result<bool, RpcError> {
        let task_id = str_param(params, "taskId")?;
        let request_id = str_param(params, "requestId")?;
        let target = explicit_or_registered(self, params, task_id).await?;
        let answer = if elicitation {
            let mut answer = Map::new();
            answer.insert("action".into(), params["action"].clone());
            // JS 真值判断：null、空串、false、0 都不带 content。
            if let Some(content) = params.get("content").filter(|c| js_truthy(c)) {
                answer.insert("content".into(), content.clone());
            }
            Value::Object(answer)
        } else {
            json!({"optionId": params["optionId"]})
        };
        let payload = json!({"interactionId": request_id, "answer": answer});
        let ack = send_command(
            &self.context.runtime,
            &target,
            envelope(
                "resolveInteraction",
                payload,
                Some(task_id),
                None,
                None,
                None,
            ),
        )
        .await?;
        let kind = if elicitation {
            "elicitation"
        } else {
            "permission"
        };
        ack_ok("resolveInteraction", ack, &format!("{kind} {request_id}"))?;
        if elicitation && params["clientMode"] == "web-remote-replayable" {
            // 手机远控应答后，其他 observer 需要显式的响应事件来清理同一 requestId 的弹窗。
            let mut event = Map::new();
            event.insert("type".into(), json!("elicitation_response"));
            event.insert("taskId".into(), json!(task_id));
            event.insert("traceId".into(), json!(crate::agent::service::uuid_v4()));
            event.insert("requestId".into(), json!(request_id));
            event.insert("action".into(), params["action"].clone());
            if let Some(content) = params.get("content").filter(|c| js_truthy(c)) {
                event.insert("content".into(), content.clone());
            }
            self.stream.emit(Arc::new((
                task_key(&target, task_id),
                task_id.to_owned(),
                Value::Object(event),
            )));
        }
        Ok(true)
    }

    pub async fn compact_session(&self, params: &Value) -> Result<Value, RpcError> {
        let task_id = str_param(params, "taskId")?;
        let target = explicit_or_registered(self, params, task_id).await?;
        let key = task_key(&target, task_id);
        if let Some(input) = params["inputId"].as_str().filter(|i| !i.is_empty()) {
            self.set_active_input(key.clone(), input.to_owned());
        }
        let request = json!({
            "sessionId": task_id,
            "inputId": params.get("inputId").cloned().unwrap_or(Value::Null),
            "instructions": params.get("instructions").cloned().unwrap_or(Value::Null),
            "expectedRevision": params.get("expectedRevision").cloned().unwrap_or(Value::Null),
        });
        let request = strip_nulls(request);
        match calls::request(&self.context.runtime, &target, "session/compact", request).await {
            Ok(result) => {
                if result["compact"]["state"] == "accepted" {
                    return Ok(result);
                }
                let meta = self.sync_snapshot(&result["snapshot"], None).await;
                self.clear_active_input(&key);
                let meta = meta?;
                self.context.emit_list_changed(
                    &target,
                    Some(task_id),
                    "task_status_changed",
                    Some(&meta),
                );
                Ok(result)
            }
            Err(error) => {
                self.clear_active_input(&key);
                Err(error)
            }
        }
    }

    pub async fn goal_session(&self, params: &Value) -> Result<Value, RpcError> {
        let task_id = str_param(params, "taskId")?;
        let target = explicit_or_registered(self, params, task_id).await?;
        let key = task_key(&target, task_id);
        let action = params["action"].as_str().unwrap_or_default();
        let may_continue = matches!(action, "set" | "replace" | "resume");
        if let Some(input) = params["inputId"]
            .as_str()
            .filter(|i| !i.is_empty() && may_continue)
        {
            self.set_active_input(key.clone(), input.to_owned());
        }
        let request = strip_nulls(json!({
            "sessionId": task_id,
            "inputId": params.get("inputId").cloned().unwrap_or(Value::Null),
            "action": params.get("action").cloned().unwrap_or(Value::Null),
            "objective": params.get("objective").cloned().unwrap_or(Value::Null),
            "expectedRevision": params.get("expectedRevision").cloned().unwrap_or(Value::Null),
        }));
        let result =
            calls::request(&self.context.runtime, &target, "session/goal", request).await?;
        let meta = self.sync_snapshot(&result["snapshot"], None).await?;
        self.context
            .emit_list_changed(&target, Some(task_id), "task_status_changed", Some(&meta));
        if !may_continue || result["snapshot"]["session"]["status"] != "running" {
            self.clear_active_input(&key);
        }
        Ok(result)
    }

    /// `switchCollaborationModeViaProtocol`：auto 走旧 `session/setMode`，其余走 V4 CAS。
    pub(super) async fn switch_mode(
        &self,
        target: &WorkspaceTarget,
        task_id: &str,
        mode: &str,
    ) -> Result<(), RpcError> {
        if mode == "auto" {
            calls::request(
                &self.context.runtime,
                target,
                "session/setMode",
                json!({"sessionId": task_id, "mode": mode}),
            )
            .await?;
            return Ok(());
        }
        send_cas(
            &self.context.runtime,
            target,
            "switchCollaborationMode",
            json!({"mode": mode}),
            task_id,
            &format!("session={task_id} mode={mode}"),
        )
        .await
        .map(|_| ())
    }

    /// 配置写入后的收口：`resumeSnapshot` → 写索引 → configOptions。
    pub(super) async fn settle_config(
        &self,
        target: &WorkspaceTarget,
        task_id: &str,
    ) -> Result<Value, RpcError> {
        let snapshot = self.resume_snapshot(target, task_id, None, None).await?;
        self.sync_snapshot(&snapshot, None).await?;
        Ok(settings_to_config_options(&snapshot["settings"]))
    }

    pub(super) fn touch_command(command: &mut Value) {
        command["updatedAt"] = json!(now_ms());
    }
}

/// JSON 没有 undefined：值为 null 的顶层键按 TS 的 undefined 省略。
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        _ => true,
    }
}

pub(super) fn strip_nulls(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            Value::Object(map.into_iter().filter(|(_, v)| !v.is_null()).collect())
        }
        other => other,
    }
}

pub(super) fn read_session_settings_model(snapshot: &Value) -> Option<Value> {
    snapshot["settings"]["model"]["current"]
        .as_object()
        .map(|m| Value::Object(m.clone()))
}

pub(super) async fn read_current_model(
    lifecycle: &TaskLifecycle,
    target: &WorkspaceTarget,
    task_id: &str,
) -> Result<Option<Value>, RpcError> {
    let snapshot = read_session(&lifecycle.context.runtime, target, task_id).await?;
    Ok(read_session_settings_model(&snapshot))
}
