//! legacy 会话事件面 `onDynamicSessionEvent`（docs/specs/rust-host-agent.md §2.6；
//! zcodeAgentService.ts onDynamicSessionEvent、zcodeSessionEventCoalescer.ts）。
//!
//! ```text
//! chat CLI ─ session/event / state.updated / 权限·提问反向请求 ─► 通知总线 ─► SessionEvents
//!    ├ seq<=0 的旧事件按 eventId 补号；同一 eventId 的 live 事件只发一次
//!    └► 共享 hub（按 workspaceKey、sessionId）
//! 监听者：订阅 hub（先暂存）─► session/subscribe（失败退避重试 8 次）
//!        ─► replay（只给自己，按 seq、timestamp）─► snapshot ─► 放行暂存 ─► live
//!        每个监听者按 eventId 去重；background-summary 模式经合并器降频
//! ```
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use zcode_host_kit::{EventHub, EventStream, Overflow, Reply, RpcError, log};

use super::extensions::AgentNotification;
use super::runtime::{AgentRuntime, Lane, RuntimePolicy, WorkspaceTarget};

const MAX_TRACKED_EVENT_IDS: usize = 10_000;
const RETRY_BASE: Duration = Duration::from_millis(500);
const RETRY_MAX: Duration = Duration::from_secs(5);
const MAX_ATTEMPTS: u32 = 8;
const COALESCE_DEFAULT: Duration = Duration::from_millis(1_500);
const COALESCE_MAX_ITEMS: usize = 96;
/// 每个 legacy 监听的出口队列（条），满了结束监听。
const LISTENER_QUEUE: usize = 4096;

/// 一条共享事件：`session_id` 为 None 表示发给该 workspace 的全部会话（无 sessionId 的 state.updated）。
#[derive(Debug)]
struct Item {
    workspace_key: String,
    session_id: Option<String>,
    event: Value,
}

/// 有界的 eventId 集合（rememberBoundedEventId）。
#[derive(Default)]
struct BoundedIds {
    ids: HashSet<String>,
    order: VecDeque<String>,
}

impl BoundedIds {
    fn remember(&mut self, id: &str) -> bool {
        if !self.ids.insert(id.to_owned()) {
            return false;
        }
        self.order.push_back(id.to_owned());
        while self.order.len() > MAX_TRACKED_EVENT_IDS {
            if let Some(removed) = self.order.pop_front() {
                self.ids.remove(&removed);
            }
        }
        true
    }
}

#[derive(Default)]
struct SeqState {
    assigned: HashMap<String, u64>,
    assigned_order: VecDeque<String>,
    live: BoundedIds,
    last: u64,
}

#[derive(Default)]
struct State {
    seq: HashMap<String, SeqState>,
    /// 已广播过的权限 / 提问请求（CLI 会用新 id 重发同一业务 requestId）。
    requests: HashSet<String>,
}

pub struct SessionEvents {
    runtime: AgentRuntime,
    hub: EventHub<Arc<Item>>,
    state: Mutex<State>,
}

fn session_key(workspace_key: &str, session_id: &str) -> String {
    format!("{workspace_key}\u{0}{session_id}")
}

/// zcodeSessionEventSchema 的必需字段。
fn is_session_event(event: &Value) -> bool {
    event["eventId"].as_str().is_some_and(|id| !id.is_empty())
        && event["sessionId"].as_str().is_some_and(|id| !id.is_empty())
        && event["type"].is_string()
        && event["seq"].as_f64().is_some_and(f64::is_finite)
        && event["timestamp"].as_f64().is_some_and(f64::is_finite)
}

impl SessionEvents {
    pub fn start(runtime: AgentRuntime) -> Arc<Self> {
        let events = Arc::new(Self {
            runtime: runtime.clone(),
            // legacy `zcode-session` 事件，订阅者是 Renderer 订阅（Gateway 立即搬进出站队列）；流式输出时每秒
            // 数百条。落后 8192 条说明转发卡住，结束它的订阅（docs/specs/rust-u2-task-scope.md §6.2）。
            hub: EventHub::new("legacy-session-events", 8192, Overflow::CloseSubscriber),
            state: Mutex::new(State::default()),
        });
        let mut notifications = Box::pin(runtime.extensions().notifications().subscribe());
        let mut lifecycle = Box::pin(runtime.lifecycle().subscribe());
        let weak = Arc::downgrade(&events);
        drop(
            runtime
                .spawner()
                .spawn_resident("legacy-session-events", async move {
                    loop {
                        tokio::select! {
                            notification = notifications.next() => {
                                let Some(notification) = notification else { return };
                                let Some(events) = weak.upgrade() else { return };
                                events.accept(&notification);
                            }
                            event = lifecycle.next() => {
                                let Some(event) = event else { return };
                                let Some(events) = weak.upgrade() else { return };
                                if event["state"] == "unavailable"
                                    && let Some(key) = event["workspaceKey"].as_str()
                                {
                                    events.forget_requests(key);
                                }
                            }
                        }
                    }
                }),
        );
        events
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn forget_requests(&self, workspace_key: &str) {
        let prefix = format!("{workspace_key}\u{0}");
        self.lock().requests.retain(|key| !key.starts_with(&prefix));
    }

    /// normalizeSessionEventSeq：旧 live sink 发来的 seq=0 事件按 eventId 补号。
    fn normalize(&self, workspace_key: &str, mut event: Value) -> Value {
        let session_id = event["sessionId"].as_str().unwrap_or_default().to_owned();
        let mut state = self.lock();
        let seq = state
            .seq
            .entry(session_key(workspace_key, &session_id))
            .or_default();
        let current = event["seq"].as_f64().unwrap_or(0.0);
        if current > 0.0 {
            seq.last = seq.last.max(current as u64);
            return event;
        }
        let event_id = event["eventId"].as_str().unwrap_or_default().to_owned();
        if let Some(assigned) = seq.assigned.get(&event_id) {
            event["seq"] = json!(assigned);
            return event;
        }
        seq.last += 1;
        let next = seq.last;
        seq.assigned.insert(event_id.clone(), next);
        seq.assigned_order.push_back(event_id);
        while seq.assigned_order.len() > MAX_TRACKED_EVENT_IDS {
            if let Some(removed) = seq.assigned_order.pop_front() {
                seq.assigned.remove(&removed);
            }
        }
        event["seq"] = json!(next);
        event
    }

    fn emit(&self, workspace_key: &str, session_id: Option<&str>, event: Value) {
        self.hub.emit(Arc::new(Item {
            workspace_key: workspace_key.to_owned(),
            session_id: session_id.map(str::to_owned),
            event,
        }));
    }

    fn accept(&self, notification: &AgentNotification) {
        if notification.lane != Lane::Chat {
            return;
        }
        let key = &notification.workspace_key;
        let params = &notification.params;
        match (notification.request, notification.method.as_str()) {
            (false, "session/event") => {
                if !is_session_event(params) {
                    log::warn(format!(
                        "invalid ZCode Protocol session event dropped workspace={key}"
                    ));
                    return;
                }
                let event = self.normalize(key, params.clone());
                let session_id = event["sessionId"].as_str().unwrap_or_default().to_owned();
                let event_id = event["eventId"].as_str().unwrap_or_default().to_owned();
                let first = self
                    .lock()
                    .seq
                    .entry(session_key(key, &session_id))
                    .or_default()
                    .live
                    .remember(&event_id);
                if first {
                    self.emit(
                        key,
                        Some(&session_id),
                        json!({"type": "session.event", "event": event}),
                    );
                }
            }
            (false, "state.updated") if params.is_object() => {
                let event = json!({"type": "state.updated", "notification": params});
                match params["sessionId"].as_str().filter(|id| !id.is_empty()) {
                    Some(session_id) => self.emit(key, Some(session_id), event),
                    None => self.emit(key, None, event),
                }
            }
            (true, method @ ("interaction/requestPermission" | "interaction/requestUserInput")) => {
                let (Some(session_id), Some(request_id)) =
                    (params["sessionId"].as_str(), params["requestId"].as_str())
                else {
                    return;
                };
                let request_key = format!(
                    "{}\u{0}{method}\u{0}{request_id}",
                    session_key(key, session_id)
                );
                if !self.lock().requests.insert(request_key) {
                    return;
                }
                let kind = if method == "interaction/requestPermission" {
                    "permission.request"
                } else {
                    "userInput.request"
                };
                self.emit(
                    key,
                    Some(session_id),
                    json!({"type": kind, "request": params}),
                );
            }
            _ => {}
        }
    }

    /// 一个监听者：`params` 是 ZCodeAgentSessionSubscribeParams。
    pub fn listen(self: &Arc<Self>, params: Value) -> Result<EventStream, RpcError> {
        let target = WorkspaceTarget::from_value(&params)?;
        let session_id = params["sessionId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| zcode_host_kit::args::invalid_argument("sessionId", "is required"))?
            .to_owned();
        let workspace_key = target.key();
        let live = {
            let session_id = session_id.clone();
            self.hub.subscribe_filtered(move |item| {
                item.workspace_key == workspace_key
                    && item.session_id.as_deref().is_none_or(|id| id == session_id)
            })
        };
        let coalesce = (params["eventCoalescing"]["mode"] == "background-summary").then(|| {
            params["eventCoalescing"]["intervalMs"]
                .as_f64()
                .filter(|ms| ms.is_finite() && *ms >= 0.0)
                .map(|ms| Duration::from_millis(ms as u64))
                .unwrap_or(COALESCE_DEFAULT)
        });
        // 订阅者是 Renderer 的 legacy 会话订阅（Gateway 立即搬进出站队列）。监听任务不能为一个读得慢的
        // 订阅停下（它同时在暂存 live 事件），落后 LISTENER_QUEUE 条时结束这个监听，已入队的照常送达
        // （docs/specs/rust-u2-task-scope.md §6.2）。
        let (sender, receiver) = mpsc::channel(LISTENER_QUEUE);
        let listener = Listener {
            events: self.clone(),
            target,
            session_id,
            params,
            sender,
            overflowed: false,
            delivered: BoundedIds::default(),
            coalescer: coalesce.map(Coalescer::new),
        };
        // 监听随返回的事件流结束（订阅方丢弃流时 sender 关闭）；在 agent 的作用域里运行。
        drop(
            self.runtime
                .spawner()
                .spawn("legacy-session-listener", listener.run(Box::pin(live))),
        );
        Ok(Box::pin(futures_util::stream::unfold(
            receiver,
            |mut receiver| async move {
                receiver
                    .recv()
                    .await
                    .map(|value| (Reply::Value(value), receiver))
            },
        )))
    }
}

struct Listener {
    events: Arc<SessionEvents>,
    target: WorkspaceTarget,
    session_id: String,
    params: Value,
    sender: mpsc::Sender<Value>,
    /// 订阅方落后 LISTENER_QUEUE 条：监听结束。
    overflowed: bool,
    delivered: BoundedIds,
    coalescer: Option<Coalescer>,
}

type LiveStream = std::pin::Pin<Box<dyn futures_util::Stream<Item = Arc<Item>> + Send>>;

impl Listener {
    async fn subscribe_once(&self) -> Result<Value, RpcError> {
        let process = self
            .events
            .runtime
            .get_client(&self.target, RuntimePolicy::StartIfNeeded)
            .await?;
        let request = json!({
            "sessionId": self.session_id,
            "deliveryKind": self.params["deliveryKind"],
            "afterSeq": self.params["afterSeq"],
            "includeSnapshot": self.params["includeSnapshot"].as_bool().unwrap_or(false),
        });
        let mut request = request;
        if let Some(object) = request.as_object_mut() {
            object.retain(|_, value| !value.is_null());
        }
        process.request_value("session/subscribe", &request).await
    }

    async fn run(mut self, mut live: LiveStream) {
        let mut buffered: Vec<Value> = Vec::new();
        let mut attempt = 0;
        // 订阅建立期间：暂存 live 事件，失败按指数退避重试。
        loop {
            let outcome = {
                let subscribe = self.subscribe_once();
                tokio::pin!(subscribe);
                loop {
                    tokio::select! {
                        result = &mut subscribe => break Some(result),
                        item = live.next() => match item {
                            Some(item) => buffered.push(item.event.clone()),
                            None => return,
                        },
                        _ = self.sender.closed() => return,
                    }
                }
            };
            match outcome {
                Some(Ok(result)) => {
                    let mut replay: Vec<Value> = result["events"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(is_session_event)
                        .map(|event| self.events.normalize(&self.target.key(), event))
                        .collect();
                    replay.sort_by(|a, b| {
                        let key = |event: &Value| {
                            (
                                event["seq"].as_f64().unwrap_or(0.0),
                                event["timestamp"].as_f64().unwrap_or(0.0),
                            )
                        };
                        key(a)
                            .partial_cmp(&key(b))
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                    // replay 只给本监听者：广播出去会把历史事件插回其他 live 订阅者的时间线。
                    for event in replay {
                        self.deliver(json!({"type": "session.event", "event": event}));
                    }
                    if let Some(snapshot) = result.get("snapshot").filter(|s| !s.is_null()) {
                        self.deliver(json!({"type": "snapshot", "snapshot": snapshot}));
                    }
                    if self.overflowed {
                        return;
                    }
                    break;
                }
                Some(Err(error)) => {
                    if attempt + 1 >= MAX_ATTEMPTS {
                        log::warn(format!(
                            "session subscription failed after {} attempts session={} workspace={}: {}",
                            attempt + 1,
                            self.session_id,
                            self.target.key(),
                            error.message
                        ));
                        break;
                    }
                    let delay = (RETRY_BASE * 2u32.pow(attempt)).min(RETRY_MAX);
                    attempt += 1;
                    let sleep = tokio::time::sleep(delay);
                    tokio::pin!(sleep);
                    loop {
                        tokio::select! {
                            _ = &mut sleep => break,
                            item = live.next() => match item {
                                Some(item) => buffered.push(item.event.clone()),
                                None => return,
                            },
                            _ = self.sender.closed() => return,
                        }
                    }
                }
                None => return,
            }
        }
        for event in std::mem::take(&mut buffered) {
            self.deliver(event);
        }
        while !self.overflowed {
            let deadline = self.coalescer.as_ref().and_then(Coalescer::deadline);
            tokio::select! {
                item = live.next() => match item {
                    Some(item) => self.deliver(item.event.clone()),
                    None => break,
                },
                _ = sleep_until(deadline) => self.flush(),
                _ = self.sender.closed() => break,
            }
        }
        // 取消时先冲刷：待合并的 delta 仍是用户可见正文。
        self.flush();
    }

    fn deliver(&mut self, event: Value) {
        if event["type"] == "session.event" {
            let id = event["event"]["eventId"].as_str().unwrap_or_default();
            if !self.delivered.remember(id) {
                return;
            }
        }
        let ready = match &mut self.coalescer {
            Some(coalescer) => coalescer.accept(event),
            None => vec![event],
        };
        for event in ready {
            self.push(event);
        }
    }

    fn flush(&mut self) {
        let ready = match &mut self.coalescer {
            Some(coalescer) => coalescer.flush(),
            None => Vec::new(),
        };
        for event in ready {
            self.push(event);
        }
    }

    fn push(&mut self, event: Value) {
        if self.overflowed {
            return;
        }
        if let Err(mpsc::error::TrySendError::Full(_)) = self.sender.try_send(event) {
            self.overflowed = true;
            log::warn(format!(
                "legacy session listener fell {LISTENER_QUEUE} events behind; ending it session={} workspace={}",
                self.session_id,
                self.target.key()
            ));
        }
    }
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// createBackgroundSessionEventCoalescer：可重组的增量按 key 合并，其他事件先冲刷再立即投递。
struct Coalescer {
    delay: Duration,
    pending: Vec<(String, Value)>,
    deadline: Option<tokio::time::Instant>,
}

impl Coalescer {
    fn new(delay: Duration) -> Self {
        Self {
            delay,
            pending: Vec::new(),
            deadline: None,
        }
    }

    fn deadline(&self) -> Option<tokio::time::Instant> {
        self.deadline
    }

    fn flush(&mut self) -> Vec<Value> {
        self.deadline = None;
        std::mem::take(&mut self.pending)
            .into_iter()
            .map(|(_, event)| event)
            .collect()
    }

    fn accept(&mut self, event: Value) -> Vec<Value> {
        let key = (event["type"] == "session.event")
            .then(|| coalesce_key(&event["event"]))
            .flatten();
        let Some(key) = key else {
            let mut out = self.flush();
            out.push(event);
            return out;
        };
        if let Some((_, existing)) = self.pending.iter_mut().find(|(k, _)| *k == key) {
            let merged = merge(&existing["event"], &event["event"]);
            *existing = json!({"type": "session.event", "event": merged});
        } else {
            self.pending.push((key, event));
        }
        if self.pending.len() >= COALESCE_MAX_ITEMS || self.delay.is_zero() {
            return self.flush();
        }
        if self.deadline.is_none() {
            self.deadline = Some(tokio::time::Instant::now() + self.delay);
        }
        Vec::new()
    }
}

fn field<'a>(payload: &'a Value, key: &str) -> &'a str {
    payload[key].as_str().unwrap_or_default()
}

/// getBackgroundSessionEventCoalesceKey。
fn coalesce_key(event: &Value) -> Option<String> {
    let payload = if event["payload"].is_object() {
        &event["payload"]
    } else {
        &Value::Null
    };
    let session = field(event, "sessionId");
    let turn = field(event, "turnId");
    let parts: Vec<&str> = match event["type"].as_str()? {
        "model.streaming" => {
            let kind = payload["kind"].as_str()?;
            if !matches!(kind, "text_delta" | "reasoning_delta" | "tool_input_delta")
                || !payload["delta"].is_string()
            {
                return None;
            }
            vec![
                "model.streaming",
                session,
                turn,
                kind,
                field(payload, "inputId"),
                field(payload, "assistantMessageId"),
                field(payload, "toolCallId"),
                field(payload, "parentToolUseId"),
                field(payload, "parentToolCallId"),
            ]
        }
        "tool.updated" if payload["kind"] == "progress" => vec![
            "tool.updated",
            session,
            turn,
            field(payload, "inputId"),
            field(payload, "toolCallId"),
        ],
        "streamRecovery.updated" => vec![
            "streamRecovery.updated",
            session,
            turn,
            field(payload, "inputId"),
            field(event, "traceId"),
        ],
        _ => return None,
    };
    Some(parts.join("\u{0}"))
}

/// mergeBackgroundSessionEvents：delta 拼接，其余保留最新。
fn merge(current: &Value, next: &Value) -> Value {
    if current["type"] == "model.streaming" && next["type"] == "model.streaming" {
        let mut merged = next.clone();
        let delta = format!(
            "{}{}",
            field(&current["payload"], "delta"),
            field(&next["payload"], "delta")
        );
        merged["payload"]["delta"] = json!(delta);
        return merged;
    }
    next.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn streaming(id: &str, delta: &str) -> Value {
        json!({"type": "session.event", "event": {"eventId": id, "sessionId": "s", "type": "model.streaming", "seq": 1, "timestamp": 1,
            "payload": {"kind": "text_delta", "delta": delta, "assistantMessageId": "m"}}})
    }

    #[test]
    fn coalescer_merges_deltas_and_flushes_on_structural_events() {
        let mut coalescer = Coalescer::new(Duration::from_secs(1));
        assert!(coalescer.accept(streaming("1", "a")).is_empty());
        assert!(coalescer.accept(streaming("2", "b")).is_empty());
        assert!(coalescer.deadline().is_some());
        let terminal = json!({"type": "session.event", "event": {"eventId": "3", "sessionId": "s", "type": "turn.completed", "seq": 3, "timestamp": 1, "payload": {}}});
        let out = coalescer.accept(terminal.clone());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["event"]["payload"]["delta"], "ab");
        assert_eq!(out[0]["event"]["eventId"], "2");
        assert_eq!(out[1], terminal);
        assert!(coalescer.deadline().is_none());
        let snapshot = json!({"type": "snapshot", "snapshot": {}});
        coalescer.accept(streaming("4", "c"));
        assert_eq!(coalescer.accept(snapshot.clone()).last(), Some(&snapshot));
    }

    #[test]
    fn coalesce_keys_follow_typescript() {
        let event = |kind: &str, payload: Value| json!({"type": kind, "sessionId": "s", "turnId": "t", "payload": payload});
        assert!(
            coalesce_key(&event(
                "model.streaming",
                json!({"kind": "text_delta", "delta": "x"})
            ))
            .is_some()
        );
        assert!(coalesce_key(&event("model.streaming", json!({"kind": "text_delta"}))).is_none());
        assert!(
            coalesce_key(&event(
                "model.streaming",
                json!({"kind": "tool_result", "delta": "x"})
            ))
            .is_none()
        );
        assert!(coalesce_key(&event("tool.updated", json!({"kind": "progress"}))).is_some());
        assert!(coalesce_key(&event("tool.updated", json!({"kind": "result"}))).is_none());
        assert!(coalesce_key(&event("streamRecovery.updated", json!({}))).is_some());
    }

    fn runtime() -> AgentRuntime {
        AgentRuntime::new(
            &zcode_host_base::testing::spawner(),
            crate::agent::AgentRuntimeConfig {
                base_env: Default::default(),
                command_vars: Default::default(),
                fallback_cwd: None,
            },
            crate::settings::SettingStore::new(
                &zcode_host_base::testing::spawner(),
                std::path::Path::new("/nonexistent"),
            ),
            crate::providers::ProviderRuntime::new(
                &zcode_host_base::testing::spawner(),
                None,
                "/nonexistent/p.json".into(),
            ),
        )
    }

    fn notification(method: &str, params: Value, request: bool) -> AgentNotification {
        AgentNotification {
            target: WorkspaceTarget::local("/w"),
            workspace_key: "/w".into(),
            generation: 1,
            lane: Lane::Chat,
            method: method.into(),
            params,
            request,
        }
    }

    #[tokio::test]
    async fn live_events_are_renumbered_deduplicated_and_requests_emitted_once() {
        let events = SessionEvents::start(runtime());
        let mut hub = Box::pin(events.hub.subscribe());
        let event = |id: &str, seq: u64| json!({"eventId": id, "sessionId": "s", "type": "x", "seq": seq, "timestamp": 1});
        events.accept(&notification("session/event", event("e5", 5), false));
        events.accept(&notification("session/event", event("e0", 0), false));
        events.accept(&notification("session/event", event("e0", 0), false));
        events.accept(&notification(
            "session/event",
            json!({"eventId": "bad"}),
            false,
        ));
        events.accept(&notification(
            "state.updated",
            json!({"revision": 2}),
            false,
        ));
        let permission = json!({"sessionId": "s", "requestId": "r1"});
        events.accept(&notification(
            "interaction/requestPermission",
            permission.clone(),
            true,
        ));
        events.accept(&notification(
            "interaction/requestPermission",
            permission.clone(),
            true,
        ));
        let first = hub.next().await.unwrap();
        assert_eq!(first.event["event"]["seq"], 5);
        let second = hub.next().await.unwrap();
        assert_eq!(
            second.event["event"]["seq"], 6,
            "seq<=0 continues after the last seen seq"
        );
        let state = hub.next().await.unwrap();
        assert_eq!(state.event["type"], "state.updated");
        assert!(state.session_id.is_none(), "workspace-wide state update");
        let request = hub.next().await.unwrap();
        assert_eq!(
            request.event,
            json!({"type": "permission.request", "request": permission})
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), hub.next())
                .await
                .is_err(),
            "duplicates are dropped"
        );
    }

    #[test]
    fn bounded_ids_forget_the_oldest() {
        let mut ids = BoundedIds::default();
        assert!(ids.remember("a"));
        assert!(!ids.remember("a"));
        for index in 0..MAX_TRACKED_EVENT_IDS {
            ids.remember(&index.to_string());
        }
        assert!(ids.remember("a"), "oldest id was evicted");
    }
}
