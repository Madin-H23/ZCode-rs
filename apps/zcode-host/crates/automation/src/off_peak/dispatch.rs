//! 闲时任务派发（packages/desktop/src/host/index.ts 的 dispatchOffPeakRun、finalizeOffPeakRun
//! 与 offPeakDispatchPlan.ts）。由 Host 内的 Scheduler 调用，请求沿用 `off-peak-run` 的形状。
//!
//! 三个分支：resume（已跑过，续跑同一会话、发续跑提示词）、bound-first-run（会话内创建，恢复绑定
//! 会话执行原 prompt）、init（新建专属会话）。闲时 Selection 与逐请求鉴权只注入本轮执行。
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use serde_json::{Value, json};
use zcode_host_kit::log;

use super::models::request_auth;
use super::service::OffPeakService;
use crate::agent::WorkspaceTarget;
use crate::automation::Area;
use crate::automation::driver::{CreateSession, ResumeSession, SendText, TerminalOutcome, uuid_v7};
use crate::tasks_index::off_peak_repo::TerminalUpdate;

pub const OFF_PEAK_RUN: &str = "off-peak-run";
const RESUME_PROMPT: &str = "Continue the previous task from where it left off. The run was interrupted \
(app restart or execution window expired). Do not start over; review what has \
already been done and complete the remaining work.";

/// 派发失败：`permanent` 对应 `OffPeakPermanentDispatchError`（确定性配置错误，重试不会自愈）。
#[derive(Debug, Clone, PartialEq)]
pub struct DispatchError {
    pub message: String,
    pub permanent: bool,
}

fn transient(message: impl Into<String>) -> DispatchError {
    DispatchError {
        message: message.into(),
        permanent: false,
    }
}

fn permanent(message: impl Into<String>) -> DispatchError {
    DispatchError {
        message: message.into(),
        permanent: true,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OffPeakRunRequest {
    pub off_peak_task_id: String,
    pub prompt: String,
    pub permission_mode: String,
    pub model_selection: Value,
    pub conversation_id: Option<String>,
    pub session_id: Option<String>,
    pub server_ticket_id: Option<String>,
    pub workspace_path: String,
    pub workspace_identity: Option<String>,
}

fn required(message: &Value, key: &str) -> Option<String> {
    message[key]
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
}

fn optional(message: &Value, key: &str) -> Option<String> {
    message[key].as_str().map(str::to_owned)
}

impl OffPeakRunRequest {
    pub fn parse(message: &Value) -> Option<Self> {
        Some(Self {
            off_peak_task_id: required(message, "offPeakTaskId")?,
            prompt: required(message, "prompt")?,
            permission_mode: required(message, "permissionMode")?,
            model_selection: crate::tasks_index::automation_repo::parse_model_selection(
                &message["modelSelection"],
            )?,
            conversation_id: optional(message, "conversationId"),
            session_id: optional(message, "sessionId"),
            server_ticket_id: optional(message, "serverTicketId"),
            workspace_path: required(message, "workspacePath")?,
            workspace_identity: optional(message, "workspaceIdentity"),
        })
    }

    fn target(&self) -> WorkspaceTarget {
        WorkspaceTarget {
            workspace_path: self.workspace_path.clone(),
            workspace_identity: self.workspace_identity.clone(),
            remote_session_id: None,
        }
    }
}

/// `resolveOffPeakDispatchKind`。
pub fn dispatch_kind(conversation_id: Option<&str>, session_id: Option<&str>) -> &'static str {
    if conversation_id.is_some_and(|id| !id.trim().is_empty()) {
        "resume"
    } else if session_id.is_some_and(|id| !id.trim().is_empty()) {
        "bound-first-run"
    } else {
        "init"
    }
}

struct Tracked {
    off_peak_task_id: String,
    target: WorkspaceTarget,
    session_id: String,
}

pub struct OffPeakDispatcher {
    area: Arc<Area>,
    service: Arc<OffPeakService>,
    tracked: Mutex<HashMap<String, Tracked>>,
}

fn track_key(workspace_key: &str, session_id: &str, input_id: &str) -> String {
    format!("{workspace_key}\u{0}{session_id}\u{0}{input_id}")
}

impl OffPeakDispatcher {
    pub fn start(area: Arc<Area>, service: Arc<OffPeakService>) -> Arc<Self> {
        let dispatcher = Arc::new(Self {
            area: area.clone(),
            service,
            tracked: Mutex::new(HashMap::new()),
        });
        let mut outcomes = Box::pin(area.driver.terminal_outcomes());
        let weak = Arc::downgrade(&dispatcher);
        drop(
            area.spawner
                .spawn_resident("off-peak-finalize", async move {
                    while let Some(outcome) = outcomes.next().await {
                        let Some(dispatcher) = weak.upgrade() else {
                            return;
                        };
                        dispatcher.finalize(outcome).await;
                    }
                }),
        );
        dispatcher
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Tracked>> {
        self.tracked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub async fn dispatch(
        self: &Arc<Self>,
        request: &OffPeakRunRequest,
    ) -> Result<String, DispatchError> {
        let Some(ticket_id) = request.server_ticket_id.as_deref() else {
            return Err(transient("off-peak dispatch without server ticket"));
        };
        if !self
            .service
            .validate_dispatch_selection(&request.model_selection)
            .await
        {
            return Err(permanent(
                "off-peak dispatch has no usable model (Built-in Provider models empty)",
            ));
        }
        // 凭据缺失与套餐不可用是确定性配置错误（OffPeakPermanentDispatchError）；其余按暂时失败重试。
        let credentials = self.service.client().credentials().await.map_err(|error| {
            if error.permanent() {
                permanent(error.to_string())
            } else {
                transient(error.to_string())
            }
        })?;
        let auth = request_auth(&credentials, ticket_id);
        let target = request.target();
        let kind = dispatch_kind(
            request.conversation_id.as_deref(),
            request.session_id.as_deref(),
        );
        let driver = &self.area.driver;
        let rpc = |error: zcode_host_kit::RpcError| transient(error.message);
        let mut prompt = request.prompt.clone();
        let (task_id, trace_id) = match kind {
            "bound-first-run" => {
                let task_id = request.session_id.clone().unwrap_or_default();
                let trace_id = format!("{}:bound:{}", request.off_peak_task_id, uuid_v7());
                self.assert_bound_dispatchable(&target, &task_id).await?;
                driver
                    .resume_session(
                        &target,
                        &task_id,
                        ResumeSession {
                            thought_level: self.resume_thought_level(&target, &task_id).await,
                            automation_id: None,
                            off_peak_task_id: Some(request.off_peak_task_id.clone()),
                        },
                    )
                    .await
                    .map_err(rpc)?;
                driver
                    .set_config_option(&target, &task_id, "mode", &request.permission_mode)
                    .await
                    .map_err(rpc)?;
                (task_id, trace_id)
            }
            "resume" => {
                let task_id = request.conversation_id.clone().unwrap_or_default();
                let trace_id = format!("{}:resume:{}", request.off_peak_task_id, uuid_v7());
                prompt = RESUME_PROMPT.to_owned();
                driver
                    .resume_session(
                        &target,
                        &task_id,
                        ResumeSession {
                            thought_level: self.resume_thought_level(&target, &task_id).await,
                            automation_id: None,
                            off_peak_task_id: Some(request.off_peak_task_id.clone()),
                        },
                    )
                    .await
                    .map_err(rpc)?;
                driver
                    .set_config_option(&target, &task_id, "mode", &request.permission_mode)
                    .await
                    .map_err(rpc)?;
                (task_id, trace_id)
            }
            _ => {
                let meta = driver
                    .create_session(
                        &target,
                        CreateSession {
                            mode: Some(request.permission_mode.clone()),
                            off_peak_task_id: Some(request.off_peak_task_id.clone()),
                            defer_persistence: true,
                            ..CreateSession::default()
                        },
                    )
                    .await
                    .map_err(rpc)?;
                let task_id = meta["taskId"].as_str().unwrap_or_default().to_owned();
                let trace_id = meta["traceId"].as_str().map_or_else(uuid_v7, str::to_owned);
                (task_id, trace_id)
            }
        };
        let key = track_key(&target.key(), &task_id, &trace_id);
        self.lock().insert(
            key.clone(),
            Tracked {
                off_peak_task_id: request.off_peak_task_id.clone(),
                target: target.clone(),
                session_id: task_id.clone(),
            },
        );
        let sent = driver
            .send_text(
                &target,
                &task_id,
                SendText {
                    input_id: trace_id.clone(),
                    text: prompt,
                    model_selection: Some(request.model_selection.clone()),
                    model_execution: Some(json!({
                        "memoryExtraction": "skip",
                        "selectionScope": "execution",
                        "requestAuth": auth,
                        "subagents": {"foregroundModel": "submission", "background": "deny"},
                    })),
                    off_peak_task_id: Some(request.off_peak_task_id.clone()),
                    off_peak_run_type: Some(
                        if kind == "resume" { "resume" } else { "init" }.into(),
                    ),
                    tool_denylist: vec!["CronCreate".into(), "OffPeakCreate".into()],
                    ..SendText::default()
                },
            )
            .await;
        if let Err(error) = sent {
            self.lock().remove(&key);
            return Err(transient(error.message));
        }
        // 只有 init 实际新建；绑定首跑与跨票续跑是原会话的后续输入（hostSessionCreateTelemetry）。
        if kind == "init" {
            crate::integration::report_session_create(
                &self.area.bridge,
                &self.area.context.environment.vars,
                crate::integration::SessionCreate {
                    session_id: &task_id,
                    message_id: &trace_id,
                    source: crate::integration::CreateSource::AutomationIdle,
                    workspace_identity: target.workspace_identity.as_deref(),
                },
            );
        }
        Ok(task_id)
    }

    /// `assertBoundSessionDispatchable`：绑定会话已删除（permanent）或正忙（transient，退避重试）。
    async fn assert_bound_dispatchable(
        &self,
        target: &WorkspaceTarget,
        session_id: &str,
    ) -> Result<(), DispatchError> {
        let index = &self.area.tasks_index;
        let deleted = index
            .list_deleted_task_ids(&target.key(), None)
            .await
            .map_err(|error| transient(error.to_string()))?
            .iter()
            .any(|id| id == session_id);
        if deleted {
            return Err(permanent(format!(
                "off-peak bound session was deleted: {session_id}"
            )));
        }
        let running = index
            .get_task_meta(&target.key(), session_id)
            .await
            .map_err(|error| transient(error.to_string()))?
            .is_some_and(|meta| meta["status"] == "running");
        if running {
            return Err(transient(format!(
                "off-peak bound session is busy: {session_id}"
            )));
        }
        Ok(())
    }

    /// `resolveTaskIndexResumeHints`：恢复历史会话时回填 tasks-index 里的档位。
    async fn resume_thought_level(
        &self,
        target: &WorkspaceTarget,
        session_id: &str,
    ) -> Option<String> {
        let meta = self
            .area
            .tasks_index
            .get_task_meta(&target.key(), session_id)
            .await
            .ok()
            .flatten()?;
        meta["thoughtLevel"]
            .as_str()
            .map(str::trim)
            .filter(|level| !level.is_empty())
            .map(str::to_owned)
    }

    /// loop 终态 → off_peak_tasks 终态：succeeded → completed，stopped → cancelled，其余 → failed。
    async fn finalize(&self, outcome: TerminalOutcome) {
        let Some(input_id) = &outcome.input_id else {
            return;
        };
        let key = track_key(&outcome.workspace_key, &outcome.session_id, input_id);
        let Some(tracked) = self.lock().remove(&key) else {
            return;
        };
        let status = match outcome.outcome {
            "succeeded" => "completed",
            "stopped" => "cancelled",
            _ => "failed",
        };
        let update = TerminalUpdate {
            status: status.into(),
            ended_at: now_ms(),
            ..TerminalUpdate::default()
        };
        match self
            .service
            .repo()
            .mark_terminal(&tracked.off_peak_task_id, update)
            .await
        {
            Ok(Some(_)) => log::info(format!(
                "off-peak run finished task={} status={status} filesChanged=n/a",
                tracked.off_peak_task_id
            )),
            Ok(None) => {
                log::info(format!(
                    "off-peak terminal writeback dropped (already terminal) task={}",
                    tracked.off_peak_task_id
                ));
                return;
            }
            Err(error) => {
                log::warn(format!("off-peak 终态回写失败: {error:#}"));
                return;
            }
        }
        self.area
            .driver
            .mark_unread(&tracked.target, &tracked.session_id)
            .await;
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}
