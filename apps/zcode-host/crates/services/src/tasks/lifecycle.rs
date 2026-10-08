//! zcode-task 的会话生命周期成员（zcodeTaskServiceAdapter.ts）：createTask、sendPrompt、resumeTask、
//! closeTask、host command 队列（enqueue / promote / cancel）与 terminal / ready 事件。
//!
//! Node 经 IZCodeAgentService 的 legacy 方法转发，Rust 直接请求 CLI（`agent_calls`）。状态：
//! - active input：taskKey → 当前 prompt 的 inputId（sendPrompt 设置，终态 / ready 清除）；
//! - runtime command 队列：taskKey → 待发命令，按 ready 边界逐条 drain。
//!
//! 二者都只在内存里，Host 重启即清空（同 Node）。
//!
//! ```text
//! syncer（sessions-index 终态迁移）─► SessionSignal
//!   ├─► onDynamicTaskTerminalOutcome {taskId, inputId?, outcome}（inputId 取发出时的 active input）
//!   ├─► 收口 running 命令、清 active input
//!   ├─► onDynamicTaskReady {taskId, reason}
//!   └─► drain 下一条 accepted 命令（V4 sendText / session/send）
//! ```
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use zcode_host_kit::{EventHub, RpcError, log};

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
    active_inputs: Mutex<HashMap<String, String>>,
    pub(super) commands: Mutex<HashMap<String, Vec<Value>>>,
    drains: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    pub(super) terminal: TaskOutcomes,
    pub(super) ready: TaskOutcomes,
    pub(super) stream: TaskStreamEvents,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl TaskLifecycle {
    pub fn start(context: Arc<TaskContext>) -> Arc<Self> {
        let lifecycle = Arc::new(Self {
            context: context.clone(),
            active_inputs: Mutex::new(HashMap::new()),
            commands: Mutex::new(HashMap::new()),
            drains: Mutex::new(HashMap::new()),
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

    pub(super) fn active_input(&self, key: &str) -> Option<String> {
        lock(&self.active_inputs).get(key).cloned()
    }

    pub(super) fn set_active_input(&self, key: String, input_id: String) {
        lock(&self.active_inputs).insert(key, input_id);
    }

    pub(super) fn clear_active_input(&self, key: &str) {
        lock(&self.active_inputs).remove(key);
    }

    fn on_signal(self: &Arc<Self>, signal: &SessionSignal) {
        let key = task_key(&signal.target, &signal.session_id);
        let input_id = self.active_input(&key);
        let mut outcome = Map::new();
        outcome.insert("taskId".into(), json!(signal.session_id));
        if let Some(input_id) = &input_id {
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
        let tracked = input_id.is_some() || lock(&self.commands).contains_key(&key);
        if tracked {
            // 终态只说明流已收口；下一条命令要等 ready 边界（同 Node 的两段处理）。
            self.complete_command(&key, input_id.as_deref());
            self.clear_active_input(&key);
        }
        let reason = if signal.failed {
            "prompt_failed"
        } else {
            "prompt_completed"
        };
        self.ready.emit(Arc::new((
            signal.session_id.clone(),
            json!({"taskId": signal.session_id, "reason": reason}),
        )));
        if tracked {
            self.schedule_drain(
                signal.target.clone(),
                signal.session_id.clone(),
                "session-ready",
            );
        }
    }

    /// `completeRuntimeCommandByInputId`：移除 traceId 等于终态 input 的 running 命令。
    fn complete_command(&self, key: &str, input_id: Option<&str>) {
        let Some(input_id) = input_id else {
            return;
        };
        let mut commands = lock(&self.commands);
        if let Some(queue) = commands.get_mut(key) {
            queue.retain(|command| {
                !(command["type"] == "send_prompt"
                    && command["status"] == "running"
                    && command["traceId"] == input_id)
            });
            if queue.is_empty() {
                commands.remove(key);
            }
        }
    }

    /// 每个 taskKey 的 drain 串行执行（Node 的 promise 链）。
    pub(super) fn schedule_drain(
        self: &Arc<Self>,
        target: WorkspaceTarget,
        task_id: String,
        reason: &'static str,
    ) {
        let key = task_key(&target, &task_id);
        let gate = lock(&self.drains).entry(key).or_default().clone();
        let lifecycle = self.clone();
        tokio::spawn(async move {
            let _serial = gate.lock().await;
            lifecycle.drain(&target, &task_id, reason).await;
        });
    }

    async fn drain(&self, target: &WorkspaceTarget, task_id: &str, reason: &str) {
        let key = task_key(target, task_id);
        if self.active_input(&key).is_some() {
            return;
        }
        let next = {
            let mut commands = lock(&self.commands);
            let Some(queue) = commands.get_mut(&key) else {
                return;
            };
            if queue.iter().any(|command| command["status"] == "running") {
                return;
            }
            let Some(command) = queue
                .iter_mut()
                .find(|c| c["type"] == "send_prompt" && c["status"] == "accepted")
            else {
                return;
            };
            command["status"] = json!("running");
            command["updatedAt"] = json!(now_ms());
            command.clone()
        };
        let mut prompt = Map::new();
        for field in ["traceId", "queryId", "content", "attachments", "clientId"] {
            if let Some(value) = next.get(field).filter(|v| !v.is_null()) {
                prompt.insert(field.into(), value.clone());
            }
        }
        prompt.insert("messageId".into(), next["commandId"].clone());
        if let Some(automation) = next.get("automationId").filter(|v| !v.is_null()) {
            prompt.insert("automationId".into(), automation.clone());
        }
        log::info(format!(
            "ZCode task command drain reason={reason} commandId={} taskId={task_id}",
            next["commandId"].as_str().unwrap_or_default()
        ));
        if let Err(error) = self
            .send_prompt_to_agent(target, task_id, &Value::Object(prompt))
            .await
        {
            let mut commands = lock(&self.commands);
            if let Some(command) = commands.get_mut(&key).and_then(|queue| {
                queue
                    .iter_mut()
                    .find(|c| c["commandId"] == next["commandId"])
            }) {
                command["status"] = json!("failed");
                command["error"] = json!(error.message);
                command["updatedAt"] = json!(now_ms());
            }
        }
    }

    /// `sendPromptToAgent`：有附件走 `session/send`，否则 V4 `sendText`；失败时清掉 active input。
    pub(super) async fn send_prompt_to_agent(
        &self,
        target: &WorkspaceTarget,
        task_id: &str,
        params: &Value,
    ) -> Result<(), RpcError> {
        let key = task_key(target, task_id);
        let trace_id = params["traceId"].as_str().unwrap_or_default().to_owned();
        self.set_active_input(key.clone(), trace_id.clone());
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
        let result = if params["attachments"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
        {
            self.send_legacy(
                target, task_id, &trace_id, params, &denylist, automation, off_peak,
            )
            .await
        } else {
            self.send_text(
                target, task_id, &trace_id, params, &denylist, automation, off_peak,
            )
            .await
        };
        if result.is_err() {
            self.clear_active_input(&key);
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_text(
        &self,
        target: &WorkspaceTarget,
        task_id: &str,
        trace_id: &str,
        params: &Value,
        denylist: &[Value],
        automation: Option<&str>,
        off_peak: Option<&str>,
    ) -> Result<(), RpcError> {
        let mut payload = Map::new();
        payload.insert("text".into(), params["content"].clone());
        payload.insert("heldQueueDisposition".into(), json!("keepQueueAndSend"));
        for field in ["modelSelection", "modelExecution"] {
            if let Some(value) = params.get(field).filter(|v| v.is_object()) {
                payload.insert(field.into(), value.clone());
            }
        }
        if let Some(automation) = automation {
            payload.insert("automationId".into(), json!(automation));
        } else if let Some(off_peak) = off_peak {
            payload.insert("offPeakTaskId".into(), json!(off_peak));
            if let Some(run_type) = params["offPeakRunType"].as_str() {
                payload.insert("offPeakRunType".into(), json!(run_type));
            }
        }
        if let Some(delivery) = params.get("botDeliveryTarget").filter(|v| !v.is_null()) {
            payload.insert("botDeliveryTarget".into(), delivery.clone());
        }
        if !denylist.is_empty() {
            payload.insert("toolDisallowlist".into(), Value::Array(denylist.to_vec()));
        }
        let client_id = params["clientId"].as_str();
        let ack = send_command(
            &self.context.runtime,
            target,
            envelope(
                "sendText",
                Value::Object(payload),
                Some(task_id),
                Some(trace_id),
                client_id,
                None,
            ),
        )
        .await?;
        ack_ok("sendText", ack, &format!("session={task_id}")).map(|_| ())
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_legacy(
        &self,
        target: &WorkspaceTarget,
        task_id: &str,
        trace_id: &str,
        params: &Value,
        denylist: &[Value],
        automation: Option<&str>,
        off_peak: Option<&str>,
    ) -> Result<(), RpcError> {
        // buildSessionSendParams 的键序；expectedRevision / expectedProviderRevision 由 host 不填。
        let mut send = Map::new();
        send.insert("sessionId".into(), json!(task_id));
        for field in ["modelSelection", "modelExecution"] {
            if let Some(value) = params.get(field).filter(|v| v.is_object()) {
                send.insert(field.into(), value.clone());
            }
        }
        send.insert("inputId".into(), json!(trace_id));
        if let Some(query) = params.get("queryId").filter(|v| !v.is_null()) {
            send.insert("queryId".into(), query.clone());
        }
        send.insert("content".into(), params["content"].clone());
        send.insert("attachments".into(), params["attachments"].clone());
        if let Some(automation) = automation {
            send.insert("automationId".into(), json!(automation));
        } else if let Some(off_peak) = off_peak {
            send.insert("offPeakTaskId".into(), json!(off_peak));
            if let Some(run_type) = params["offPeakRunType"].as_str() {
                send.insert("offPeakRunType".into(), json!(run_type));
            }
        }
        if let Some(delivery) = params.get("botDeliveryTarget").filter(|v| !v.is_null()) {
            send.insert("botDeliveryTarget".into(), delivery.clone());
        }
        if !denylist.is_empty() {
            send.insert("toolDenylist".into(), Value::Array(denylist.to_vec()));
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

pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}
