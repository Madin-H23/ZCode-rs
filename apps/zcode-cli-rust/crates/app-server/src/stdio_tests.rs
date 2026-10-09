use super::*;

fn dispatched(line: &str) -> Input {
    let (tx, mut rx) = mpsc::channel(1);
    assert!(dispatch(&tx, line.as_bytes()));
    rx.try_recv().unwrap()
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
