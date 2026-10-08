//! `RpcClient` 与 `ChannelClient` 的行为对齐：请求编码与 TS 夹具逐字节相同；Initialize 前排队；
//! 取消、退订、错误透传与连接关闭。
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use zcode_host_rpc::codec::{self, RpcError};
use zcode_host_rpc::{RpcClient, RpcValue, read_request};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/rpc-serialization.json")).unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

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
    to_client: mpsc::UnboundedSender<Vec<u8>>,
    from_client: mpsc::UnboundedReceiver<Vec<u8>>,
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
    let (outbound, from_client) = mpsc::unbounded_channel();
    let (to_client, inbound) = mpsc::unbounded_channel();
    (
        RpcClient::start(outbound, inbound),
        Peer {
            to_client,
            from_client,
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
    peer.to_client.send(codec::initialize()).unwrap();
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
        .send(codec::success_value(0, &json!({"os": "x"})))
        .unwrap();
    peer.to_client
        .send(codec::promise_error(
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
    peer.to_client.send(codec::initialize()).unwrap();
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
        .send(codec::success_value(request.id, &json!(1)))
        .unwrap();
    let next = tokio::spawn(client.call("git", "status", vec![]));
    let request = peer.next().await;
    peer.to_client
        .send(codec::success_value(request.id, &json!(2)))
        .unwrap();
    assert_eq!(next.await.unwrap().unwrap(), RpcValue::Int(2));
}

#[tokio::test]
async fn events_flow_until_the_stream_is_dropped() {
    let (client, mut peer) = start();
    peer.to_client.send(codec::initialize()).unwrap();
    let mut events = client.listen(
        "zcode-agent",
        "onDynamicConversationFrame",
        RpcValue::Json(json!({"workspacePath": "/w"})),
    );
    let listen = peer.next().await;
    assert_eq!(listen.kind, zcode_host_rpc::RequestKind::EventListen);
    assert_eq!(listen.body, RpcValue::Json(json!({"workspacePath": "/w"})));
    peer.to_client
        .send(codec::response_value(204, listen.id, &json!({"seq": 1})))
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
    peer.to_client.send(codec::initialize()).unwrap();
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
    peer.to_client.send(codec::initialize()).unwrap();
    let call = tokio::spawn(client.call("x", "y", vec![]));
    let request = peer.next().await;
    peer.to_client
        .send(codec::response_value(
            203,
            request.id,
            &json!({"reason": "nope"}),
        ))
        .unwrap();
    let error = call.await.unwrap().unwrap_err();
    assert_eq!(error.data.as_deref(), Some(&json!({"reason": "nope"})));
}
