//! Bot 读会话的实时部分：把 V4 会话帧投影成 Node `watchTaskStream` 消费的事件
//! （docs/specs/rust-host-bots.md 第 4 节）。
//!
//! Node Bot 订阅 legacy 任务事件流（agent_message_chunk、tool_call、task_complete…）；Rust Host 不提供
//! legacy 事件面，Bot 直接订阅 `conversation/<taskId>` 的 V4 帧。投影器只看行与状态的变化：
//! 初始快照是基线（只记录、不产生事件），之后的 delta 与 resync 快照按行 id 求差。
use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

/// 投影出的事件（与 Node 的 `ZCodeStreamEvent` 子集一一对应）。
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// `agent_message_chunk`。
    MessageChunk(String),
    /// `agent_thought_chunk`。
    ThoughtChunk(String),
    /// `tool_call`（首次出现）。
    ToolCall(ToolState),
    /// `tool_call_update`（状态、输入或输出变化）。
    ToolUpdate(ToolState),
    /// `permission_request` / `elicitation_request`：新增的待处理交互（V4 `PendingInteraction`）。
    Interaction(Value),
    /// `elicitation_response`：交互从待处理列表消失（已在别处回答或超时）。
    InteractionResolved(String),
    /// `task_complete`。
    Complete,
    /// `task_error`。
    Failed(String),
}

/// `BotReplyToolCallState` 需要的工具行字段。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolState {
    pub tool_id: String,
    pub tool_name: String,
    pub input: Value,
    pub output: Value,
    /// Node 词表：pending / in_progress / completed / failed / stopped。
    pub status: &'static str,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
enum Row {
    Text { reasoning: bool, text: String },
    Tool(ToolState),
    Turn { terminal: bool },
    Other,
}

/// V4 工具状态 → Node 词表（第 4.1 节）。
pub fn tool_status(status: &str) -> &'static str {
    match status {
        "running" => "in_progress",
        "success" => "completed",
        "error" => "failed",
        "cancelled" => "stopped",
        _ => "pending",
    }
}

fn is_terminal_phase(phase: &str) -> bool {
    matches!(phase, "completedSuccess" | "completedInterrupted" | "error")
}

fn tool_state(row: &Value) -> ToolState {
    ToolState {
        tool_id: row["toolCallId"].as_str().unwrap_or_default().to_owned(),
        tool_name: row["toolName"].as_str().unwrap_or_default().to_owned(),
        input: row.get("input").cloned().unwrap_or(Value::Null),
        output: row.get("output").cloned().unwrap_or(Value::Null),
        status: tool_status(row["status"].as_str().unwrap_or_default()),
        error: row["error"]["message"].as_str().map(str::to_owned),
    }
}

fn parse_row(row: &Value) -> Row {
    match row["kind"].as_str() {
        Some(kind @ ("assistantText" | "reasoning")) => Row::Text {
            reasoning: kind == "reasoning",
            text: row["text"].as_str().unwrap_or_default().to_owned(),
        },
        Some("toolCall") => Row::Tool(tool_state(row)),
        Some("turnHeader") => Row::Turn {
            terminal: matches!(
                row["state"].as_str(),
                Some("completedSuccess" | "completedInterrupted" | "failed")
            ),
        },
        _ => Row::Other,
    }
}

/// 一个会话订阅的投影状态。
#[derive(Default)]
pub struct Projector {
    initialized: bool,
    rows: BTreeMap<i64, Row>,
    /// 基线之后出现的最大行 id 之前的行都已知；resync 快照里比它大的新行才算新增。
    max_row: Option<i64>,
    interactions: HashMap<String, Value>,
    phase: String,
    /// 基线之后观察到运行（phase 非终态非草稿，或出现新的轮次）。
    active: bool,
    last_error: Option<String>,
    /// 本轮是否已经失败（失败轮次的 turnHeader）。
    failed_turn: bool,
    /// 本帧观察到轮次收口，帧处理完后发终态事件。
    pending_terminal: bool,
    finished: bool,
}

impl Projector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// 喂入一个逻辑帧（`{payload: {kind: snapshot|deltas, ...}}`），返回本帧产生的事件。
    pub fn apply(&mut self, frame: &Value) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if self.finished {
            return events;
        }
        let payload = &frame["payload"];
        match payload["kind"].as_str() {
            Some("snapshot") => self.apply_snapshot(&payload["snapshot"], &mut events),
            Some("deltas") => {
                if !self.initialized {
                    // 没有基线就无法求差：丢弃，等待快照。
                    return events;
                }
                for delta in payload["deltas"].as_array().into_iter().flatten() {
                    self.apply_delta(delta, &mut events);
                }
            }
            _ => {}
        }
        self.settle(&mut events);
        events
    }

    fn apply_snapshot(&mut self, snapshot: &Value, events: &mut Vec<StreamEvent>) {
        let rows = snapshot["rows"]["window"].as_array().cloned().unwrap_or_default();
        let interactions = snapshot["pendingInteractions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if !self.initialized {
            for row in &rows {
                if let Some(id) = row["rowId"].as_i64() {
                    self.rows.insert(id, parse_row(row));
                    self.max_row = Some(self.max_row.map_or(id, |max| max.max(id)));
                }
            }
            for interaction in interactions {
                if let Some(id) = interaction["interactionId"].as_str() {
                    self.interactions.insert(id.to_owned(), interaction.clone());
                }
            }
            self.apply_control(&snapshot["control"], false);
            self.active = matches!(self.phase.as_str(), "prewarming" | "running");
            self.initialized = true;
            return;
        }
        // resync：行按 id 求差，窗口外被截掉的旧行不算删除。
        for row in &rows {
            self.upsert_row(row, events);
        }
        self.apply_interactions(&interactions, events);
        self.apply_control(&snapshot["control"], true);
    }

    fn apply_delta(&mut self, delta: &Value, events: &mut Vec<StreamEvent>) {
        match delta["op"].as_str() {
            Some("row.appended" | "row.upserted") => self.upsert_row(&delta["row"], events),
            Some("row.removed") => {
                if let Some(from) = delta["fromRowId"].as_i64() {
                    self.rows.retain(|id, _| *id < from);
                }
            }
            Some("row.delta") => {
                let (Some(id), Some(append)) = (delta["rowId"].as_i64(), delta["append"].as_str())
                else {
                    return;
                };
                if delta["path"] != "text" || append.is_empty() {
                    return;
                }
                if let Some(Row::Text { reasoning, text }) = self.rows.get_mut(&id) {
                    text.push_str(append);
                    events.push(if *reasoning {
                        StreamEvent::ThoughtChunk(append.to_owned())
                    } else {
                        StreamEvent::MessageChunk(append.to_owned())
                    });
                }
            }
            Some("state.updated") => {
                let patch = &delta["patch"];
                if let Some(interactions) = patch["pendingInteractions"].as_array() {
                    self.apply_interactions(interactions, events);
                }
                if patch["control"].is_object() {
                    self.apply_control(&patch["control"], true);
                }
            }
            _ => {}
        }
    }

    fn upsert_row(&mut self, row: &Value, events: &mut Vec<StreamEvent>) {
        let Some(id) = row["rowId"].as_i64() else {
            return;
        };
        let next = parse_row(row);
        let is_new_row = self.max_row.is_none_or(|max| id > max);
        match (self.rows.get(&id), &next) {
            (None, Row::Text { reasoning, text }) => {
                if is_new_row && !text.is_empty() {
                    events.push(chunk(*reasoning, text.clone()));
                }
            }
            (Some(Row::Text { text: previous, .. }), Row::Text { reasoning, text }) => {
                if let Some(suffix) = text.strip_prefix(previous.as_str())
                    && !suffix.is_empty()
                {
                    events.push(chunk(*reasoning, suffix.to_owned()));
                }
            }
            (None, Row::Tool(tool)) => {
                if is_new_row {
                    events.push(StreamEvent::ToolCall(tool.clone()));
                    if tool.status != "pending" {
                        events.push(StreamEvent::ToolUpdate(tool.clone()));
                    }
                }
            }
            (Some(Row::Tool(previous)), Row::Tool(tool)) => {
                if previous != tool {
                    events.push(StreamEvent::ToolUpdate(tool.clone()));
                }
            }
            (None, Row::Turn { terminal }) => {
                if is_new_row {
                    self.active = true;
                    if *terminal {
                        self.finish_turn(row);
                    }
                }
            }
            (Some(Row::Turn { terminal: false }), Row::Turn { terminal: true }) => {
                if self.active || is_new_row {
                    self.active = true;
                    self.finish_turn(row);
                }
            }
            _ => {}
        }
        self.max_row = Some(self.max_row.map_or(id, |max| max.max(id)));
        self.rows.insert(id, next);
    }

    fn finish_turn(&mut self, row: &Value) {
        if row["state"] == "failed" {
            self.failed_turn = true;
        }
        self.pending_terminal = true;
    }

    fn apply_interactions(&mut self, interactions: &[Value], events: &mut Vec<StreamEvent>) {
        let mut next = HashMap::new();
        for interaction in interactions {
            let Some(id) = interaction["interactionId"].as_str() else {
                continue;
            };
            if !self.interactions.contains_key(id) {
                events.push(StreamEvent::Interaction(interaction.clone()));
            }
            next.insert(id.to_owned(), interaction.clone());
        }
        for id in self.interactions.keys() {
            if !next.contains_key(id) {
                events.push(StreamEvent::InteractionResolved(id.clone()));
            }
        }
        self.interactions = next;
    }

    fn apply_control(&mut self, control: &Value, live: bool) {
        if let Some(error) = control["lastError"]["message"].as_str() {
            self.last_error = Some(error.to_owned());
        }
        let Some(phase) = control["phase"].as_str() else {
            return;
        };
        if live {
            if matches!(phase, "prewarming" | "running") {
                self.active = true;
            } else if is_terminal_phase(phase) && self.active && !is_terminal_phase(&self.phase) {
                if phase == "error" {
                    self.failed_turn = true;
                }
                self.pending_terminal = true;
            }
        }
        self.phase = phase.to_owned();
    }

    /// 一帧处理完后再判断终态：同一帧里 lastError 可能晚于轮次收口到达。
    fn settle(&mut self, events: &mut Vec<StreamEvent>) {
        if !self.pending_terminal || self.finished {
            return;
        }
        self.finished = true;
        events.push(if self.failed_turn {
            StreamEvent::Failed(self.last_error.clone().unwrap_or_default())
        } else {
            StreamEvent::Complete
        });
    }
}

fn chunk(reasoning: bool, text: String) -> StreamEvent {
    if reasoning {
        StreamEvent::ThoughtChunk(text)
    } else {
        StreamEvent::MessageChunk(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snapshot(rows: Value, phase: &str, interactions: Value) -> Value {
        json!({"payload": {"kind": "snapshot", "snapshot": {"control": {"phase": phase, "lastError": null},
            "pendingInteractions": interactions, "rows": {"window": rows}}}})
    }

    fn deltas(deltas: Value) -> Value {
        json!({"payload": {"kind": "deltas", "deltas": deltas}})
    }

    #[test]
    fn projects_v4_frames_into_bot_events() {
        let mut projector = Projector::new();
        // 没有基线时丢弃 delta。
        assert!(projector.apply(&deltas(json!([{"op": "row.delta", "rowId": 1, "path": "text", "append": "x"}]))).is_empty());
        let baseline = json!([
            {"rowId": 1, "kind": "turnHeader", "state": "completedSuccess"},
            {"rowId": 2, "kind": "assistantText", "text": "old answer", "state": "complete"},
        ]);
        assert!(projector.apply(&snapshot(baseline, "completedSuccess", json!([]))).is_empty());
        assert!(projector.is_initialized());
        let events = projector.apply(&deltas(json!([
            {"op": "row.appended", "row": {"rowId": 3, "kind": "turnHeader", "state": "running"}},
            {"op": "state.updated", "patch": {"control": {"phase": "running"}}},
            {"op": "row.appended", "row": {"rowId": 4, "kind": "reasoning", "text": "think", "state": "streaming"}},
            {"op": "row.appended", "row": {"rowId": 5, "kind": "toolCall", "toolCallId": "c1", "toolName": "Bash",
                "status": "running", "inputText": "", "input": {"command": "ls"}}},
            {"op": "row.upserted", "row": {"rowId": 5, "kind": "toolCall", "toolCallId": "c1", "toolName": "Bash",
                "status": "error", "inputText": "", "input": {"command": "ls"}, "error": {"code": "x", "message": "boom"}}},
            {"op": "row.appended", "row": {"rowId": 6, "kind": "assistantText", "text": "He", "state": "streaming"}},
            {"op": "row.delta", "rowId": 6, "path": "text", "append": "llo"},
            {"op": "row.delta", "rowId": 2, "path": "inputText", "append": "ignored"},
        ])));
        let tool = |status: &'static str, error: Option<&str>| ToolState {
            tool_id: "c1".into(),
            tool_name: "Bash".into(),
            input: json!({"command": "ls"}),
            output: Value::Null,
            status,
            error: error.map(str::to_owned),
        };
        assert_eq!(
            events,
            vec![
                StreamEvent::ThoughtChunk("think".into()),
                StreamEvent::ToolCall(tool("in_progress", None)),
                StreamEvent::ToolUpdate(tool("in_progress", None)),
                StreamEvent::ToolUpdate(tool("failed", Some("boom"))),
                StreamEvent::MessageChunk("He".into()),
                StreamEvent::MessageChunk("llo".into()),
            ]
        );
        // 交互出现与消失。
        let interaction = json!({"interactionId": "i1", "kind": "userInput", "anchorRowId": null, "createdAt": 1,
            "payload": {"kind": "userInput", "prompt": "Q?", "freeText": true}});
        let events = projector.apply(&deltas(json!([{"op": "state.updated", "patch": {"pendingInteractions": [interaction]}}])));
        assert_eq!(events, vec![StreamEvent::Interaction(interaction.clone())]);
        let events = projector.apply(&deltas(json!([{"op": "state.updated", "patch": {"pendingInteractions": []}}])));
        assert_eq!(events, vec![StreamEvent::InteractionResolved("i1".into())]);
        // resync 快照：已知行求差，新行产生事件。
        let resync = snapshot(
            json!([
                {"rowId": 6, "kind": "assistantText", "text": "Hello world", "state": "streaming"},
                {"rowId": 7, "kind": "assistantText", "text": "!", "state": "streaming"},
            ]),
            "running",
            json!([]),
        );
        assert_eq!(
            projector.apply(&resync),
            vec![StreamEvent::MessageChunk(" world".into()), StreamEvent::MessageChunk("!".into())]
        );
        // 终态：轮次收口与 lastError 在同一帧，帧处理完才发事件。
        let events = projector.apply(&deltas(json!([
            {"op": "row.upserted", "row": {"rowId": 3, "kind": "turnHeader", "state": "failed"}},
            {"op": "state.updated", "patch": {"control": {"phase": "error", "lastError": {"message": "quota"}}}},
        ])));
        assert_eq!(events, vec![StreamEvent::Failed("quota".into())]);
        assert!(projector.is_finished());
        assert!(projector.apply(&deltas(json!([{"op": "row.delta", "rowId": 6, "path": "text", "append": "x"}]))).is_empty());
    }

    #[test]
    fn completes_when_the_phase_settles_without_a_new_turn_row() {
        let mut projector = Projector::new();
        projector.apply(&snapshot(json!([]), "draft", json!([])));
        assert!(projector.apply(&deltas(json!([{"op": "state.updated", "patch": {"control": {"phase": "completedSuccess"}}}]))).is_empty(),
            "a terminal phase without observed activity is not this turn's end");
        projector.apply(&deltas(json!([{"op": "state.updated", "patch": {"control": {"phase": "running"}}}])));
        let events = projector.apply(&deltas(json!([{"op": "state.updated", "patch": {"control": {"phase": "completedInterrupted"}}}])));
        assert_eq!(events, vec![StreamEvent::Complete]);
        assert_eq!(tool_status("inputStreaming"), "pending");
        assert_eq!(tool_status("cancelled"), "stopped");
    }
}
