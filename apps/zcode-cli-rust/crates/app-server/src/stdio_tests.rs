use super::*;

fn dispatched(line: &str) -> Input {
    let (tx, mut rx) = mpsc::channel(1);
    assert!(dispatch(&tx, line.as_bytes()));
    rx.try_recv().unwrap()
}

/// 异步读端与 stdin 线程使用同一套分帧：按行、跳过空行、EOF 后发 Eof，并触发 input_closed。
#[tokio::test]
async fn async_readers_frame_lines_like_stdin() {
    let (mut host, runtime) = tokio::io::duplex(64);
    let closed = CancellationToken::new();
    let mut input = start_reader(tokio::io::BufReader::new(runtime), closed.clone());
    tokio::io::AsyncWriteExt::write_all(
        &mut host,
        b"{\"id\":\"a\",\"result\":1}\n\n{\"id\":1,\"method\":\"runtime/capabilities\",\"params\":{}}\nnot json\n",
    )
    .await
    .unwrap();
    drop(host);
    assert!(matches!(input.recv().await, Some(Input::Response { id, result: Ok(_) }) if id == "a"));
    assert!(
        matches!(input.recv().await, Some(Input::Request(r)) if r.method == "runtime/capabilities")
    );
    assert!(matches!(input.recv().await, Some(Input::Invalid)));
    assert!(matches!(input.recv().await, Some(Input::Eof)));
    assert!(closed.is_cancelled());
}

#[test]
fn host_error_replies_reach_the_waiter_as_errors() {
    let input = dispatched(
        r#"{"id":"rust-auth-1","error":{"code":-32004,"message":"account.notLoggedIn","data":{"x":1}}}"#,
    );
    let Input::Response { id, result } = input else {
        panic!("expected a response");
    };
    assert_eq!(id, "rust-auth-1");
    assert_eq!(
        result,
        Err(HostError {
            code: -32004,
            message: "account.notLoggedIn".into(),
            data: Some(json!({"x": 1})),
        })
    );
}

#[test]
fn host_result_replies_stay_values() {
    let Input::Response { result, .. } = dispatched(r#"{"id":"rust-auth-2","result":null}"#) else {
        panic!("expected a response");
    };
    assert_eq!(result, Ok(Value::Null));
}
