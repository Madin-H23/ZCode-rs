use super::*;
use crate::session::ForegroundRun;

fn can_edit() -> Value {
    json!({"canEdit": true, "editDisposition": "rewind"})
}

/// 一轮进行中：轮次头、realUser 输入与流式中的回复。
fn streaming_turn() -> Session {
    let mut s = Session::new(
        "s".into(),
        "w".into(),
        "p".into(),
        "m".into(),
        "none".into(),
        "e".into(),
        1,
    );
    let mut header = s.row("turnHeader", "t1", "t1", 1);
    header["state"] = "running".into();
    s.rows.push(header);
    let mut input = s.row("userInput", "t1", "u1", 1);
    input["origin"] = "realUser".into();
    s.rows.push(input);
    s.history.inputs.push(InputBoundary {
        entity: "u1".into(),
        turn: "t1".into(),
        row: 0,
        user_row: 1,
        message: 0,
        state: State::capture(&s),
        kind: "sendText".into(),
        payload: json!({"text": "hi"}),
        node_message: None,
    });
    let mut reply = s.row("assistantText", "t1", "a1", 2);
    reply["state"] = "streaming".into();
    s.rows.push(reply);
    s.run = Some(ForegroundRun {
        id: "run".into(),
        started_at: 1,
    });
    s
}

fn actions(s: &Session) -> Vec<Value> {
    s.rows.iter().map(|r| r["actions"].clone()).collect()
}

/// Node `materializeCommandRowActions`：运行中只有最新 realUser 输入可编辑；回复行在这一轮结束、
/// 且状态为 complete 时才可重试，稳定的回复同时可分支。
#[test]
fn the_reply_offers_retry_and_fork_only_after_its_turn_ends() {
    let mut s = streaming_turn();
    s.history_actions();
    assert_eq!(actions(&s), [Value::Null, can_edit(), Value::Null]);

    // 模型提交了稳定回复，这一轮仍在运行。
    s.rows[2]["state"] = "complete".into();
    s.history.responses.push(ResponseBoundary {
        entity: "a1".into(),
        turn: "t1".into(),
        row: 2,
        message: 0,
        state: State::capture(&s),
        node_message: None,
    });
    s.history_actions();
    assert_eq!(actions(&s), [Value::Null, can_edit(), Value::Null]);

    s.run = None;
    s.finish_rows("success", 3);
    s.history_actions();
    assert_eq!(
        actions(&s),
        [
            Value::Null,
            can_edit(),
            json!({"canRetry": true, "canFork": true})
        ]
    );
}

#[test]
fn an_interrupted_reply_is_not_retryable() {
    let mut s = streaming_turn();
    s.run = None;
    s.finish_rows("interrupted", 3);
    s.history_actions();
    assert_eq!(s.rows[2]["state"], "interrupted");
    assert_eq!(actions(&s), [Value::Null, can_edit(), Value::Null]);
}
