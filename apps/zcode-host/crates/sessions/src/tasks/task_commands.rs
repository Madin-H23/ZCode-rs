//! zcode-task 的 task 命令（`enqueueTaskCommand` / `promoteTaskCommand` / `cancelTaskCommand`），映射到
//! runtime 的输入队列（docs/specs/rust-u3-stage-b.md 3.4）。
//!
//! Host 不保存队列，也不保存正在运行的输入：每个成员先读 `v4/conversation/inputQueue`，再用 V4 命令改
//! runtime 的 CommandInbox。两个 id 的分工与原来的 drain 相同：`traceId` 是输入的 id（V4 commandId），
//! `commandId` 是调用方给队列项的名字，记在队列项上（`externalId`），promote / cancel 按它找到队列项。
//! owner 与 stale run 的判定与错误码留在这里，判定依据的正在运行的输入由 runtime 给出。
//!
//! ```text
//! enqueue ─► inputQueue（owner；同名项已在队列或正在运行 → 不再提交）
//!          └► sendText(commandId = traceId, externalId = commandId, requestedDelivery: queue)；有附件：session/send
//! promote ─► inputQueue ─► 没有 run：sendQueuedNow │ 已是队首：不动 │ 否则 reorderQueueItem（移到队首之前）
//! cancel  ─► inputQueue ─► deleteQueueItem ─► task_snapshot_updated
//! 写命令都带读到的 baseRevision；stale 时重读再判断，至多 4 次。
//! ```
use std::sync::Arc;

use serde_json::{Map, Value, json};
use zcode_host_kit::RpcError;

use super::agent_calls::{self as calls, ack_ok, envelope, send_command};
use super::context::invalid;
use super::lifecycle::{TaskLifecycle, now_ms, task_key};
use crate::agent::WorkspaceTarget;

/// 写命令遇到 stale 时重读队列的次数上限（与 CAS 命令相同）。
const ATTEMPTS: usize = 4;

fn required<'a>(params: &'a Value, name: &str) -> Result<&'a str, RpcError> {
    params[name]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| invalid(name))
}

fn owner_error(message: &str, code: &str) -> RpcError {
    RpcError::new("Error", message).with_code(code)
}

/// `v4/conversation/inputQueue` 的结果。
struct InputQueue {
    revision: i64,
    /// 有 run 在跑；没有来源命令的轮（目标续跑）也算，此时 `active` 为 null。
    running: bool,
    items: Vec<Value>,
    active: Value,
}

impl InputQueue {
    fn item(&self, command_id: &str) -> Option<&Value> {
        self.items
            .iter()
            .find(|item| item["externalId"] == command_id)
    }

    /// 这个队列项已被 runtime 取出开跑，或已预留（sendQueuedNow / startNow）。
    fn running(&self, command_id: &str) -> bool {
        self.active["externalId"] == command_id
            || self
                .item(command_id)
                .is_some_and(|item| item["dispatch"]["state"] != "queued")
    }

    /// `assertCurrentOwnerRun`：错误码与文案同 Node；正在运行的输入取自 runtime。
    fn assert_owner(&self, params: &Value) -> Result<(), RpcError> {
        let Some(owner) = params["ownerRunId"].as_str().filter(|id| !id.is_empty()) else {
            return Ok(());
        };
        match self.active["inputId"].as_str() {
            // 手机端 owner 命令到达时 task 可能已经终态收口：不能再改动它的队列。
            None => Err(owner_error("No active task owner.", "NO_ACTIVE_TASK_OWNER")),
            // ownerRunId 是远控请求的 stale 防护边界：旧 run 的命令不能修改当前队列。
            Some(active) if active != owner => Err(owner_error(
                "Stale task owner command.",
                "STALE_TASK_OWNER_COMMAND",
            )),
            Some(_) => Ok(()),
        }
    }
}

/// 写命令的结果：成功、要重读（stale），或报错。
enum Write {
    Done,
    Stale,
}

impl TaskLifecycle {
    async fn input_queue(
        &self,
        target: &WorkspaceTarget,
        task_id: &str,
    ) -> Result<InputQueue, RpcError> {
        let state = calls::read(
            &self.context.runtime,
            target,
            "v4/conversation/inputQueue",
            json!({"sessionId": task_id}),
        )
        .await?;
        Ok(InputQueue {
            revision: state["revision"].as_i64().unwrap_or_default(),
            running: state["running"] == true,
            items: state["queue"]["items"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
            active: state["activeInput"].clone(),
        })
    }

    /// 带 baseRevision 的队列命令：stale 时由调用方重读。
    async fn queue_command(
        &self,
        target: &WorkspaceTarget,
        task_id: &str,
        (kind, payload): (&str, Value),
        revision: i64,
    ) -> Result<Write, RpcError> {
        let ack = send_command(
            &self.context.runtime,
            target,
            envelope(kind, payload, Some(task_id), None, None, Some(revision)),
        )
        .await?;
        if ack["status"] == "stale" {
            return Ok(Write::Stale);
        }
        ack_ok(kind, ack, &format!("session={task_id}")).map(|_| Write::Done)
    }

    pub async fn enqueue_command(&self, params: &Value) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        let id = required(params, "taskId")?.to_owned();
        let command_id = required(params, "commandId")?;
        required(params, "traceId")?;
        let queue = self.input_queue(&target, &id).await?;
        queue.assert_owner(params)?;
        let command = enqueued_command(params, &target, &id);
        // 同一个 commandId 的重试：队列项还在或正在运行时不再提交第二条（之后由 runtime 按 traceId 去重）。
        if queue.item(command_id).is_some() || queue.active["externalId"] == command_id {
            return Ok(json!({"accepted": true, "command": command}));
        }
        let mut prompt = Map::new();
        for field in [
            "traceId",
            "queryId",
            "content",
            "attachments",
            "clientId",
            "automationId",
        ] {
            if let Some(value) = params.get(field).filter(|v| !v.is_null()) {
                prompt.insert(field.into(), value.clone());
            }
        }
        self.send_prompt_to_agent(&target, &id, &Value::Object(prompt), Some(command_id))
            .await?;
        Ok(json!({"accepted": true, "command": command}))
    }

    pub async fn promote_command(&self, params: &Value) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        let id = required(params, "taskId")?.to_owned();
        let command_id = required(params, "commandId")?;
        for attempt in 0..ATTEMPTS {
            let queue = self.input_queue(&target, &id).await?;
            if attempt == 0 {
                queue.assert_owner(params)?;
            }
            if queue.running(command_id) {
                return Ok(json!({"accepted": true,
                    "command": queue_command_of(&queue, command_id, &target, &id, "running")}));
            }
            let Some(item) = queue.item(command_id) else {
                return Err(owner_error(
                    "Task command not found.",
                    "OWNER_COMMAND_FAILED",
                ));
            };
            let item_id = item["queueItemId"].clone();
            let write = if !queue.running {
                // 没有 run（队列暂停）：立即开跑这一项，没有可取消的 run。
                ("sendQueuedNow", json!({"queueItemId": item_id}))
            } else if queue.items[0]["queueItemId"] == item_id {
                return Ok(json!({"accepted": true,
                    "command": queue_command_of(&queue, command_id, &target, &id, "accepted")}));
            } else {
                // 移到原队首之前；不用 sendQueuedNow：它会取消正在运行的 run。
                let before = queue.items[0]["queueItemId"].clone();
                (
                    "reorderQueueItem",
                    json!({"queueItemId": item_id, "beforeQueueItemId": before}),
                )
            };
            if let Write::Done = self
                .queue_command(&target, &id, write, queue.revision)
                .await?
            {
                return Ok(json!({"accepted": true,
                    "command": queue_command_of(&queue, command_id, &target, &id, "accepted")}));
            }
        }
        Err(stale_error("promoteTaskCommand", &id))
    }

    pub async fn cancel_command(&self, params: &Value) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        let id = required(params, "taskId")?.to_owned();
        let command_id = required(params, "commandId")?;
        for attempt in 0..ATTEMPTS {
            let queue = self.input_queue(&target, &id).await?;
            if attempt == 0 {
                queue.assert_owner(params)?;
            }
            if queue.running(command_id) {
                return Ok(json!({"canceled": false, "commandId": command_id,
                    "reason": "already_running", "status": "running"}));
            }
            let Some(item) = queue.item(command_id) else {
                return Ok(
                    json!({"canceled": true, "commandId": command_id, "reason": "not_found"}),
                );
            };
            let write = (
                "deleteQueueItem",
                json!({"queueItemId": item["queueItemId"]}),
            );
            if let Write::Done = self
                .queue_command(&target, &id, write, queue.revision)
                .await?
            {
                self.emit_canceled(&target, &id, &item["sourceCommandId"]);
                return Ok(
                    json!({"canceled": true, "commandId": command_id, "status": "accepted"}),
                );
            }
        }
        Err(stale_error("cancelTaskCommand", &id))
    }

    /// 队列项被取消：在任务事件流上发 `task_snapshot_updated`（同 Node）。
    fn emit_canceled(&self, target: &WorkspaceTarget, task_id: &str, trace_id: &Value) {
        let mut event = Map::new();
        event.insert("type".into(), json!("task_snapshot_updated"));
        event.insert("workspacePath".into(), json!(target.workspace_path));
        if let Some(identity) = &target.workspace_identity {
            event.insert("workspaceIdentity".into(), json!(identity));
        }
        event.insert("workspaceKey".into(), json!(target.key()));
        event.insert("taskId".into(), json!(task_id));
        event.insert("traceId".into(), trace_id.clone());
        event.insert("reason".into(), json!("task_status_changed"));
        self.stream.emit(Arc::new((
            task_key(target, task_id),
            task_id.to_owned(),
            Value::Object(event),
        )));
    }
}

fn stale_error(member: &str, task_id: &str) -> RpcError {
    RpcError::new(
        "ZCodeV4CommandRejectedError",
        format!(
            "{member}: the input queue kept changing — session={task_id}（连续 stale，放弃重试）"
        ),
    )
    .with_code("ZCODE_V4_COMMAND_REJECTED")
}

/// command 的公共字段，键序同 `ZCodeTaskRuntimeCommandBase`。
fn command_base(
    target: &WorkspaceTarget,
    (command_id, task_id, trace_id, query_id): (&str, &str, &Value, Option<&Value>),
    status: &str,
    created_at: Value,
) -> Map<String, Value> {
    let mut command = Map::new();
    command.insert("commandId".into(), json!(command_id));
    command.insert("taskId".into(), json!(task_id));
    command.insert("traceId".into(), trace_id.clone());
    if let Some(query) = query_id {
        command.insert("queryId".into(), query.clone());
    }
    command.insert("workspacePath".into(), json!(target.workspace_path));
    if let Some(identity) = &target.workspace_identity {
        command.insert("workspaceIdentity".into(), json!(identity));
    }
    command.insert("workspaceKey".into(), json!(target.key()));
    command.insert("status".into(), json!(status));
    command.insert("createdAt".into(), created_at);
    command.insert("updatedAt".into(), json!(now_ms()));
    command
}

/// `enqueueTaskCommand` 的返回：由参数拼出（同改动前）。
fn enqueued_command(params: &Value, target: &WorkspaceTarget, task_id: &str) -> Value {
    let command_id = params["commandId"].as_str().unwrap_or_default();
    let query = params.get("queryId").filter(|v| !v.is_null());
    let mut command = command_base(
        target,
        (command_id, task_id, &params["traceId"], query),
        "accepted",
        json!(now_ms()),
    );
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
    Value::Object(command)
}

/// `promoteTaskCommand` 的返回：由 runtime 的队列项（或正在运行的输入）重建。Host 不保存入队参数，
/// 所以没有 queryId、clientId、clientLabel、attachments、automationId（rust-u3-stage-b.md 3.4.4 第 4 条）。
fn queue_command_of(
    queue: &InputQueue,
    command_id: &str,
    target: &WorkspaceTarget,
    task_id: &str,
    status: &str,
) -> Value {
    let (trace_id, text, created_at) = match queue.item(command_id) {
        Some(item) => (
            item["sourceCommandId"].clone(),
            item["text"].clone(),
            item["admittedAt"].clone(),
        ),
        None => (
            queue.active["inputId"].clone(),
            queue.active["text"].clone(),
            queue.active["startedAt"].clone(),
        ),
    };
    let created_at = if created_at.is_number() {
        created_at
    } else {
        json!(now_ms())
    };
    let ids = (command_id, task_id, &trace_id, None);
    let mut command = command_base(target, ids, status, created_at);
    command.insert("type".into(), json!("send_prompt"));
    command.insert(
        "content".into(),
        if text.is_string() { text } else { json!("") },
    );
    Value::Object(command)
}
