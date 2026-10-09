//! zcode-task 的查询、配置与文件成员。
use serde_json::{Value, json};
use zcode_host_kit::RpcError;

use super::agent_calls::{self as calls, send_cas};
use super::config_options::{
    MODE_CONFIG_ID, MODEL_CONFIG_ID, THOUGHT_LEVEL_CONFIG_ID, parse_model_picker_value,
    settings_to_config_options, to_zcode_mode,
};
use super::context::{invalid, repo_error, target_of};
use super::lifecycle::TaskLifecycle;
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
