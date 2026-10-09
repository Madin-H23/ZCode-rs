//! task realtime bridge（packages/desktop/src/host/taskRealtimeBridge.ts）：Host 与 Main
//! `taskRealtimeBus` 之间的 owner / lease、stream op 与 owner 命令通道。
//!
//! 只有 InitLocal 带 `hostId` 时才建立（Main 为窗口 Host 注册 realtime 时下发）。Node Host 里它的
//! 唯一使用方是远端 workspace 的 sendPrompt 镜像（acquire lease → publish stream op → release），
//! 送达的事件与 owner 命令没有订阅者；Rust 保持同样的消息与语义，并把 API 交给远端 workspace 使用。
//! 跨 Host 的会话消息（session-message-deliver）由 zcode-task `deliverSessionMessage` 应答，与 Node
//! 一样回 `failed`（该能力在 adapter 里尚未支持），避免 Main 等满 30 s 超时。
//!
//! ```text
//! 使用方 ─acquire(target)─► task-run-lease-acquire{request+leaseRequestId} ─► Main bus
//!        ◄──────────────── task-run-lease-result{result.leaseRequestId} ◄───┘
//! Main bus ─task-realtime-deliver{event}─► 按 eventId 去重（窗口 1000）─► delivered_events()
//! Main bus ─session-message-deliver{request}─► deliverSessionMessage ─► session-message-deliver-result
//! ```
use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use zcode_host_control::ControlRouter;
use zcode_host_kit::{EventHub, RpcError, log};

use crate::main_bridge::MainBridge;

const SEEN_EVENT_LIMIT: usize = 1000;
/// Main 的 bus 同步应答 lease 请求、owner 命令自带超时；这里只防 Main 失联时永远挂起。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

pub const DELIVER: &str = "task-realtime-deliver";
pub const LEASE_RESULT: &str = "task-run-lease-result";
pub const OWNER_COMMAND_DELIVER: &str = "task-owner-command-deliver";
pub const OWNER_COMMAND_RESULT: &str = "task-owner-command-result";
pub const SESSION_MESSAGE_DELIVER: &str = "session-message-deliver";
pub const SESSION_MESSAGE_DELIVERY_RESULT: &str = "session-message-delivery-result";

pub struct TaskRealtimeBridge {
    host_id: String,
    delivery_kind: String,
    main: MainBridge,
    seen: Mutex<(HashSet<String>, VecDeque<String>)>,
    events: EventHub<Arc<Value>>,
    owner_commands: EventHub<Arc<Value>>,
}

fn disposed() -> RpcError {
    RpcError::new("Error", "task realtime bridge disposed")
}

impl TaskRealtimeBridge {
    /// `createTaskRealtimeBridgeForHostInit`：没有 hostId 时不建立。
    pub fn for_host_init(
        host_id: Option<&str>,
        delivery_kind: Option<&str>,
        main: MainBridge,
    ) -> Option<Arc<Self>> {
        let host_id = host_id.filter(|id| !id.is_empty())?;
        Some(Arc::new(Self {
            host_id: host_id.to_owned(),
            delivery_kind: delivery_kind.unwrap_or("desktop_window").to_owned(),
            main,
            seen: Mutex::new((HashSet::new(), VecDeque::new())),
            events: EventHub::new(),
            owner_commands: EventHub::new(),
        }))
    }

    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    pub fn delivery_kind(&self) -> &str {
        &self.delivery_kind
    }

    /// 登记 Main → Host 的 lease / owner 命令结果与送达事件。
    pub fn register(self: &Arc<Self>, router: &mut ControlRouter) {
        for kind in [LEASE_RESULT, OWNER_COMMAND_RESULT] {
            let bridge = self.clone();
            let id_field = if kind == LEASE_RESULT {
                "leaseRequestId"
            } else {
                "commandRequestId"
            };
            router.on(kind, move |message| {
                if let Some(id) = message["result"][id_field].as_str() {
                    bridge.main.resolve(id, message["result"].clone());
                }
            });
        }
        let bridge = self.clone();
        router.on(DELIVER, move |message| {
            let event = &message["event"];
            if let Some(event_id) = event["eventId"].as_str()
                && bridge.remember_event(event_id)
            {
                bridge.events.emit(Arc::new(event.clone()));
            }
        });
        let bridge = self.clone();
        router.on(OWNER_COMMAND_DELIVER, move |message| {
            if message["command"].is_object() {
                bridge
                    .owner_commands
                    .emit(Arc::new(message["command"].clone()));
            }
        });
    }

    /// 送达事件按 eventId 去重，只保留最近 1000 个。
    fn remember_event(&self, event_id: &str) -> bool {
        let mut seen = self
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if seen.0.contains(event_id) {
            return false;
        }
        seen.0.insert(event_id.to_owned());
        seen.1.push_back(event_id.to_owned());
        while seen.1.len() > SEEN_EVENT_LIMIT {
            if let Some(removed) = seen.1.pop_front() {
                seen.0.remove(&removed);
            }
        }
        true
    }

    pub fn delivered_events(&self) -> &EventHub<Arc<Value>> {
        &self.events
    }

    pub fn owner_commands(&self) -> &EventHub<Arc<Value>> {
        &self.owner_commands
    }

    pub fn publish(&self, event: Value) {
        self.main
            .post(json!({"type": "task-realtime-publish", "event": event}));
    }

    /// `acquireTaskRunLease(target)` → `TaskRunLeaseResult`。
    pub async fn acquire_task_run_lease(&self, target: Value) -> Result<Value, RpcError> {
        let lease_request_id = crate::agent::service::uuid_v4();
        let mut request = target;
        request["leaseRequestId"] = json!(lease_request_id);
        let message = json!({"type": "task-run-lease-acquire", "request": request});
        self.main
            .request(lease_request_id, message, REQUEST_TIMEOUT, |_| {})
            .await
            .ok_or_else(disposed)
    }

    pub fn release_task_run_lease(&self, target: Value) {
        self.main
            .post(json!({"type": "task-run-lease-release", "target": target}));
    }

    pub fn publish_stream_op(&self, target: Value, op: Value) {
        self.main
            .post(json!({"type": "task-stream-op-publish", "target": target, "op": op}));
    }

    /// `requestOwnerCommand(command)` → `TaskOwnerCommandResult`。
    pub async fn request_owner_command(&self, command: Value) -> Result<Value, RpcError> {
        let command_request_id = crate::agent::service::uuid_v4();
        let mut command = command;
        command["commandRequestId"] = json!(command_request_id);
        let message = json!({"type": "task-owner-command-request", "command": command});
        self.main
            .request(command_request_id, message, REQUEST_TIMEOUT, |_| {})
            .await
            .ok_or_else(disposed)
    }

    pub fn publish_owner_command_result(&self, result: Value) {
        self.main
            .post(json!({"type": "task-owner-command-result", "result": result}));
    }
}

/// 跨 Host 会话消息的 Host 侧应答（host/index.ts 的 SessionMessageDeliver 分支）：zcode-task 的
/// `deliverSessionMessage` 在 adapter 里不支持，所以总是回 `failed`，错误文案同 Node。
pub fn register_session_messages(router: &mut ControlRouter, main: MainBridge) {
    router.on(SESSION_MESSAGE_DELIVER, move |message| {
        let request = &message["request"];
        let error = super::lifecycle_members::unsupported("deliverSessionMessage").message;
        main.post(json!({
            "type": "session-message-deliver-result",
            "result": {
                "error": error,
                "messageId": request["messageId"],
                "requestId": request["requestId"],
                "sessionId": request["fromSessionId"],
                "status": "failed",
            },
        }));
    });
    router.on(SESSION_MESSAGE_DELIVERY_RESULT, |_| {
        log::warn(format!(
            "failed to forward session message delivery result: {}",
            super::lifecycle_members::unsupported("sendSessionMessageDeliveryResult").message
        ));
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use futures_util::StreamExt;
    use zcode_host_control::{ControlSink, HostToMain};

    use super::*;

    #[derive(Default)]
    struct Sink(StdMutex<Vec<Value>>);

    impl ControlSink for Sink {
        fn send(&self, message: HostToMain) {
            if let HostToMain::Raw(value) = message {
                self.0.lock().unwrap().push(value);
            }
        }
    }

    #[tokio::test]
    async fn lease_round_trip_dedupe_and_session_messages() {
        let sink = Arc::new(Sink::default());
        let main = MainBridge::new(sink.clone());
        assert!(TaskRealtimeBridge::for_host_init(None, None, main.clone()).is_none());
        let bridge = TaskRealtimeBridge::for_host_init(Some("h1"), None, main.clone()).unwrap();
        assert_eq!(bridge.delivery_kind(), "desktop_window");
        let mut router = ControlRouter::default();
        bridge.register(&mut router);
        register_session_messages(&mut router, main);

        let target = json!({"workspacePath": "/w", "workspaceKey": "/w", "taskId": "t", "runId": "r", "traceId": "r"});
        let acquire = {
            let bridge = bridge.clone();
            tokio::spawn(async move { bridge.acquire_task_run_lease(target).await })
        };
        let request = loop {
            if let Some(sent) = sink.0.lock().unwrap().first().cloned() {
                break sent;
            }
            tokio::task::yield_now().await;
        };
        assert_eq!(request["type"], "task-run-lease-acquire");
        let id = request["request"]["leaseRequestId"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(router.dispatch(
            LEASE_RESULT,
            json!({"result": {"leaseRequestId": id, "acquired": true, "ownerHostId": "h1"}})
        ));
        assert_eq!(acquire.await.unwrap().unwrap()["acquired"], true);

        let mut events = Box::pin(bridge.delivered_events().subscribe());
        router.dispatch(DELIVER, json!({"event": {"eventId": "e1"}}));
        router.dispatch(DELIVER, json!({"event": {"eventId": "e1"}}));
        router.dispatch(DELIVER, json!({"event": {"eventId": "e2"}}));
        assert_eq!(events.next().await.unwrap()["eventId"], "e1");
        assert_eq!(events.next().await.unwrap()["eventId"], "e2");

        router.dispatch(SESSION_MESSAGE_DELIVER, json!({"request": {"messageId": "m", "requestId": "q", "fromSessionId": "s", "toSessionId": "o", "content": "x", "createdAt": 1}}));
        let reply = sink.0.lock().unwrap().last().cloned().unwrap();
        assert_eq!(reply["type"], "session-message-deliver-result");
        assert_eq!(reply["result"]["status"], "failed");
        assert_eq!(reply["result"]["sessionId"], "s");
    }
}
