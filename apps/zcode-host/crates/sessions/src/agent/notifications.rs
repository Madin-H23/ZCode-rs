//! CLI 通知的分发（docs/specs/rust-host-agent.md §2.9；zcodeAgentService.ts wireClient 的通知部分）。
//!
//! ```text
//! 任一 lane 的 CLI 通知 ─► Extensions.notifications ─► AgentEvents（单任务，按到达顺序）
//!    ├► process/resourceSample、mcpTelemetry、mcpResourceSamples、toolExecResource ─► Main 控制消息
//!    ├► plugins/operationProgress ─► 按 operationId 分发
//!    └► v4/telemetry/local-ttft、v4/telemetry/event、v4/cua/permission-observation
//!          ─► strict 校验（agent/schema.rs）─► 按 workspace 分发 ─► connection scope 过滤 ─► Renderer
//! ```
use std::sync::Arc;

use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use zcode_host_kit::{EventHub, EventStream, Overflow, Reply, log};

use super::extensions::{AgentNotification, Extensions};
use super::schema;
use crate::main_bridge::MainBridge;

/// 一条按 workspaceKey 分发的事实。
#[derive(Debug, Clone)]
pub struct KeyedFact {
    pub workspace_key: String,
    pub value: Value,
}

pub struct AgentEvents {
    plugin_progress: EventHub<Arc<(String, Value)>>,
    local_ttft: EventHub<Arc<KeyedFact>>,
    telemetry: EventHub<Arc<KeyedFact>>,
    cua: EventHub<Value>,
}

impl Default for AgentEvents {
    /// 四个 hub 的订阅者都是 Renderer 订阅（Gateway 立即搬进出站队列）：落后 1024 条说明转发卡住，
    /// 结束它的订阅（docs/specs/rust-u2-task-scope.md §6.2）。
    fn default() -> Self {
        Self {
            plugin_progress: EventHub::new(
                "agent-plugin-progress",
                1024,
                Overflow::CloseSubscriber,
            ),
            local_ttft: EventHub::new("agent-local-ttft", 1024, Overflow::CloseSubscriber),
            telemetry: EventHub::new("agent-telemetry", 1024, Overflow::CloseSubscriber),
            cua: EventHub::new("agent-cua", 1024, Overflow::CloseSubscriber),
        }
    }
}

impl AgentEvents {
    /// 订阅通知总线并开始分发；`main` 为 None 时（单测）不转发 Main 控制消息。
    pub fn start(extensions: &Extensions, main: Option<MainBridge>) -> Arc<Self> {
        let events = Arc::new(Self::default());
        let mut notifications = Box::pin(extensions.notifications().subscribe());
        let weak = Arc::downgrade(&events);
        tokio::spawn(async move {
            while let Some(notification) = notifications.next().await {
                let Some(events) = weak.upgrade() else { return };
                events.dispatch(&notification, main.as_ref());
            }
        });
        events
    }

    pub fn dispatch(&self, notification: &AgentNotification, main: Option<&MainBridge>) {
        if notification.request {
            return;
        }
        let params = &notification.params;
        match notification.method.as_str() {
            "process/resourceSample" => {
                // CLI 样本自带 lane 会被协议 schema 拒绝；lane 只能由 Host 按所属进程补上。
                let Some(sample) = params.as_object().filter(|map| !map.contains_key("lane"))
                else {
                    log::debug("invalid CLI resource sample dropped");
                    return;
                };
                let mut sample = sample.clone();
                sample.insert("lane".into(), json!(notification.lane.sample_label()));
                post(
                    main,
                    json!({"type": "agent-resource-sample", "runtimeSurface": "local", "sample": sample}),
                );
            }
            "process/mcpTelemetry" if params.is_object() => post(
                main,
                json!({"type": "mcp-telemetry", "runtimeSurface": "local", "event": params}),
            ),
            "process/mcpResourceSamples" if params.is_array() => post(
                main,
                json!({"type": "mcp-resource-samples", "runtimeSurface": "local", "samples": params}),
            ),
            "process/toolExecResource" if params.is_object() => post(
                main,
                json!({"type": "tool-exec-resource", "runtimeSurface": "local", "sample": params}),
            ),
            "plugins/operationProgress" => match params["operationId"].as_str() {
                Some(operation_id) if !operation_id.is_empty() => self
                    .plugin_progress
                    .emit(Arc::new((operation_id.to_owned(), params.clone()))),
                _ => log::warn("invalid plugin operation progress dropped"),
            },
            "v4/telemetry/local-ttft" => {
                let target = &notification.target;
                let local = target.remote_session_id.is_none()
                    && target
                        .workspace_identity
                        .as_deref()
                        .is_none_or(|identity| identity.trim().is_empty());
                if local && schema::is_local_ttft_facts(params) {
                    self.local_ttft.emit(Arc::new(KeyedFact {
                        workspace_key: notification.workspace_key.clone(),
                        value: params.clone(),
                    }));
                }
            }
            "v4/telemetry/event" => {
                if schema::is_conversation_telemetry_fact(params) {
                    self.telemetry.emit(Arc::new(KeyedFact {
                        workspace_key: notification.workspace_key.clone(),
                        value: params.clone(),
                    }));
                } else {
                    // 严格丢弃未知字段，避免 CLI 新字段未经审计穿透到 renderer reporter。
                    log::warn(format!(
                        "invalid v4 conversation telemetry fact dropped workspace={}",
                        notification.workspace_key
                    ));
                }
            }
            "v4/cua/permission-observation" => {
                if !schema::is_cua_permission_observation(params) {
                    log::warn(format!(
                        "invalid v4 CUA permission observation dropped workspace={}",
                        notification.workspace_key
                    ));
                    return;
                }
                if !notification.target.is_local() {
                    return;
                }
                let mut observation: Map<String, Value> =
                    params.as_object().cloned().unwrap_or_default();
                observation.insert(
                    "workspacePath".into(),
                    json!(notification.target.workspace_path),
                );
                if let Some(identity) = notification
                    .target
                    .workspace_identity
                    .as_deref()
                    .filter(|identity| !identity.is_empty())
                {
                    observation.insert("workspaceIdentity".into(), json!(identity));
                }
                self.cua.emit(Value::Object(observation));
            }
            other => log::debug(format!(
                "agent[{}] notification not forwarded: {other}",
                notification.workspace_key
            )),
        }
    }

    /// `onDynamicPluginOperationProgress(operationId)`。
    pub fn plugin_progress(&self, operation_id: String) -> EventStream {
        let events = self
            .plugin_progress
            .subscribe_filtered(move |entry| entry.0 == operation_id)
            .map(|entry| Reply::Value(entry.1.clone()));
        Box::pin(events)
    }

    pub fn local_ttft(&self, workspace_key: String) -> EventStream {
        keyed(&self.local_ttft, workspace_key)
    }

    pub fn telemetry(&self, workspace_key: String) -> EventStream {
        keyed(&self.telemetry, workspace_key)
    }

    pub fn cua_observations(&self) -> EventStream {
        Box::pin(self.cua.subscribe().map(Reply::Value))
    }
}

fn keyed(hub: &EventHub<Arc<KeyedFact>>, workspace_key: String) -> EventStream {
    Box::pin(
        hub.subscribe_filtered(move |fact| fact.workspace_key == workspace_key)
            .map(|fact| Reply::Value(fact.value.clone())),
    )
}

fn post(main: Option<&MainBridge>, message: Value) {
    // Main 退出或通道关闭时只丢当前样本，不影响通知分发。
    if let Some(main) = main {
        main.post(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Lane, WorkspaceTarget};

    fn notification(
        target: WorkspaceTarget,
        lane: Lane,
        method: &str,
        params: Value,
    ) -> AgentNotification {
        AgentNotification {
            workspace_key: target.key(),
            target,
            generation: 1,
            lane,
            method: method.into(),
            params,
            request: false,
        }
    }

    #[derive(Default)]
    struct Sink(std::sync::Mutex<Vec<Value>>);

    impl zcode_host_control::ControlSink for Sink {
        fn send(&self, message: zcode_host_control::HostToMain) {
            if let zcode_host_control::HostToMain::Raw(value) = message {
                self.0.lock().unwrap().push(value);
            }
        }
    }

    #[test]
    fn resource_facts_go_to_main_with_lane_labels() {
        let sink = Arc::new(Sink::default());
        let main = MainBridge::new(sink.clone());
        let events = AgentEvents::default();
        let target = WorkspaceTarget::local("/w");
        let sample = json!({"cpuPercent": 1});
        events.dispatch(
            &notification(
                target.clone(),
                Lane::McpStatus,
                "process/resourceSample",
                sample,
            ),
            Some(&main),
        );
        events.dispatch(
            &notification(
                target.clone(),
                Lane::Chat,
                "process/resourceSample",
                json!({"lane": "x"}),
            ),
            Some(&main),
        );
        events.dispatch(
            &notification(
                target.clone(),
                Lane::Chat,
                "process/mcpTelemetry",
                json!({"kind": "spawn"}),
            ),
            Some(&main),
        );
        events.dispatch(
            &notification(
                target.clone(),
                Lane::Chat,
                "process/mcpResourceSamples",
                json!([]),
            ),
            Some(&main),
        );
        events.dispatch(
            &notification(
                target,
                Lane::PluginManagement,
                "process/toolExecResource",
                json!({"ms": 1}),
            ),
            Some(&main),
        );
        let sent = sink.0.lock().unwrap().clone();
        assert_eq!(
            sent,
            vec![
                json!({"type": "agent-resource-sample", "runtimeSurface": "local", "sample": {"cpuPercent": 1, "lane": "mcp-status"}}),
                json!({"type": "mcp-telemetry", "runtimeSurface": "local", "event": {"kind": "spawn"}}),
                json!({"type": "mcp-resource-samples", "runtimeSurface": "local", "samples": []}),
                json!({"type": "tool-exec-resource", "runtimeSurface": "local", "sample": {"ms": 1}}),
            ]
        );
    }

    #[tokio::test]
    async fn routes_progress_ttft_and_cua_with_scope_rules() {
        let events = AgentEvents::default();
        let mut progress = events.plugin_progress("op-1".into());
        let mut ttft = events.local_ttft("/w".into());
        let mut cua = events.cua_observations();
        let local = WorkspaceTarget::local("/w");
        let mut remote = WorkspaceTarget::local("/w");
        remote.workspace_identity = Some("remote:ssh:h:22:u:/w".into());

        events.dispatch(
            &notification(
                local.clone(),
                Lane::PluginManagement,
                "plugins/operationProgress",
                json!({"operationId": "op-2"}),
            ),
            None,
        );
        events.dispatch(
            &notification(
                local.clone(),
                Lane::PluginManagement,
                "plugins/operationProgress",
                json!({"operationId": "op-1", "stage": "x"}),
            ),
            None,
        );
        assert_eq!(
            progress.next().await,
            Some(Reply::Value(json!({"operationId": "op-1", "stage": "x"})))
        );

        let facts = json!({"version": 1, "observationId": "123e4567-e89b-42d3-a456-426614174000", "instanceId": "i", "commandId": "c", "receivedAt": 1});
        events.dispatch(
            &notification(
                remote.clone(),
                Lane::Chat,
                "v4/telemetry/local-ttft",
                facts.clone(),
            ),
            None,
        );
        let mut invalid = facts.clone();
        invalid["extra"] = json!(1);
        events.dispatch(
            &notification(
                local.clone(),
                Lane::Chat,
                "v4/telemetry/local-ttft",
                invalid,
            ),
            None,
        );
        events.dispatch(
            &notification(
                local.clone(),
                Lane::Chat,
                "v4/telemetry/local-ttft",
                facts.clone(),
            ),
            None,
        );
        assert_eq!(ttft.next().await, Some(Reply::Value(facts)));

        let observation = json!({"schemaVersion": 1, "eventId": "e", "eventSeq": 0, "occurredAt": 1, "sessionId": "s", "toolCallId": "t",
            "permissionStatus": {"schemaVersion": 1, "platform": "darwin", "grantOwner": "g", "accessibility": "granted", "screenRecording": "denied"}});
        events.dispatch(
            &notification(
                remote,
                Lane::Chat,
                "v4/cua/permission-observation",
                observation.clone(),
            ),
            None,
        );
        events.dispatch(
            &notification(
                local,
                Lane::Chat,
                "v4/cua/permission-observation",
                observation.clone(),
            ),
            None,
        );
        let mut expected = observation;
        expected["workspacePath"] = json!("/w");
        assert_eq!(cua.next().await, Some(Reply::Value(expected)));
    }
}
