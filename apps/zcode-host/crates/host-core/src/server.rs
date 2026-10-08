//! 单个 attachment 上的 `@zcode/rpc` 服务端语义（`packages/rpc/src/channelServer.ts`）。
//!
//! 与 Node 版的有意差异：未知 channel 立即报错，不做 1000 ms 等待；订阅失败只记日志
//! （Node 版此时异常逃出消息处理器，同样不会应答）。
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures_util::StreamExt;
use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;
use zcode_host_rpc::codec::{self, RESPONSE_EVENT_FIRE, RESPONSE_PROMISE_SUCCESS};
use zcode_host_rpc::{RequestKind, RpcError, RpcValue, read_request};

use zcode_host_kit::{CallContext, ChannelRegistry, FlowState, Reply, log};

use crate::rpc_telemetry::RpcTelemetry;

type Active = Arc<Mutex<HashMap<i32, AbortHandle>>>;

pub(crate) async fn serve(
    context: CallContext,
    mut ready: watch::Receiver<Option<Arc<ChannelRegistry>>>,
    mut inbound: mpsc::UnboundedReceiver<Vec<u8>>,
    outbound: mpsc::UnboundedSender<Vec<u8>>,
    mut flow: watch::Receiver<FlowState>,
    telemetry: Arc<RpcTelemetry>,
) {
    let registry = match ready.wait_for(Option::is_some).await {
        Ok(registry) => registry.clone(),
        Err(_) => return,
    };
    let Some(registry) = registry else {
        return;
    };
    let _ = outbound.send(codec::initialize());
    log::debug(format!("attachment {} initialized", context.attachment_id));

    let active: Active = Arc::new(Mutex::new(HashMap::new()));
    // 接入层的积压边沿与请求在同一循环里按到达顺序处理；接入方不观测背压时 flow 永不变化。
    let mut flow_open = true;
    loop {
        let message = tokio::select! {
            message = inbound.recv() => match message {
                Some(message) => message,
                None => break,
            },
            changed = flow.changed(), if flow_open => {
                match changed {
                    Ok(()) => {
                        let state = *flow.borrow_and_update();
                        registry.connection_flow(&context, state);
                    }
                    Err(_) => flow_open = false,
                }
                continue;
            }
        };
        let request = match read_request(&message) {
            Ok(request) => request,
            Err(error) => {
                log::warn(format!("dropping malformed rpc message: {error}"));
                continue;
            }
        };
        match request.kind {
            RequestKind::Promise => {
                let args = match request.body {
                    RpcValue::Array(items) => items,
                    RpcValue::Undefined => Vec::new(),
                    other => vec![other],
                };
                let Some(channel) = registry.get(&request.channel) else {
                    let error = unknown_channel(&request.channel);
                    let _ = outbound.send(codec::promise_error(request.id, &error));
                    continue;
                };
                // RPC 网络遥测（NetworkTelemetryChannelServer）：只包已登记的 channel，计时含整个调用。
                let started = Instant::now();
                let call = channel.call(&context, &request.name, args);
                let (id, outbound, tracked) = (request.id, outbound.clone(), active.clone());
                let (telemetry, channel_name, method) = (
                    telemetry.clone(),
                    request.channel.clone(),
                    request.name.clone(),
                );
                let task = tokio::spawn(async move {
                    let result = call.await;
                    telemetry.record(
                        &channel_name,
                        &method,
                        started.elapsed(),
                        result.as_ref().err(),
                    );
                    let response = match result {
                        Ok(reply) => encode_reply(RESPONSE_PROMISE_SUCCESS, id, reply),
                        Err(error) => codec::promise_error(id, &error),
                    };
                    let _ = outbound.send(response);
                    lock(&tracked).remove(&id);
                });
                track(&active, request.id, task.abort_handle());
            }
            RequestKind::EventListen => {
                let Some(channel) = registry.get(&request.channel) else {
                    log::warn(format!("listen on unknown channel {}", request.channel));
                    continue;
                };
                let mut events = match channel.listen(&context, &request.name, request.body) {
                    Ok(events) => events,
                    Err(error) => {
                        log::warn(format!(
                            "listen {}.{} rejected: {}",
                            request.channel, request.name, error.message
                        ));
                        continue;
                    }
                };
                let (id, outbound) = (request.id, outbound.clone());
                let task = tokio::spawn(async move {
                    while let Some(event) = events.next().await {
                        if outbound
                            .send(encode_reply(RESPONSE_EVENT_FIRE, id, event))
                            .is_err()
                        {
                            break;
                        }
                    }
                });
                track(&active, request.id, task.abort_handle());
            }
            RequestKind::PromiseCancel | RequestKind::EventDispose => {
                if let Some(handle) = lock(&active).remove(&request.id) {
                    handle.abort();
                }
            }
        }
    }

    // 连接关闭：取消这条连接上所有进行中的调用和订阅，再让按连接持有状态的 channel 清理。
    for (_, handle) in lock(&active).drain() {
        handle.abort();
    }
    registry.connection_closed(&context);
    log::debug(format!("attachment {} closed", context.attachment_id));
}

fn unknown_channel(channel: &str) -> RpcError {
    RpcError::new(
        "Unknown channel",
        format!("Channel name '{channel}' is not registered in the Rust Host"),
    )
}

fn encode_reply(kind: i32, id: i32, reply: Reply) -> Vec<u8> {
    match reply {
        Reply::Undefined => codec::response_undefined(kind, id),
        Reply::Value(value) => codec::response_value(kind, id, &value),
        Reply::Bytes(bytes) => codec::response_bytes(kind, id, &bytes),
        Reply::Encoded(body) => codec::response_encoded(kind, id, &body),
        Reply::RawJson(raw) => codec::raw_response(kind, id, &raw).unwrap_or_else(|error| {
            codec::promise_error(id, &RpcError::new("Error", error.to_string()))
        }),
    }
}

fn track(active: &Active, id: i32, handle: AbortHandle) {
    // 任务可能在登记前就已结束并尝试移除自己；结束了的句柄 abort 是空操作，留在表里到连接关闭。
    lock(active).insert(id, handle);
}

fn lock(active: &Active) -> std::sync::MutexGuard<'_, HashMap<i32, AbortHandle>> {
    active
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::time::Duration;
    use zcode_host_control::ClientMode;
    use zcode_host_kit::contract::{ChannelContract, Contract, Member, MemberKind};
    use zcode_host_rpc::Reader;

    fn contract() -> Contract {
        let member = |name: &str, kind| Member {
            name: name.into(),
            kind,
        };
        Contract {
            channels: vec![
                ChannelContract {
                    channel: "setting".into(),
                    members: vec![
                        member("get", MemberKind::Method),
                        member("onDidUpdate", MemberKind::Event),
                    ],
                },
                ChannelContract {
                    channel: "settings-sync".into(),
                    members: vec![member("getFirstRunPromptState", MemberKind::Method)],
                },
            ],
        }
    }

    fn request(header: Value, body: Option<Value>) -> Vec<u8> {
        let mut out = Vec::new();
        codec::write_value(&mut out, &header);
        match body {
            Some(body) => codec::write_value(&mut out, &body),
            None => codec::write_undefined(&mut out),
        }
        out
    }

    fn decode(message: &[u8]) -> (Value, RpcValue) {
        let mut reader = Reader::new(message);
        let header = reader.value().unwrap().into_json();
        (header, reader.value().unwrap())
    }

    struct NullSink;

    impl zcode_host_control::ControlSink for NullSink {
        fn send(&self, _message: zcode_host_control::HostToMain) {}
    }

    struct Harness {
        ready: watch::Sender<Option<Arc<ChannelRegistry>>>,
        to_server: mpsc::UnboundedSender<Vec<u8>>,
        from_server: mpsc::UnboundedReceiver<Vec<u8>>,
        flow: watch::Sender<FlowState>,
    }

    fn start() -> Harness {
        let (ready, ready_rx) = watch::channel(None);
        let (to_server, inbound) = mpsc::unbounded_channel();
        let (outbound, from_server) = mpsc::unbounded_channel();
        let context = CallContext {
            connection: 1,
            attachment_id: "a1".into(),
            client_mode: ClientMode::DesktopContinuous,
        };
        let (flow, flow_rx) = watch::channel(FlowState::Drained);
        let telemetry = crate::rpc_telemetry::RpcTelemetry::new(Arc::new(NullSink));
        tokio::spawn(serve(
            context, ready_rx, inbound, outbound, flow_rx, telemetry,
        ));
        Harness {
            ready,
            to_server,
            from_server,
            flow,
        }
    }

    async fn next(harness: &mut Harness) -> (Value, RpcValue) {
        let message = tokio::time::timeout(Duration::from_secs(1), harness.from_server.recv())
            .await
            .expect("server answered within 1s")
            .expect("server still open");
        decode(&message)
    }

    #[tokio::test]
    async fn initialize_waits_for_ready() {
        let mut harness = start();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), harness.from_server.recv())
                .await
                .is_err()
        );
        harness
            .ready
            .send_replace(Some(Arc::new(ChannelRegistry::from_contract(contract()))));
        assert_eq!(next(&mut harness).await.0, json!([200]));
    }

    #[tokio::test]
    async fn stub_methods_answer_immediately() {
        let mut harness = start();
        harness
            .ready
            .send_replace(Some(Arc::new(ChannelRegistry::from_contract(contract()))));
        next(&mut harness).await;

        harness
            .to_server
            .send(request(json!([100, 1, "setting", "get"]), Some(json!([]))))
            .unwrap();
        let (header, body) = next(&mut harness).await;
        assert_eq!(header, json!([202, 1]));
        let body = body.into_json();
        assert_eq!(body["name"], "NotImplemented");
        assert_eq!(body["code"], "fault.host.notImplemented");

        harness
            .to_server
            .send(request(
                json!([100, 2, "settings-sync", "getFirstRunPromptState"]),
                Some(json!([])),
            ))
            .unwrap();
        let (header, body) = next(&mut harness).await;
        assert_eq!(header, json!([201, 2]));
        assert_eq!(body.into_json(), json!({"handled": true}));

        harness
            .to_server
            .send(request(json!([100, 3, "nope", "x"]), Some(json!([]))))
            .unwrap();
        let (header, body) = next(&mut harness).await;
        assert_eq!(header, json!([202, 3]));
        assert_eq!(body.into_json()["name"], "Unknown channel");

        harness
            .to_server
            .send(request(
                json!([100, 4, "setting", "missing"]),
                Some(json!([])),
            ))
            .unwrap();
        let (header, body) = next(&mut harness).await;
        assert_eq!(header, json!([202, 4]));
        assert_eq!(body.into_json()["message"], "Method not found: missing");
    }

    #[tokio::test]
    async fn listen_is_accepted_without_reply() {
        let mut harness = start();
        harness
            .ready
            .send_replace(Some(Arc::new(ChannelRegistry::from_contract(contract()))));
        next(&mut harness).await;
        harness
            .to_server
            .send(request(json!([102, 5, "setting", "onDidUpdate"]), None))
            .unwrap();
        harness
            .to_server
            .send(request(json!([103, 5]), None))
            .unwrap();
        // 订阅没有应答；之后的调用照常应答，说明连接没有被订阅卡住。
        harness
            .to_server
            .send(request(json!([100, 6, "setting", "get"]), Some(json!([]))))
            .unwrap();
        assert_eq!(next(&mut harness).await.0, json!([202, 6]));
    }

    /// 记录收到的流控边沿。
    struct FlowRecorder(Mutex<Vec<(u64, FlowState)>>);

    impl zcode_host_kit::ServiceChannel for FlowRecorder {
        fn call(
            &self,
            _context: &CallContext,
            _method: &str,
            _args: Vec<RpcValue>,
        ) -> zcode_host_kit::CallFuture {
            Box::pin(async { Ok(Reply::Undefined) })
        }

        fn listen(
            &self,
            _context: &CallContext,
            _event: &str,
            _arg: RpcValue,
        ) -> Result<zcode_host_kit::EventStream, RpcError> {
            Err(RpcError::new("Error", "no events"))
        }

        fn connection_flow(&self, context: &CallContext, state: FlowState) {
            self.0.lock().unwrap().push((context.connection, state));
        }
    }

    #[tokio::test]
    async fn flow_edges_reach_channels_with_the_connection_context() {
        let mut harness = start();
        let recorder = Arc::new(FlowRecorder(Mutex::new(Vec::new())));
        let mut registry = ChannelRegistry::from_contract(contract());
        registry.register("recorder", recorder.clone());
        harness.ready.send_replace(Some(Arc::new(registry)));
        next(&mut harness).await;
        harness.flow.send_replace(FlowState::Saturated);
        harness.flow.send_replace(FlowState::Drained);
        // 之后的请求照常应答；应答到达时，之前的边沿已在同一循环里处理过。
        harness
            .to_server
            .send(request(json!([100, 7, "recorder", "x"]), Some(json!([]))))
            .unwrap();
        assert_eq!(next(&mut harness).await.0, json!([201, 7]));
        let seen = recorder.0.lock().unwrap().clone();
        assert_eq!(seen.last(), Some(&(1, FlowState::Drained)));
        assert!(seen.iter().all(|(connection, _)| *connection == 1));
    }
}
