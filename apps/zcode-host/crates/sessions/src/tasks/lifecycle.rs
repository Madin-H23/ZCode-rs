//! zcode-task 的会话生命周期成员（zcodeTaskServiceAdapter.ts）：createTask、sendPrompt、resumeTask、
//! closeTask、task 命令（enqueue / promote / cancel，见 `task_commands`）与 terminal / ready 事件。
//!
//! Node 经 IZCodeAgentService 的 legacy 方法转发，Rust 直接请求 CLI（`agent_calls`）。Host 不保存输入
//! 队列，也不保存正在运行的输入：两者的唯一所有者是 runtime 的 CommandInbox
//! （docs/specs/rust-u3-stage-b.md 3.4）。
//!
//! ```text
//! syncer（sessions-index 终态迁移，带上一份摘要的 activeInputId）─► SessionSignal
//!   ├─► onDynamicTaskTerminalOutcome {taskId, inputId?, outcome}
//!   └─► onDynamicTaskReady {taskId, reason}
//! 排队的下一条输入由 runtime 在 run 结束时开跑，Host 不 drain。
//! ```
use std::sync::Arc;

use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use zcode_host_kit::{EventHub, RpcError};

use super::SessionSignal;
use super::agent_calls::{self as calls, ack_ok, envelope, send_command};
use super::context::TaskContext;
use crate::agent::WorkspaceTarget;

/// 终态与 ready 事件按裸 taskId 分发。
pub type TaskOutcomes = EventHub<Arc<(String, Value)>>;
/// onDynamicTaskEvent / onDynamicStreamEvent：(taskKey, taskId, event)。
pub type TaskStreamEvents = EventHub<Arc<(String, String, Value)>>;

pub fn task_key(target: &WorkspaceTarget, task_id: &str) -> String {
    format!("{}\u{0}{task_id}", target.key())
}

/// automation / off-peak 的写工具黑名单（AUTOMATION_MUTATION_TOOL_NAMES / OFF_PEAK_MUTATION_TOOL_NAMES）。
const AUTOMATION_MUTATION_TOOLS: &[&str] = &["CronCreate", "CronUpdate", "CronDelete"];
const OFF_PEAK_MUTATION_TOOLS: &[&str] = &["OffPeakCreate"];

pub struct TaskLifecycle {
    pub(super) context: Arc<TaskContext>,
    pub(super) terminal: TaskOutcomes,
    pub(super) ready: TaskOutcomes,
    pub(super) stream: TaskStreamEvents,
}

impl TaskLifecycle {
    pub fn start(context: Arc<TaskContext>) -> Arc<Self> {
        let lifecycle = Arc::new(Self {
            context: context.clone(),
            terminal: TaskOutcomes::new(),
            ready: TaskOutcomes::new(),
            stream: TaskStreamEvents::new(),
        });
        let mut signals = Box::pin(context.tasks.signals().subscribe());
        let weak = Arc::downgrade(&lifecycle);
        tokio::spawn(async move {
            while let Some(signal) = signals.next().await {
                let Some(lifecycle) = weak.upgrade() else {
                    return;
                };
                lifecycle.on_signal(&signal);
            }
        });
        lifecycle
    }

    pub fn terminal_outcomes(&self) -> &TaskOutcomes {
        &self.terminal
    }

    pub fn ready_outcomes(&self) -> &TaskOutcomes {
        &self.ready
    }

    pub fn stream_events(&self) -> &TaskStreamEvents {
        &self.stream
    }

    /// 终态 → ready。inputId 是 runtime 给出的这一轮的输入（上一份摘要的 activeInputId）。
    fn on_signal(&self, signal: &SessionSignal) {
        let mut outcome = Map::new();
        outcome.insert("taskId".into(), json!(signal.session_id));
        if let Some(input_id) = &signal.input_id {
            outcome.insert("inputId".into(), json!(input_id));
        }
        outcome.insert(
            "outcome".into(),
            json!(if signal.failed { "failed" } else { "succeeded" }),
        );
        self.terminal.emit(Arc::new((
            signal.session_id.clone(),
            Value::Object(outcome),
        )));
        let reason = if signal.failed {
            "prompt_failed"
        } else {
            "prompt_completed"
        };
        self.ready.emit(Arc::new((
            signal.session_id.clone(),
            json!({"taskId": signal.session_id, "reason": reason}),
        )));
    }

    /// `sendPromptToAgent`：有附件走 `session/send`，否则 V4 `sendText`；输入的 id（V4 commandId）是
    /// traceId。`queued` 是 task 命令的 commandId：带上时要求 runtime 排队（运行中即使 legacy 锁被占用
    /// 也入队），并把它记在队列项上（`externalId`）。
    pub(super) async fn send_prompt_to_agent(
        &self,
        target: &WorkspaceTarget,
        task_id: &str,
        params: &Value,
        queued: Option<&str>,
    ) -> Result<(), RpcError> {
        let trace_id = params["traceId"].as_str().unwrap_or_default();
        let mut denylist: Vec<Value> = params["toolDenylist"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let automation = params["automationId"].as_str().filter(|id| !id.is_empty());
        let off_peak = params["offPeakTaskId"].as_str().filter(|id| !id.is_empty());
        // 权限只看本轮 automationId / offPeakTaskId（resolvePromptToolDenylist），两者各自叠加。
        let extra = automation
            .map(|_| AUTOMATION_MUTATION_TOOLS)
            .into_iter()
            .chain(off_peak.map(|_| OFF_PEAK_MUTATION_TOOLS))
            .flatten();
        for tool in extra {
            if !denylist.iter().any(|existing| existing == tool) {
                denylist.push(json!(tool));
            }
        }
        let turn = Turn {
            trace_id,
            denylist: &denylist,
            automation,
            off_peak,
            queued,
        };
        if params["attachments"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
        {
            self.send_legacy(target, task_id, params, &turn).await
        } else {
            self.send_text(target, task_id, params, &turn).await
        }
    }

    async fn send_text(
        &self,
        target: &WorkspaceTarget,
        task_id: &str,
        params: &Value,
        turn: &Turn<'_>,
    ) -> Result<(), RpcError> {
        let mut payload = Map::new();
        payload.insert("text".into(), params["content"].clone());
        payload.insert("heldQueueDisposition".into(), json!("keepQueueAndSend"));
        if let Some(external) = turn.queued {
            payload.insert("requestedDelivery".into(), json!("queue"));
            payload.insert("externalId".into(), json!(external));
        }
        for field in ["modelSelection", "modelExecution"] {
            if let Some(value) = params.get(field).filter(|v| v.is_object()) {
                payload.insert(field.into(), value.clone());
            }
        }
        if let Some(automation) = turn.automation {
            payload.insert("automationId".into(), json!(automation));
        } else if let Some(off_peak) = turn.off_peak {
            payload.insert("offPeakTaskId".into(), json!(off_peak));
            if let Some(run_type) = params["offPeakRunType"].as_str() {
                payload.insert("offPeakRunType".into(), json!(run_type));
            }
        }
        if let Some(delivery) = params.get("botDeliveryTarget").filter(|v| !v.is_null()) {
            payload.insert("botDeliveryTarget".into(), delivery.clone());
        }
        if !turn.denylist.is_empty() {
            payload.insert(
                "toolDisallowlist".into(),
                Value::Array(turn.denylist.to_vec()),
            );
        }
        let client_id = params["clientId"].as_str();
        let ack = send_command(
            &self.context.runtime,
            target,
            envelope(
                "sendText",
                Value::Object(payload),
                Some(task_id),
                Some(turn.trace_id),
                client_id,
                None,
            ),
        )
        .await?;
        ack_ok("sendText", ack, &format!("session={task_id}")).map(|_| ())
    }

    async fn send_legacy(
        &self,
        target: &WorkspaceTarget,
        task_id: &str,
        params: &Value,
        turn: &Turn<'_>,
    ) -> Result<(), RpcError> {
        // buildSessionSendParams 的键序；expectedRevision / expectedProviderRevision 由 host 不填。
        let mut send = Map::new();
        send.insert("sessionId".into(), json!(task_id));
        for field in ["modelSelection", "modelExecution"] {
            if let Some(value) = params.get(field).filter(|v| v.is_object()) {
                send.insert(field.into(), value.clone());
            }
        }
        send.insert("inputId".into(), json!(turn.trace_id));
        if let Some(query) = params.get("queryId").filter(|v| !v.is_null()) {
            send.insert("queryId".into(), query.clone());
        }
        send.insert("content".into(), params["content"].clone());
        send.insert("attachments".into(), params["attachments"].clone());
        if let Some(automation) = turn.automation {
            send.insert("automationId".into(), json!(automation));
        } else if let Some(off_peak) = turn.off_peak {
            send.insert("offPeakTaskId".into(), json!(off_peak));
            if let Some(run_type) = params["offPeakRunType"].as_str() {
                send.insert("offPeakRunType".into(), json!(run_type));
            }
        }
        if let Some(delivery) = params.get("botDeliveryTarget").filter(|v| !v.is_null()) {
            send.insert("botDeliveryTarget".into(), delivery.clone());
        }
        if !turn.denylist.is_empty() {
            send.insert("toolDenylist".into(), Value::Array(turn.denylist.to_vec()));
        }
        if let Some(external) = turn.queued {
            send.insert("requestedDelivery".into(), json!("queue"));
            send.insert("externalId".into(), json!(external));
        }
        calls::request(
            &self.context.runtime,
            target,
            "session/send",
            Value::Object(send),
        )
        .await
        .map(|_| ())
    }
}

/// 一次提交的输入：id（V4 commandId / legacy inputId）、本轮工具黑名单、归因与排队要求。
struct Turn<'a> {
    trace_id: &'a str,
    denylist: &'a [Value],
    automation: Option<&'a str>,
    off_peak: Option<&'a str>,
    /// task 命令的 commandId（`externalId`）：要求排队。
    queued: Option<&'a str>,
}

pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}
