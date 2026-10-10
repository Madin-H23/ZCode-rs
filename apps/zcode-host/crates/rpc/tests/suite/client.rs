//! `RpcClient` 与 `ChannelClient` 的行为对齐：请求编码与 TS 夹具逐字节相同；Initialize 前排队；
//! 取消、退订、错误透传与连接关闭。
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::json;
use tokio::sync::mpsc;
use zcode_async::TaskScope;
use zcode_host_rpc::client::{CONNECTION_CLOSED, LISTENER_CAPACITY, OUTBOUND_CAPACITY};
use zcode_host_rpc::codec::{self, RpcError};
use zcode_host_rpc::{RpcClient, RpcValue, read_request};

use super::common::{fixture, hex};

#[test]
fn request_encoding_matches_typescript_bytes() {
    let messages = &fixture()["messages"];
    let call = codec::encode_request(100, 5, "setting", "get", &RpcValue::Array(vec![]));
    assert_eq!(hex(&call), messages["requestPromise"]);
    let call = codec::encode_request(
        100,
        6,
        "file",
        "readTextFile",
        &RpcValue::Array(vec![RpcValue::Json(
            json!({"path": "/tmp/a.txt", "length": 10}),
        )]),
    );
    assert_eq!(hex(&call), messages["requestCall"]);
    let listen = codec::encode_request(
        102,
        7,
        "zcode-agent",
        "onDynamicConversationFrame",
        &RpcValue::Json(json!({"workspacePath": "/w"})),
    );
    assert_eq!(hex(&listen), messages["requestListen"]);
    assert_eq!(
        hex(&codec::encode_cancel(101, 6)),
        messages["requestCancel"]
    );
    assert_eq!(
        hex(&codec::encode_cancel(103, 7)),
        messages["requestDispose"]
    );
}

#[test]
fn error_payload_restores_passthrough_keys() {
    let error = RpcError::from_payload(&json!({
        "message": "boom", "name": "TypeError", "code": "fault.x", "data": {"a": 1},
        "status": 429, "retryAfterMs": 50, "stack": ["s"], "unknown": true,
    }));
    assert_eq!(error.name, "TypeError");
    assert_eq!(error.message, "boom");
    assert_eq!(error.code_str(), Some("fault.x"));
    assert_eq!(error.field("status"), Some(&json!(429)));
    assert_eq!(error.field("retryAfterMs"), Some(&json!(50)));
    assert_eq!(error.field("unknown"), None);
    assert_eq!(
        error.to_json(),
        json!({"message": "boom", "name": "TypeError", "code": "fault.x", "data": {"a": 1}, "status": 429, "retryAfterMs": 50})
    );
}

struct Peer {
    to_client: mpsc::Sender<Vec<u8>>,
    from_client: mpsc::Receiver<Vec<u8>>,
    /// 读循环所在的作用域：随 Peer 一起释放。
    _scope: TaskScope,
}

impl Peer {
    async fn next(&mut self) -> zcode_host_rpc::Request {
        let message = tokio::time::timeout(Duration::from_secs(1), self.from_client.recv())
            .await
            .expect("client sent within 1s")
            .expect("client open");
        read_request(&message).unwrap()
    }

    async fn silent(&mut self) {
        assert!(
            tokio::time::timeout(Duration::from_millis(50), self.from_client.recv())
                .await
                .is_err(),
            "client sent nothing"
        );
    }
}

fn start() -> (RpcClient, Peer) {
    start_with(OUTBOUND_CAPACITY)
}

fn start_with(outbound_capacity: usize) -> (RpcClient, Peer) {
    let scope = TaskScope::root("test", |panic| panic!("task panicked: {panic:?}"));
    let (outbound, from_client) = mpsc::channel(outbound_capacity);
    let (to_client, inbound) = mpsc::channel(64);
    (
        RpcClient::start(&scope.spawner(), outbound, inbound),
        Peer {
            to_client,
            from_client,
            _scope: scope,
        },
    )
}

#[tokio::test]
async fn requests_wait_for_initialize_then_flow_in_order() {
    let (client, mut peer) = start();
    let first = tokio::spawn(client.call("system", "getInfo", vec![]));
    let second =
        tokio::spawn(client.call("file", "stat", vec![RpcValue::Json(json!({"path": "/"}))]));
    peer.silent().await;
    peer.to_client.try_send(codec::initialize()).unwrap();
    assert!(client.wait_initialized().await);
    let request = peer.next().await;
    assert_eq!(
        (request.id, request.channel.as_str(), request.name.as_str()),
        (0, "system", "getInfo")
    );
    let request = peer.next().await;
    assert_eq!((request.id, request.name.as_str()), (1, "stat"));
    assert_eq!(
        request.body,
        RpcValue::Array(vec![RpcValue::Json(json!({"path": "/"}))])
    );

    peer.to_client
        .try_send(codec::success_value(0, &json!({"os": "x"})))
        .unwrap();
    peer.to_client
        .try_send(codec::promise_error(
            1,
            &RpcError::new("Error", "ENOENT").with_code("ENOENT"),
        ))
        .unwrap();
    assert_eq!(
        first.await.unwrap().unwrap(),
        RpcValue::Json(json!({"os": "x"}))
    );
    let error = second.await.unwrap().unwrap_err();
    assert_eq!(
        (error.message.as_str(), error.code_str()),
        ("ENOENT", Some("ENOENT"))
    );
}

#[tokio::test]
async fn dropping_a_call_cancels_it_and_late_answers_are_ignored() {
    let (client, mut peer) = start();
    peer.to_client.try_send(codec::initialize()).unwrap();
    let call = client.call("git", "status", vec![]);
    let pending = tokio::spawn(call);
    let request = peer.next().await;
    pending.abort();
    let _ = pending.await;
    let cancel = peer.next().await;
    assert_eq!(
        (cancel.kind, cancel.id),
        (zcode_host_rpc::RequestKind::PromiseCancel, request.id)
    );
    // 迟到的应答不影响后续调用。
    peer.to_client
        .try_send(codec::success_value(request.id, &json!(1)))
        .unwrap();
    let next = tokio::spawn(client.call("git", "status", vec![]));
    let request = peer.next().await;
    peer.to_client
        .try_send(codec::success_value(request.id, &json!(2)))
        .unwrap();
    assert_eq!(next.await.unwrap().unwrap(), RpcValue::Int(2));
}

#[tokio::test]
async fn events_flow_until_the_stream_is_dropped() {
    let (client, mut peer) = start();
    peer.to_client.try_send(codec::initialize()).unwrap();
    let mut events = client.listen(
        "zcode-agent",
        "onDynamicConversationFrame",
        RpcValue::Json(json!({"workspacePath": "/w"})),
    );
    let listen = peer.next().await;
    assert_eq!(listen.kind, zcode_host_rpc::RequestKind::EventListen);
    assert_eq!(listen.body, RpcValue::Json(json!({"workspacePath": "/w"})));
    peer.to_client
        .try_send(codec::response_value(204, listen.id, &json!({"seq": 1})))
        .unwrap();
    assert_eq!(events.next().await, Some(RpcValue::Json(json!({"seq": 1}))));
    drop(events);
    let dispose = peer.next().await;
    assert_eq!(
        (dispose.kind, dispose.id),
        (zcode_host_rpc::RequestKind::EventDispose, listen.id)
    );
}

#[tokio::test]
async fn closing_the_transport_fails_pending_calls_and_ends_streams() {
    let (client, mut peer) = start();
    peer.to_client.try_send(codec::initialize()).unwrap();
    let pending = tokio::spawn(client.call("file", "readFile", vec![]));
    let mut events = client.listen("setting", "onDidUpdate", RpcValue::Undefined);
    peer.next().await;
    peer.next().await;
    drop(peer);
    let error = pending.await.unwrap().unwrap_err();
    assert_eq!(error.message, zcode_host_rpc::client::CONNECTION_CLOSED);
    assert_eq!(events.next().await, None);
    client.closed().await;
    assert!(client.is_closed());
    assert!(client.call("file", "x", vec![]).await.is_err());
}

#[tokio::test]
async fn non_error_rejection_keeps_the_payload() {
    let (client, mut peer) = start();
    peer.to_client.try_send(codec::initialize()).unwrap();
    let call = tokio::spawn(client.call("x", "y", vec![]));
    let request = peer.next().await;
    peer.to_client
        .try_send(codec::response_value(
            203,
            request.id,
            &json!({"reason": "nope"}),
        ))
        .unwrap();
    let error = call.await.unwrap().unwrap_err();
    assert_eq!(error.data.as_deref(), Some(&json!({"reason": "nope"})));
}

/// 转发用的 encoded 调用与事件：body 逐字节保持远端编码（数组里的 undefined、VSBuffer 标签）。
#[tokio::test]
async fn encoded_calls_and_events_keep_remote_bytes() {
    let (client, mut peer) = start();
    peer.to_client.try_send(codec::initialize()).unwrap();
    // [undefined, VSBuffer(0xaa 0xbb)]：Array 标签 4、长度 2、Undefined 0、VSBuffer 3 + 长度 2。
    let body = vec![4u8, 2, 0, 3, 2, 0xaa, 0xbb];
    let call = tokio::spawn(client.call_encoded("file", "x", vec![]));
    let request = peer.next().await;
    let mut response = Vec::new();
    codec::write_array_header(&mut response, 2);
    codec::write_int(&mut response, 201);
    codec::write_int(&mut response, request.id);
    response.extend_from_slice(&body);
    peer.to_client.try_send(response).unwrap();
    assert_eq!(call.await.unwrap().unwrap(), body);
    assert_eq!(
        codec::response_encoded(201, request.id, &body)[..],
        codec::split_response(&codec::response_encoded(201, request.id, &body))
            .map(|(kind, id, rest)| {
                assert_eq!((kind, id), (201, Some(request.id)));
                codec::response_encoded(kind, request.id, &rest)
            })
            .unwrap()[..]
    );

    let mut events = client.listen_encoded("terminal", "onData", RpcValue::Undefined);
    let listen = peer.next().await;
    let mut fire = Vec::new();
    codec::write_array_header(&mut fire, 2);
    codec::write_int(&mut fire, 204);
    codec::write_int(&mut fire, listen.id);
    fire.extend_from_slice(&body);
    peer.to_client.try_send(fire).unwrap();
    assert_eq!(events.next().await, Some(body.clone()));
    // 头之后没有 body：按 undefined 交出。
    let mut bare = Vec::new();
    codec::write_array_header(&mut bare, 2);
    codec::write_int(&mut bare, 204);
    codec::write_int(&mut bare, listen.id);
    peer.to_client.try_send(bare).unwrap();
    assert_eq!(events.next().await, Some(vec![0]));
}

fn event_fire(id: i32, value: i64) -> Vec<u8> {
    codec::response_value(204, id, &json!(value))
}

/// 有界队列（docs/specs/rust-u2-task-scope.md §6.2）：事件流的消费方落后 LISTENER_CAPACITY 条时，
/// 只有这个事件流结束并向对端退订，已入队的事件照常读完；同一连接上的调用不受影响。
#[tokio::test]
async fn a_listener_that_falls_behind_ends_and_is_disposed_upstream() {
    let (client, mut peer) = start();
    peer.to_client.try_send(codec::initialize()).unwrap();
    let mut events = client.listen("terminal", "onData", RpcValue::Undefined);
    let listen = peer.next().await;
    for value in 0..=LISTENER_CAPACITY as i64 {
        peer.to_client
            .send(event_fire(listen.id, value))
            .await
            .unwrap();
    }
    let dispose = peer.next().await;
    assert_eq!(
        (dispose.kind, dispose.id),
        (zcode_host_rpc::RequestKind::EventDispose, listen.id)
    );
    let mut received = 0;
    while let Some(value) = events.next().await {
        assert_eq!(value.into_json(), json!(received));
        received += 1;
    }
    assert_eq!(received, LISTENER_CAPACITY as i64);
    let call = tokio::spawn(client.call("git", "status", vec![]));
    let request = peer.next().await;
    peer.to_client
        .try_send(codec::response_value(201, request.id, &json!("clean")))
        .unwrap();
    assert_eq!(call.await.unwrap().unwrap().into_json(), json!("clean"));
}

/// 出站队列满（传输长时间不写）或 `[200]` 之前排队超过 OUTBOUND_CAPACITY 条：客户端关闭，挂起的
/// 调用以 Connection closed 失败，而不是无限缓存。
#[tokio::test]
async fn a_full_outbound_queue_closes_the_client() {
    let (client, peer) = start_with(1);
    peer.to_client.try_send(codec::initialize()).unwrap();
    assert!(client.wait_initialized().await);
    let first = tokio::spawn(client.call("a", "first", vec![]));
    let second = tokio::spawn(client.call("a", "second", vec![]));
    for call in [first, second] {
        assert_eq!(call.await.unwrap().unwrap_err().message, CONNECTION_CLOSED);
    }
    assert!(client.is_closed());

    let (client, _peer) = start();
    let calls: Vec<_> = (0..=OUTBOUND_CAPACITY)
        .map(|_| client.call("a", "queued", vec![]))
        .collect();
    assert!(client.is_closed());
    for result in futures_util::future::join_all(calls).await {
        assert_eq!(result.unwrap_err().message, CONNECTION_CLOSED);
    }
}
