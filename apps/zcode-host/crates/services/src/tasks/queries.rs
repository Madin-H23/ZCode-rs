//! zcode-task 的查询、配置、文件与事件成员，以及 lifecycle 成员的 channel 注册。
use std::sync::Arc;

use serde_json::{Map, Value, json};
use zcode_host_kit::RpcError;

use super::agent_calls::{self as calls, send_cas};
use super::config_options::{
    MODE_CONFIG_ID, MODEL_CONFIG_ID, THOUGHT_LEVEL_CONFIG_ID, parse_model_picker_value,
    settings_to_config_options, to_zcode_mode,
};
use super::context::{invalid, repo_error, target_of};
use super::lifecycle::{TaskLifecycle, now_ms, task_key};
use super::lifecycle_members::read_current_model;
use super::trajectory;
use crate::agent::WorkspaceTarget;

fn task_id(params: &Value) -> Result<String, RpcError> {
    params["taskId"]
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| invalid("taskId"))
}

fn owner_error(message: &str, code: &str) -> RpcError {
    RpcError::new("Error", message).with_code(code)
}

/// 本地日期（`YYYY-MM-DD`），与 Node `new Date().getFullYear()` 等本地时间一致。
fn local_date() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    #[cfg(unix)]
    #[allow(unsafe_code)]
    {
        let time: libc::time_t = now as libc::time_t;
        // SAFETY: localtime_r 只写入我们提供的 tm 结构。
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        if !unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
            return format!(
                "{:04}-{:02}-{:02}",
                tm.tm_year + 1900,
                tm.tm_mon + 1,
                tm.tm_mday
            );
        }
    }
    // 非 unix 或取本地时间失败：按 UTC（见 rust-host-tasks.md 第 5 节）。
    let (y, m, d) = civil(now.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (
        if m <= 2 {
            yoe + era * 400 + 1
        } else {
            yoe + era * 400
        },
        m,
        d,
    )
}

impl TaskLifecycle {
    fn assert_owner(&self, key: &str, params: &Value) -> Result<(), RpcError> {
        let Some(owner) = params["ownerRunId"].as_str().filter(|id| !id.is_empty()) else {
            return Ok(());
        };
        match self.active_input(key) {
            None => Err(owner_error("No active task owner.", "NO_ACTIVE_TASK_OWNER")),
            Some(active) if active != owner => Err(owner_error(
                "Stale task owner command.",
                "STALE_TASK_OWNER_COMMAND",
            )),
            Some(_) => Ok(()),
        }
    }

    pub async fn enqueue_command(self: &Arc<Self>, params: &Value) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        let id = task_id(params)?;
        let key = task_key(&target, &id);
        self.assert_owner(&key, params)?;
        let now = now_ms();
        let mut command = Map::new();
        command.insert("commandId".into(), params["commandId"].clone());
        command.insert("taskId".into(), json!(id));
        command.insert("traceId".into(), params["traceId"].clone());
        for (key_name, value) in [
            ("queryId", params.get("queryId")),
            ("workspacePath", Some(&json!(target.workspace_path))),
            ("workspaceIdentity", params.get("workspaceIdentity")),
        ] {
            if let Some(value) = value.filter(|v| !v.is_null()) {
                command.insert(key_name.into(), value.clone());
            }
        }
        command.insert("workspaceKey".into(), json!(target.key()));
        command.insert("status".into(), json!("accepted"));
        command.insert("createdAt".into(), json!(now));
        command.insert("updatedAt".into(), json!(now));
        for field in ["clientId", "clientLabel"] {
            if let Some(value) = params.get(field).filter(|v| !v.is_null()) {
                command.insert(field.into(), value.clone());
            }
        }
        command.insert("type".into(), json!("send_prompt"));
        command.insert("content".into(), params["content"].clone());
        for field in ["attachments", "automationId"] {
            if let Some(value) = params.get(field).filter(|v| !v.is_null()) {
                command.insert(field.into(), value.clone());
            }
        }
        let command = Value::Object(command);
        self.commands
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(key)
            .or_default()
            .push(command.clone());
        self.schedule_drain(target, id, "enqueue");
        Ok(json!({"accepted": true, "command": command}))
    }

    pub async fn promote_command(self: &Arc<Self>, params: &Value) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        let id = task_id(params)?;
        let key = task_key(&target, &id);
        self.assert_owner(&key, params)?;
        let command_id = params["commandId"].clone();
        let promoted = {
            let mut commands = self.commands.lock().unwrap_or_else(|p| p.into_inner());
            let queue = commands.entry(key.clone()).or_default();
            let Some(index) = queue.iter().position(|c| c["commandId"] == command_id) else {
                return Err(owner_error(
                    "Task command not found.",
                    "OWNER_COMMAND_FAILED",
                ));
            };
            if queue[index]["status"] == "running" {
                return Ok(json!({"accepted": true, "command": queue[index].clone()}));
            }
            let mut command = queue.remove(index);
            command["status"] = json!("accepted");
            Self::touch_command(&mut command);
            queue.insert(0, command.clone());
            command
        };
        self.schedule_drain(target, id, "promote");
        Ok(json!({"accepted": true, "command": promoted}))
    }

    pub async fn cancel_command(&self, params: &Value) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        let id = task_id(params)?;
        let key = task_key(&target, &id);
        self.assert_owner(&key, params)?;
        let command_id = params["commandId"].clone();
        let removed = {
            let mut commands = self.commands.lock().unwrap_or_else(|p| p.into_inner());
            let Some(queue) = commands.get_mut(&key) else {
                return Ok(
                    json!({"canceled": true, "commandId": command_id, "reason": "not_found"}),
                );
            };
            let Some(index) = queue.iter().position(|c| c["commandId"] == command_id) else {
                return Ok(
                    json!({"canceled": true, "commandId": command_id, "reason": "not_found"}),
                );
            };
            if queue[index]["status"] == "running" {
                return Ok(
                    json!({"canceled": false, "commandId": command_id, "reason": "already_running", "status": "running"}),
                );
            }
            let removed = queue.remove(index);
            if queue.is_empty() {
                commands.remove(&key);
            }
            removed
        };
        let mut event = Map::new();
        event.insert("type".into(), json!("task_snapshot_updated"));
        event.insert("workspacePath".into(), json!(target.workspace_path));
        if let Some(identity) = &target.workspace_identity {
            event.insert("workspaceIdentity".into(), json!(identity));
        }
        event.insert("workspaceKey".into(), json!(target.key()));
        event.insert("taskId".into(), json!(id));
        event.insert("traceId".into(), removed["traceId"].clone());
        event.insert("reason".into(), json!("task_status_changed"));
        self.stream.emit(Arc::new((key, id, Value::Object(event))));
        Ok(json!({"canceled": true, "commandId": command_id, "status": removed["status"]}))
    }

    pub async fn set_model(&self, params: &Value) -> Result<Value, RpcError> {
        let id = task_id(params)?;
        let target = self.context.task_target(&id).await?;
        calls::request(
            &self.context.runtime,
            &target,
            "session/setModel",
            json!({"sessionId": id, "model": params["modelSelection"]}),
        )
        .await?;
        self.settle_config(&target, &id).await
    }

    pub async fn set_config_option(&self, params: &Value) -> Result<Value, RpcError> {
        let id = task_id(params)?;
        let target = self.context.task_target(&id).await?;
        let value = params["value"].as_str().unwrap_or_default();
        match params["configId"].as_str() {
            Some(MODEL_CONFIG_ID) => {
                let selection =
                    parse_model_picker_value(value).map_err(|m| RpcError::new("Error", m))?;
                return self
                    .set_model(&json!({"taskId": id, "modelSelection": selection}))
                    .await;
            }
            Some(THOUGHT_LEVEL_CONFIG_ID) => {
                let Some(model) = read_current_model(self, &target, &id).await? else {
                    return Err(RpcError::new("Error", "请先选择模型，再设置思考档位"));
                };
                let payload = json!({"provider": model["providerId"], "model": model["modelId"], "thought": value});
                send_cas(
                    &self.context.runtime,
                    &target,
                    "switchModelConfig",
                    payload,
                    &id,
                    &format!("session={id} thought={value}"),
                )
                .await?;
            }
            Some(MODE_CONFIG_ID) => {
                self.switch_mode(&target, &id, to_zcode_mode(Some(value)).unwrap_or("build"))
                    .await?;
            }
            _ => {}
        }
        self.settle_config(&target, &id).await
    }

    pub async fn set_mode(&self, params: &Value) -> Result<(), RpcError> {
        let id = task_id(params)?;
        let target = self.context.task_target(&id).await?;
        self.switch_mode(
            &target,
            &id,
            to_zcode_mode(params["mode"].as_str()).unwrap_or("build"),
        )
        .await?;
        let snapshot = self.resume_snapshot(&target, &id, None, None).await?;
        self.sync_snapshot(&snapshot, None).await.map(|_| ())
    }

    pub async fn set_automation_session_config(&self, params: &Value) -> Result<Value, RpcError> {
        let id = task_id(params)?;
        let target = self.context.task_target(&id).await?;
        let model = &params["modelSelection"];
        let thought = params["thoughtLevel"].as_str().map(str::trim).unwrap_or("");
        let payload =
            json!({"provider": model["providerId"], "model": model["modelId"], "thought": thought});
        let runtime = &self.context.runtime;
        let label = format!(
            "{}/{}",
            model["providerId"].as_str().unwrap_or_default(),
            model["modelId"].as_str().unwrap_or_default()
        );
        send_cas(
            runtime,
            &target,
            "switchModelConfig",
            payload.clone(),
            &id,
            &format!("automation session={id} model={label} thought={thought}"),
        )
        .await?;
        if !thought.is_empty() {
            // 跨模型切换会先采用目标模型默认 Think；同模型再发一次显式收敛到 automation 的档位。
            send_cas(
                runtime,
                &target,
                "switchModelConfig",
                payload,
                &id,
                &format!("automation session={id} thought={thought}"),
            )
            .await?;
        }
        if let Some(mode) = params["mode"]
            .as_str()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            self.switch_mode(&target, &id, to_zcode_mode(Some(mode)).unwrap_or("build"))
                .await?;
        }
        self.settle_config(&target, &id).await
    }

    pub async fn get_task_config_options(&self, params: &Value) -> Result<Value, RpcError> {
        let id = task_id(params)?;
        let target = self.context.task_target(&id).await?;
        let snapshot = self.resume_snapshot(&target, &id, None, None).await?;
        Ok(settings_to_config_options(&snapshot["settings"]))
    }

    pub async fn get_task_model_selection(&self, params: &Value) -> Result<Value, RpcError> {
        let id = task_id(params)?;
        let target = self.context.task_target(&id).await?;
        Ok(read_current_model(self, &target, &id)
            .await?
            .unwrap_or(Value::Null))
    }

    pub async fn get_task_meta(&self, params: &Value) -> Result<Value, RpcError> {
        let target = target_of(params);
        let id = task_id(params)?;
        let meta = self
            .context
            .index
            .get_task_meta(&target.key(), &id)
            .await
            .map_err(repo_error)?;
        if let Some(meta) = &meta {
            self.context.remember(meta);
        }
        Ok(meta.unwrap_or(Value::Null))
    }

    pub async fn get_task_token_usage(&self, params: &Value) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        let id = task_id(params)?;
        calls::read(
            &self.context.runtime,
            &target,
            "v4/conversation/usage",
            json!({"sessionId": id}),
        )
        .await
    }

    pub async fn get_model_trajectory(&self, params: &Value) -> Result<Value, RpcError> {
        let id = task_id(params)?;
        let paths = &self.context.paths;
        let dirs = trajectory::model_io_dirs(&paths.user_home, &paths.data_base_dir);
        Ok(trajectory::read_model_trajectory(&id, params["limit"].as_f64(), &dirs).await)
    }

    pub async fn get_native_session_log_file(&self) -> Value {
        let paths = &self.context.paths;
        let dir = paths
            .log_dir
            .clone()
            .unwrap_or_else(|| paths.user_home.join(".zcode").join("cli").join("log"));
        let path = dir.join(format!("zcode-{}.jsonl", local_date()));
        let exists = tokio::fs::try_exists(&path).await.unwrap_or(false);
        json!({"provider": "glm", "path": path.to_string_lossy(), "exists": exists})
    }
}
