//! Gateway 的行为测试（docs/specs/rust-u2-gateway.md §3）：进程内连接，不经 socket。
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::watch;
use zcode_async::{TaskPanic, TaskScope};
use zcode_host_control::{AttachmentScope, ClientMode};
use zcode_host_kit::contract::{ChannelContract, Contract, Member, MemberKind};
use zcode_host_kit::{
    CallContext, CallFuture, ChannelRegistry, EventStream, FlowState, Reply, RpcError, RpcValue,
    ServiceChannel,
};
use zcode_host_rpc::{Reader, codec};

use super::*;

const WAIT: Duration = Duration::from_secs(10);

struct NoTelemetry;

impl CallObserver for NoTelemetry {
    fn call_finished(&self, _: &str, _: &str, _: Duration, _: Option<&RpcError>) {}
}

struct Host {
    gateway: Gateway,
    panics: Arc<Mutex<Vec<TaskPanic>>>,
    _scope: TaskScope,
}

fn host(limits: OutboundLimits) -> Host {
    let panics = Arc::new(Mutex::new(Vec::new()));
    let reported = panics.clone();
    let scope = TaskScope::root("host", move |panic| {
        reported.lock().unwrap().push(panic.clone());
    });
    let gateway = Gateway::new(scope.spawner(), Arc::new(NoTelemetry), limits);
    Host {
        gateway,
        panics,
        _scope: scope,
    }
}

fn meta(attachment_id: &str) -> AttachmentMeta {
    AttachmentMeta {
        attachment_id: attachment_id.into(),
        client_mode: ClientMode::DesktopContinuous,
        scope: AttachmentScope::Local,
        window_id: None,
    }
}

fn ready(registry: &Arc<ChannelRegistry>) -> ChannelSource {
    let registry = registry.clone();
    Box::pin(async move {
        Some(Channels {
            registry,
            closed: None,
        })
    })
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

/// 测试里的客户端：像 transport 一样写入请求、取出应答并报告已写出。
struct Client(Connection);

impl Client {
    async fn send(&self, header: Value, body: Option<Value>) {
        self.0.inbound.send(request(header, body)).await.unwrap();
    }

    async fn call(&self, id: i32, method: &str, params: Value) {
        self.send(json!([100, id, "probe", method]), Some(json!([params])))
            .await;
    }

    async fn next(&self) -> (Value, RpcValue) {
        let message = tokio::time::timeout(WAIT, self.0.outbound.next())
            .await
            .expect("answered in time")
            .expect("connection still open");
        self.0.outbound.written(message.len());
        let mut reader = Reader::new(&message);
        let header = reader.value().unwrap().into_json();
        (header, reader.value().unwrap())
    }

    async fn ended(&self) -> bool {
        tokio::time::timeout(WAIT, self.0.outbound.next())
            .await
            .expect("closed in time")
            .is_none()
    }
}

/// 可观测的 channel：参数 `{n, key?, gate?, panic?, bytes?}`。
#[derive(Default)]
struct Probe {
    log: Mutex<Vec<String>>,
    gates: Mutex<HashMap<String, watch::Sender<bool>>>,
    flows: Mutex<Vec<(u64, FlowState)>>,
}

impl Probe {
    fn record(&self, entry: String) {
        self.log.lock().unwrap().push(entry);
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn gate(&self, name: &str) -> watch::Receiver<bool> {
        self.gates
            .lock()
            .unwrap()
            .entry(name.to_owned())
            .or_insert_with(|| watch::Sender::new(false))
            .subscribe()
    }

    fn open(&self, name: &str) {
        self.gates
            .lock()
            .unwrap()
            .entry(name.to_owned())
            .or_insert_with(|| watch::Sender::new(false))
            .send_replace(true);
    }

    async fn wait_for(&self, entry: &str) {
        tokio::time::timeout(WAIT, async {
            while !self.log().iter().any(|seen| seen == entry) {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{entry} within {WAIT:?}; log: {:?}", self.log()));
    }
}

/// 调用 future 被释放而没有完成时记一笔：用来确认任务确实已经结束。
struct DropLog(Arc<Probe>, i64, bool);

impl Drop for DropLog {
    fn drop(&mut self) {
        if !self.2 {
            self.0.record(format!("dropped:{}", self.1));
        }
    }
}

struct ProbeChannel(Arc<Probe>);

fn params(args: &[RpcValue]) -> Option<&Value> {
    match args.first() {
        Some(RpcValue::Json(value)) => Some(value),
        _ => None,
    }
}

impl ServiceChannel for ProbeChannel {
    fn serial_key(&self, _: &CallContext, _: &str, args: &[RpcValue]) -> Option<String> {
        params(args)?["key"].as_str().map(str::to_owned)
    }

    fn call(&self, _: &CallContext, _: &str, args: Vec<RpcValue>) -> CallFuture {
        let params = params(&args).cloned().unwrap_or_default();
        let probe = self.0.clone();
        let gate = params["gate"].as_str().map(|name| probe.gate(name));
        Box::pin(async move {
            let n = params["n"].as_i64().unwrap_or(-1);
            probe.record(format!("start:{n}"));
            let mut guard = DropLog(probe.clone(), n, false);
            if let Some(mut gate) = gate {
                let _ = gate.wait_for(|open| *open).await;
            }
            if params["panic"] == json!(true) {
                panic!("probe panic {n}");
            }
            guard.2 = true;
            probe.record(format!("end:{n}"));
            match params["bytes"].as_u64() {
                Some(bytes) => Ok(Reply::Bytes(vec![0; bytes as usize])),
                None => Ok(Reply::Value(json!(n))),
            }
        })
    }

    fn listen(&self, _: &CallContext, _: &str, _: RpcValue) -> Result<EventStream, RpcError> {
        Ok(Box::pin(futures_util::stream::pending()))
    }

    fn connection_closed(&self, context: &CallContext) {
        self.0.record(format!("closed:{}", context.connection));
    }

    fn connection_flow(&self, context: &CallContext, state: FlowState) {
        self.0
            .flows
            .lock()
            .unwrap()
            .push((context.connection, state));
    }
}

fn probe_registry() -> (Arc<Probe>, Arc<ChannelRegistry>) {
    let probe = Arc::new(Probe::default());
    let mut registry = ChannelRegistry::from_contract(contract());
    registry.register("probe", Arc::new(ProbeChannel(probe.clone())));
    (probe, Arc::new(registry))
}

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

/// 等异步的收尾（释放通道、上报 panic）完成。
async fn eventually(what: &str, check: impl Fn() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what} within {WAIT:?}"));
}

/// 建一条连接并读掉 `[200]`。
async fn connected(host: &Host, attachment_id: &str, registry: &Arc<ChannelRegistry>) -> Client {
    let client = Client(host.gateway.connect(meta(attachment_id), ready(registry)));
    assert_eq!(client.next().await.0, json!([200]));
    client
}

#[tokio::test]
async fn initialize_waits_for_the_channel_table() {
    let host = host(OutboundLimits::default());
    let (set, mut table) = watch::channel::<Option<Arc<ChannelRegistry>>>(None);
    let source: ChannelSource = Box::pin(async move {
        let registry = table.wait_for(Option::is_some).await.ok()?.clone()?;
        Some(Channels {
            registry,
            closed: None,
        })
    });
    let client = Client(host.gateway.connect(meta("a1"), source));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), client.0.outbound.next())
            .await
            .is_err()
    );
    set.send_replace(Some(Arc::new(ChannelRegistry::from_contract(contract()))));
    assert_eq!(client.next().await.0, json!([200]));
}

#[tokio::test]
async fn stub_methods_and_unknown_channels_answer_immediately() {
    let host = host(OutboundLimits::default());
    let registry = Arc::new(ChannelRegistry::from_contract(contract()));
    let client = connected(&host, "a1", &registry).await;

    client
        .send(json!([100, 1, "setting", "get"]), Some(json!([])))
        .await;
    let (header, body) = client.next().await;
    assert_eq!(header, json!([202, 1]));
    let body = body.into_json();
    assert_eq!(body["name"], "NotImplemented");
    assert_eq!(body["code"], "fault.host.notImplemented");

    client
        .send(
            json!([100, 2, "settings-sync", "getFirstRunPromptState"]),
            Some(json!([])),
        )
        .await;
    let (header, body) = client.next().await;
    assert_eq!(header, json!([201, 2]));
    assert_eq!(body.into_json(), json!({"handled": true}));

    client
        .send(json!([100, 3, "nope", "x"]), Some(json!([])))
        .await;
    let (header, body) = client.next().await;
    assert_eq!(header, json!([202, 3]));
    assert_eq!(body.into_json()["name"], "Unknown channel");

    client
        .send(json!([100, 4, "setting", "missing"]), Some(json!([])))
        .await;
    let (header, body) = client.next().await;
    assert_eq!(header, json!([202, 4]));
    assert_eq!(body.into_json()["message"], "Method not found: missing");
}

#[tokio::test]
async fn listen_is_accepted_without_reply() {
    let host = host(OutboundLimits::default());
    let registry = Arc::new(ChannelRegistry::from_contract(contract()));
    let client = connected(&host, "a1", &registry).await;
    client
        .send(json!([102, 5, "setting", "onDidUpdate"]), None)
        .await;
    client.send(json!([103, 5]), None).await;
    // 订阅没有应答；之后的调用照常应答，说明连接没有被订阅卡住。
    client
        .send(json!([100, 6, "setting", "get"]), Some(json!([])))
        .await;
    assert_eq!(client.next().await.0, json!([202, 6]));
}

/// G1：同一连接上连续到达的同键调用按到达顺序逐个执行。修复前（每个调用一个任务、谁先被调度谁先
/// 执行）同样的 2000 个调用里有六百多对相邻调用的开始顺序与到达顺序相反。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn calls_with_one_key_run_one_at_a_time_in_arrival_order() {
    const CALLS: i64 = 2000;
    let host = host(OutboundLimits::default());
    let (probe, registry) = probe_registry();
    let client = connected(&host, "a1", &registry).await;
    for n in 0..CALLS {
        client
            .call(n as i32 + 10, "x", json!({"n": n, "key": "session:w:s"}))
            .await;
    }
    let mut replies = Vec::new();
    for _ in 0..CALLS {
        replies.push(client.next().await.0);
    }
    let expected_log: Vec<String> = (0..CALLS)
        .flat_map(|n| [format!("start:{n}"), format!("end:{n}")])
        .collect();
    assert_eq!(probe.log(), expected_log, "no overlap, arrival order");
    let expected_replies: Vec<Value> = (0..CALLS).map(|n| json!([201, n + 10])).collect();
    assert_eq!(replies, expected_replies);
    eventually("the idle lane is removed", || {
        host.gateway.shared.lanes.is_empty()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn different_keys_run_in_parallel() {
    let host = host(OutboundLimits::default());
    let (probe, registry) = probe_registry();
    let client = connected(&host, "a1", &registry).await;
    client
        .call(1, "x", json!({"n": 1, "key": "a", "gate": "a"}))
        .await;
    probe.wait_for("start:1").await;
    client.call(2, "x", json!({"n": 2, "key": "b"})).await;
    assert_eq!(client.next().await.0, json!([201, 2]), "b is not behind a");
    probe.open("a");
    assert_eq!(client.next().await.0, json!([201, 1]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unkeyed_calls_are_not_blocked_by_a_keyed_lane() {
    let host = host(OutboundLimits::default());
    let (probe, registry) = probe_registry();
    let client = connected(&host, "a1", &registry).await;
    client
        .call(1, "x", json!({"n": 1, "key": "k", "gate": "k"}))
        .await;
    client.call(2, "x", json!({"n": 2, "key": "k"})).await;
    client.call(3, "x", json!({"n": 3})).await;
    assert_eq!(client.next().await.0, json!([201, 3]));
    assert!(
        !probe.log().contains(&"start:2".to_owned()),
        "the second keyed call waits for the first"
    );
    probe.open("k");
    assert_eq!(client.next().await.0, json!([201, 1]));
    assert_eq!(client.next().await.0, json!([201, 2]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_queued_call_never_runs() {
    let host = host(OutboundLimits::default());
    let (probe, registry) = probe_registry();
    let client = connected(&host, "a1", &registry).await;
    client
        .call(1, "x", json!({"n": 1, "key": "k", "gate": "k"}))
        .await;
    client.call(2, "x", json!({"n": 2, "key": "k"})).await;
    client.call(3, "x", json!({"n": 3, "key": "k"})).await;
    probe.wait_for("start:1").await;
    client.send(json!([101, 2]), None).await;
    probe.open("k");
    assert_eq!(client.next().await.0, json!([201, 1]));
    assert_eq!(client.next().await.0, json!([201, 3]));
    assert_eq!(
        probe.log(),
        ["start:1", "end:1", "start:3", "end:3"],
        "the cancelled call left the queue without starting"
    );
    eventually("the idle lane is removed", || {
        host.gateway.shared.lanes.is_empty()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_a_connection_drops_its_queued_calls_and_the_key_moves_on() {
    let host = host(OutboundLimits::default());
    let (probe, registry) = probe_registry();
    let first = connected(&host, "a1", &registry).await;
    let second = connected(&host, "a2", &registry).await;
    first
        .call(1, "x", json!({"n": 1, "key": "k", "gate": "k"}))
        .await;
    probe.wait_for("start:1").await;
    first.call(2, "x", json!({"n": 2, "key": "k"})).await;
    second.call(3, "x", json!({"n": 3, "key": "k"})).await;
    let Client(Connection {
        id,
        inbound,
        outbound,
        ..
    }) = first;
    // transport 停止读取（对端断开）。
    drop(inbound);
    assert_eq!(second.next().await.0, json!([201, 3]));
    probe.wait_for(&format!("closed:{id}")).await;
    let log = probe.log();
    assert!(log.contains(&"dropped:1".to_owned()), "{log:?}");
    assert!(!log.contains(&"start:2".to_owned()), "{log:?}");
    assert_eq!(outbound.next().await, None);
    assert_eq!(outbound.close_reason(), Some(CloseReason::PeerClosed));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_closed_runs_once_after_the_connection_tasks_ended() {
    let host = host(OutboundLimits::default());
    let (probe, registry) = probe_registry();
    let client = connected(&host, "a1", &registry).await;
    client.call(1, "x", json!({"n": 1, "gate": "g"})).await;
    client
        .call(2, "x", json!({"n": 2, "key": "k", "gate": "g"}))
        .await;
    client.call(3, "x", json!({"n": 3, "key": "k"})).await;
    client.send(json!([102, 4, "probe", "onEvent"]), None).await;
    probe.wait_for("start:1").await;
    probe.wait_for("start:2").await;
    // 两条关闭路径同时到达：detach 与 transport 停止读取。
    assert_eq!(host.gateway.close_attachment("a1"), 1);
    let Client(Connection { id, inbound, .. }) = client;
    drop(inbound);
    probe.wait_for(&format!("closed:{id}")).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let log = probe.log();
    let closed_at = log
        .iter()
        .position(|entry| *entry == format!("closed:{id}"))
        .unwrap();
    assert_eq!(
        log.iter()
            .filter(|entry| entry.starts_with("closed:"))
            .count(),
        1,
        "{log:?}"
    );
    for dropped in ["dropped:1", "dropped:2"] {
        let at = log.iter().position(|entry| entry == dropped);
        assert!(at.is_some_and(|at| at < closed_at), "{log:?}");
    }
    assert!(!log.contains(&"start:3".to_owned()), "{log:?}");
    assert!(host.gateway.connections().is_empty());
    assert!(host.gateway.shared.lanes.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flow_edges_reach_channels_with_the_connection_context() {
    let host = host(OutboundLimits {
        high_water: 100,
        low_water: 25,
        hard_limit: 1 << 20,
    });
    let (probe, registry) = probe_registry();
    let client = Client(host.gateway.connect(meta("a1"), ready(&registry)));
    // 不读：`[200]` 与 200 字节的应答一起越过高水位。
    client.call(1, "x", json!({"n": 1, "bytes": 200})).await;
    tokio::time::timeout(WAIT, async {
        while probe.flows.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(client.next().await.0, json!([200]));
    assert_eq!(client.next().await.0, json!([201, 1]));
    // 读完后积压回到 0，越过低水位发 drained；之后的请求照常应答。
    client.call(2, "x", json!({"n": 2})).await;
    assert_eq!(client.next().await.0, json!([201, 2]));
    assert_eq!(
        *probe.flows.lock().unwrap(),
        [
            (client.0.id, FlowState::Saturated),
            (client.0.id, FlowState::Drained)
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exceeding_the_hard_limit_closes_only_that_connection() {
    let host = host(OutboundLimits {
        high_water: 100,
        low_water: 25,
        hard_limit: 1000,
    });
    let (probe, registry) = probe_registry();
    let stalled = Client(host.gateway.connect(meta("a1"), ready(&registry)));
    let healthy = connected(&host, "a2", &registry).await;
    stalled.call(1, "x", json!({"n": 1, "bytes": 600})).await;
    stalled.call(2, "x", json!({"n": 2, "bytes": 600})).await;
    probe.wait_for(&format!("closed:{}", stalled.0.id)).await;
    assert!(stalled.ended().await);
    assert_eq!(
        stalled.0.outbound.close_reason(),
        Some(CloseReason::OutboundBacklog)
    );
    assert_eq!(healthy.0.outbound.close_reason(), None);
    healthy.call(3, "x", json!({"n": 3})).await;
    assert_eq!(healthy.next().await.0, json!([201, 3]));
    let open: Vec<u64> = host
        .gateway
        .connections()
        .iter()
        .map(|info| info.id)
        .collect();
    assert_eq!(open, [healthy.0.id]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicking_handler_answers_with_an_error_and_is_reported() {
    let host = host(OutboundLimits::default());
    let (_, registry) = probe_registry();
    let client = connected(&host, "a1", &registry).await;
    client
        .call(1, "x", json!({"n": 1, "key": "k", "panic": true}))
        .await;
    let (header, body) = client.next().await;
    assert_eq!(header, json!([202, 1]));
    assert_eq!(
        body.into_json()["message"],
        "probe.x failed: internal error in the Rust Host"
    );
    // 通道已释放，连接继续工作。
    client.call(2, "x", json!({"n": 2, "key": "k"})).await;
    assert_eq!(client.next().await.0, json!([201, 2]));
    eventually("the panic is reported", || {
        !host.panics.lock().unwrap().is_empty()
    })
    .await;
    assert_eq!(
        *host.panics.lock().unwrap(),
        [TaskPanic {
            scope: format!("host/connection:{}", client.0.id),
            task: "call",
            message: "probe panic 1".into(),
        }]
    );
}

#[tokio::test]
async fn close_attachment_closes_every_connection_of_the_attachment() {
    let host = host(OutboundLimits::default());
    let (_, registry) = probe_registry();
    let first = connected(&host, "a1", &registry).await;
    let second = connected(&host, "a1", &registry).await;
    let other = connected(&host, "a2", &registry).await;
    assert_eq!(host.gateway.close_attachment("a1"), 2);
    assert!(first.ended().await);
    assert!(second.ended().await);
    for closed in [&first, &second] {
        assert_eq!(
            closed.0.outbound.close_reason(),
            Some(CloseReason::Detached)
        );
    }
    other.call(1, "x", json!({"n": 1})).await;
    assert_eq!(other.next().await.0, json!([201, 1]));
    assert_eq!(other.0.outbound.close_reason(), None);
}

#[tokio::test]
async fn the_channel_table_can_end_the_connection() {
    let host = host(OutboundLimits::default());
    let (probe, registry) = probe_registry();
    let (end, ended) = watch::channel(false);
    let source: ChannelSource = Box::pin(async move {
        Some(Channels {
            registry,
            closed: Some(ended),
        })
    });
    let client = Client(host.gateway.connect(meta("a1"), source));
    assert_eq!(client.next().await.0, json!([200]));
    end.send_replace(true);
    assert!(client.ended().await);
    assert_eq!(
        client.0.outbound.close_reason(),
        Some(CloseReason::ScopeClosed)
    );
    probe.wait_for(&format!("closed:{}", client.0.id)).await;
}

#[tokio::test]
async fn a_connection_without_a_channel_table_closes_without_initialize() {
    let host = host(OutboundLimits::default());
    let client = Client(host.gateway.connect(meta("a1"), Box::pin(async { None })));
    assert!(client.ended().await);
    assert_eq!(
        client.0.outbound.close_reason(),
        Some(CloseReason::ScopeUnavailable)
    );
    tokio::time::timeout(WAIT, client.0.inbound.closed())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transport_write_failure_closes_with_transport_failed() {
    let host = host(OutboundLimits::default());
    let (probe, registry) = probe_registry();
    let client = connected(&host, "a1", &registry).await;
    client.0.outbound.close();
    assert!(client.ended().await);
    assert_eq!(
        client.0.outbound.close_reason(),
        Some(CloseReason::TransportFailed)
    );
    probe.wait_for(&format!("closed:{}", client.0.id)).await;
}

/// 连接循环被中止（关闭根作用域）时没有走完收口：出站队列以 `InternalError` 关闭，不调用
/// `connection_closed`（§2.4）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_aborted_connection_loop_closes_with_internal_error() {
    let Host {
        gateway, _scope, ..
    } = host(OutboundLimits::default());
    let (probe, registry) = probe_registry();
    let client = Client(gateway.connect(meta("a1"), ready(&registry)));
    assert_eq!(client.next().await.0, json!([200]));
    drop(_scope);
    assert!(client.ended().await);
    assert_eq!(
        client.0.outbound.close_reason(),
        Some(CloseReason::InternalError)
    );
    assert!(gateway.connections().is_empty());
    assert!(!probe.log().iter().any(|entry| entry.starts_with("closed:")));
}

/// Host 退出（§2.4、§2.6）：已就绪与还在等 channel 表的连接都以 `HostShutdown` 关闭，就绪的那条照常
/// 收口（`connection_closed`）；`shutdown` 等 transport 释放 `Outbound` 才返回；之后接入的连接直接关闭。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_closes_every_connection_and_waits_for_the_transports() {
    let host = host(OutboundLimits::default());
    let (probe, registry) = probe_registry();
    let live = connected(&host, "a1", &registry).await;
    let live_id = live.0.id;
    let pending = Client(
        host.gateway
            .connect(meta("a2"), Box::pin(std::future::pending())),
    );
    let released = Arc::new(AtomicUsize::new(0));
    let transports: Vec<_> = [live, pending]
        .into_iter()
        .map(|client| {
            let released = released.clone();
            // 像 transport 一样：出站队列关闭后还要一点时间发 Close，之后才释放 Outbound。
            tokio::spawn(async move {
                while client.0.outbound.next().await.is_some() {}
                let reason = client.0.outbound.close_reason();
                tokio::time::sleep(Duration::from_millis(100)).await;
                released.fetch_add(1, Ordering::SeqCst);
                drop(client);
                reason
            })
        })
        .collect();
    let started = Instant::now();
    host.gateway.shutdown(WAIT).await;
    assert_eq!(
        released.load(Ordering::SeqCst),
        2,
        "shutdown returned before the transports released their Outbound"
    );
    assert!(started.elapsed() < WAIT);
    for transport in transports {
        assert_eq!(transport.await.unwrap(), Some(CloseReason::HostShutdown));
    }
    assert!(probe.log().contains(&format!("closed:{live_id}")));
    assert!(host.gateway.connections().is_empty());

    let late = Client(host.gateway.connect(meta("a3"), ready(&registry)));
    assert!(late.ended().await, "no [200] after the shutdown began");
    assert_eq!(
        late.0.outbound.close_reason(),
        Some(CloseReason::HostShutdown)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_stops_waiting_after_the_grace_period() {
    let host = host(OutboundLimits::default());
    let (_, registry) = probe_registry();
    // 一个永远不释放 Outbound 的 transport（例如对端不读，Close 写不出去）。
    let stuck = connected(&host, "a1", &registry).await;
    let started = Instant::now();
    host.gateway.shutdown(Duration::from_millis(100)).await;
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(100), "{waited:?}");
    assert!(waited < WAIT, "{waited:?}");
    assert!(stuck.ended().await);
    assert_eq!(
        stuck.0.outbound.close_reason(),
        Some(CloseReason::HostShutdown)
    );
}
