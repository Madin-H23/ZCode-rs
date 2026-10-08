//! 与 packages/rpc 的真实编码逐字节比对（夹具由 fixtures/gen.ts 生成）。
use serde_json::{Value, json};
use zcode_host_rpc::codec::{self, RpcError};
use zcode_host_rpc::{FrameReader, Reader, RequestKind, RpcValue, frame, read_request};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/rpc-serialization.json")).unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}

fn is_undefined(value: &Value) -> bool {
    value.get("$undefined").is_some()
}

#[test]
fn values_match_typescript_bytes() {
    let fixture = fixture();
    for case in fixture["values"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let expected = case["hex"].as_str().unwrap();
        let mut out = Vec::new();
        if is_undefined(&case["value"]) {
            codec::write_undefined(&mut out);
        } else {
            codec::write_value(&mut out, &case["value"]);
        }
        assert_eq!(hex(&out), expected, "encode {name}");

        let decoded = Reader::new(&unhex(expected)).value().unwrap();
        if is_undefined(&case["value"]) {
            assert_eq!(decoded, RpcValue::Undefined, "decode {name}");
        } else {
            assert_eq!(decoded.into_json(), case["value"], "decode {name}");
        }
    }
}

#[test]
fn raw_json_passthrough_matches_typescript_bytes() {
    let fixture = fixture();
    for case in fixture["values"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        // 数组走 Object 标签是有意的偏差（值等价），其余与 TS 逐字节一致。
        if is_undefined(&case["value"]) || case["value"].is_array() {
            continue;
        }
        let raw = serde_json::to_string(&case["value"]).unwrap();
        let mut out = Vec::new();
        codec::write_raw_json(&mut out, &raw).unwrap();
        assert_eq!(hex(&out), case["hex"].as_str().unwrap(), "raw {name}");
    }
}

#[test]
fn bytes_match_typescript_bytes() {
    let bytes = &fixture()["bytes"];
    let mut out = Vec::new();
    codec::write_bytes(&mut out, &[0, 1, 2, 250, 255]);
    assert_eq!(hex(&out), bytes["topLevel"].as_str().unwrap());

    let nested = Reader::new(&unhex(bytes["nested"].as_str().unwrap()))
        .value()
        .unwrap()
        .into_json();
    assert_eq!(
        nested,
        json!({"data": {"__zcode_rpc_nested_uint8array_v1": true, "base64": "aGkh"}})
    );
    assert_eq!(codec::base64_encode(b"hi!"), "aGkh");
    assert_eq!(codec::base64_encode(b"hi"), "aGk=");
    assert_eq!(codec::base64_encode(b"h"), "aA==");
}

#[test]
fn responses_match_typescript_bytes() {
    let messages = &fixture()["messages"];
    let expect = |name: &str| messages[name].as_str().unwrap().to_owned();
    assert_eq!(hex(&codec::initialize()), expect("initialize"));
    assert_eq!(
        hex(&codec::success_undefined(8)),
        expect("successUndefined")
    );
    assert_eq!(
        hex(&codec::success_value(7, &json!({"ok": true, "n": 3}))),
        expect("successObject")
    );
    assert_eq!(
        hex(&codec::promise_error(9, &RpcError::new("Error", "boom"))),
        expect("promiseError")
    );
    assert_eq!(
        hex(&codec::promise_error(
            10,
            &RpcError::new("NotImplemented", "not implemented: setting.get")
                .with_code("fault.host.notImplemented"),
        )),
        expect("promiseErrorWithCode")
    );
    assert_eq!(
        hex(&codec::raw_response(
            codec::RESPONSE_EVENT_FIRE,
            300,
            r#"{"topic":"conversation/s1","subscriptionId":"x"}"#
        )
        .unwrap()),
        expect("eventFire")
    );
}

#[test]
fn requests_decode_from_typescript_bytes() {
    let messages = &fixture()["messages"];
    let decode = |name: &str| read_request(&unhex(messages[name].as_str().unwrap())).unwrap();

    let promise = decode("requestPromise");
    assert_eq!(promise.kind, RequestKind::Promise);
    assert_eq!((promise.id, promise.channel.as_str()), (5, "setting"));
    assert_eq!(promise.name, "get");
    assert_eq!(promise.body, RpcValue::Array(vec![]));

    let call = decode("requestCall");
    assert_eq!(
        call.body.into_json(),
        json!([{"path": "/tmp/a.txt", "length": 10}])
    );

    let listen = decode("requestListen");
    assert_eq!(listen.kind, RequestKind::EventListen);
    assert_eq!(listen.name, "onDynamicConversationFrame");
    assert_eq!(listen.body.into_json(), json!({"workspacePath": "/w"}));

    let cancel = decode("requestCancel");
    assert_eq!((cancel.kind, cancel.id), (RequestKind::PromiseCancel, 6));
    let dispose = decode("requestDispose");
    assert_eq!((dispose.kind, dispose.id), (RequestKind::EventDispose, 7));
}

#[test]
fn frame_reader_reassembles_split_and_merged_chunks() {
    let first = frame(b"one");
    let second = frame(&[7u8; 300]);
    let mut stream = first.clone();
    stream.extend_from_slice(&second);

    let mut reader = FrameReader::default();
    reader.push(&stream[..5]);
    assert_eq!(reader.next_message(), None);
    reader.push(&stream[5..first.len() + 20]);
    assert_eq!(reader.next_message(), Some(b"one".to_vec()));
    assert_eq!(reader.next_message(), None);
    reader.push(&stream[first.len() + 20..]);
    assert_eq!(reader.next_message(), Some(vec![7u8; 300]));
    assert_eq!(reader.next_message(), None);
}

#[test]
fn frame_reader_skips_non_regular_frames() {
    let mut ack = frame(b"x");
    ack[0] = 3;
    let mut reader = FrameReader::default();
    reader.push(&ack);
    reader.push(&frame(b"y"));
    assert_eq!(reader.next_message(), Some(b"y".to_vec()));
}
