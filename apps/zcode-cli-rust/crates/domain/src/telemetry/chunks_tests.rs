use super::*;
use serde_json::json;

/// A `stream.chunk` fact of turn `t1` the way the normalizer builds it.
fn chunk(session: &str, seq: u64, (channel, message): (&str, &str), first: bool) -> Value {
    json!({"version": 1, "eventId": format!("e{seq}"), "eventSeq": seq, "occurredAt": 1000 + seq,
        "sessionId": session, "sourceCommandId": "c1", "turnId": "t1", "kind": "stream.chunk",
        "channel": channel, "chunkLength": 3, "firstChunk": first, "assistantMessageId": message})
}

fn fact(session: &str, seq: u64, kind: &str, extra: Value) -> Value {
    let mut fact = json!({"version": 1, "eventId": format!("e{seq}"), "eventSeq": seq,
        "occurredAt": 1000 + seq, "sessionId": session, "sourceCommandId": "c1", "turnId": "t1",
        "kind": kind});
    fact.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    fact
}

const TEXT: (&str, &str) = ("text", "m1");

/// What each sent fact covers: `[session, kind, eventSeq, chunkCount, chunkLength, lastChunkAgeMs]`.
fn summary(facts: &[Value]) -> Vec<Value> {
    facts
        .iter()
        .map(|f| {
            json!([
                f["sessionId"],
                f["kind"],
                f["eventSeq"],
                f["chunkCount"],
                f["chunkLength"],
                f["lastChunkAgeMs"]
            ])
        })
        .collect()
}

/// The Engine around the window: it wakes at the deadline before the next event.
struct Clock {
    window: ChunkWindow,
    start: Instant,
    sent: Vec<Value>,
}

impl Clock {
    fn new() -> Self {
        Self {
            window: ChunkWindow::default(),
            start: Instant::now(),
            sent: Vec::new(),
        }
    }
    fn at(&self, ms: u64) -> Instant {
        self.start + Duration::from_millis(ms)
    }
    fn wake_until(&mut self, ms: u64) {
        while let Some(due) = self.window.deadline().filter(|due| *due <= self.at(ms)) {
            let released = self.window.due(due);
            assert!(!released.is_empty());
            self.sent.extend(released);
        }
    }
    fn admit(&mut self, ms: u64, fact: Value) {
        self.wake_until(ms);
        let now = self.at(ms);
        let out = self.window.admit(fact, now);
        self.sent.extend(out);
    }
}

/// 40 deltas 20 ms apart: the first goes out at once, the rest once per
/// 250 ms window, and the request's completion releases the tail first.
#[test]
fn a_streaming_turn_sends_the_first_chunk_then_one_fact_per_window() {
    let mut clock = Clock::new();
    for i in 0..40 {
        clock.admit(i * 20, chunk("s1", i + 1, TEXT, i == 0));
    }
    clock.admit(
        790,
        fact(
            "s1",
            41,
            "model.request.status",
            json!({"status": "model_request_completed"}),
        ),
    );
    assert_eq!(
        summary(&clock.sent),
        vec![
            json!(["s1", "stream.chunk", 1, null, 3, null]),
            json!(["s1", "stream.chunk", 13, 12, 36, 10]),
            json!(["s1", "stream.chunk", 25, 12, 36, 20]),
            json!(["s1", "stream.chunk", 38, 13, 39, 10]),
            json!(["s1", "stream.chunk", 40, 2, 6, 10]),
            json!(["s1", "model.request.status", 41, null, null, null]),
        ]
    );
    // A merged fact is the newest delta's fact with the sums and its age.
    let mut merged = chunk("s1", 13, TEXT, false);
    merged["chunkLength"] = 36.into();
    merged["chunkCount"] = 12.into();
    merged["lastChunkAgeMs"] = 10.into();
    assert_eq!(clock.sent[1], merged);
    assert_eq!(clock.window.deadline(), None);
}

/// The first chunk, a switch of channel or message, the first chunk after
/// another fact of the session and one after 250 ms of silence go out at once.
#[test]
fn chunks_that_open_something_go_out_at_once() {
    let mut clock = Clock::new();
    clock.admit(0, chunk("s1", 1, ("thought", "m1"), true));
    clock.admit(20, chunk("s1", 2, ("thought", "m1"), false));
    // 换通道：先发暂存的思考分片，正文分片立即发出（不靠 firstChunk）。
    clock.admit(40, chunk("s1", 3, TEXT, false));
    clock.admit(60, chunk("s1", 4, TEXT, false));
    // 另一个会话的分片不影响 s1 的暂存。
    clock.admit(70, chunk("s2", 1, TEXT, true));
    // 换 assistant 消息（新的 step）。
    clock.admit(80, chunk("s1", 5, ("text", "m2"), false));
    // 中间夹了一条别的事实（工具开始）：之后的分片立即发出。
    clock.admit(
        90,
        fact("s1", 6, "tool.lifecycle", json!({"phase": "started"})),
    );
    clock.admit(100, chunk("s1", 7, ("text", "m2"), false));
    clock.admit(120, chunk("s1", 8, ("text", "m2"), false));
    // 静默超过 250 ms（暂存在 350 定时发出）后的分片立即发出。
    clock.admit(700, chunk("s1", 9, ("text", "m2"), false));
    assert_eq!(
        summary(&clock.sent),
        vec![
            json!(["s1", "stream.chunk", 1, null, 3, null]),
            json!(["s1", "stream.chunk", 2, 1, 3, 20]),
            json!(["s1", "stream.chunk", 3, null, 3, null]),
            json!(["s2", "stream.chunk", 1, null, 3, null]),
            json!(["s1", "stream.chunk", 4, 1, 3, 20]),
            json!(["s1", "stream.chunk", 5, null, 3, null]),
            json!(["s1", "tool.lifecycle", 6, null, null, null]),
            json!(["s1", "stream.chunk", 7, null, 3, null]),
            json!(["s1", "stream.chunk", 8, 1, 3, 230]),
            json!(["s1", "stream.chunk", 9, null, 3, null]),
        ]
    );
}

/// The end of a turn (success, stop or failure) and a failed request release
/// the held chunks before their own fact.
#[test]
fn other_facts_release_the_held_chunks_first() {
    for (kind, extra) in [
        ("turn.terminal", json!({"status": "success"})),
        (
            "turn.terminal",
            json!({"status": "interrupted", "errorCode": "USER_INTERRUPT"}),
        ),
        (
            "turn.terminal",
            json!({"status": "failed", "errorCode": "MODEL_ERROR"}),
        ),
        (
            "model.request.status",
            json!({"status": "model_request_failed"}),
        ),
    ] {
        let mut clock = Clock::new();
        clock.admit(0, chunk("s1", 1, TEXT, true));
        clock.admit(20, chunk("s1", 2, TEXT, false));
        clock.admit(40, chunk("s1", 3, TEXT, false));
        clock.admit(45, fact("s1", 4, kind, extra));
        assert_eq!(
            summary(&clock.sent),
            vec![
                json!(["s1", "stream.chunk", 1, null, 3, null]),
                json!(["s1", "stream.chunk", 3, 2, 6, 5]),
                json!(["s1", kind, 4, null, null, null]),
            ],
            "{kind}"
        );
        assert_eq!(clock.window.deadline(), None);
    }
}

/// Held facts of several sessions go out in the order they started, both on
/// a flush and when their windows close.
#[test]
fn held_facts_of_several_sessions_keep_their_order() {
    let mut clock = Clock::new();
    for (ms, session, seq, first) in [
        (0, "s2", 1, true),
        (5, "s1", 1, true),
        (10, "s2", 2, false),
        (15, "s1", 2, false),
    ] {
        clock.admit(ms, chunk(session, seq, TEXT, first));
    }
    assert_eq!(clock.window.deadline(), Some(clock.at(250)));
    assert_eq!(clock.window.due(clock.at(249)), Vec::<Value>::new());
    let flushed = clock.window.flush(clock.at(100));
    assert_eq!(
        summary(&flushed),
        vec![
            json!(["s2", "stream.chunk", 2, 1, 3, 90]),
            json!(["s1", "stream.chunk", 2, 1, 3, 85]),
        ]
    );
    clock.sent.clear();
    clock.admit(110, chunk("s1", 3, TEXT, false));
    clock.admit(120, chunk("s2", 3, TEXT, false));
    clock.wake_until(400);
    assert_eq!(
        summary(&clock.sent),
        vec![
            json!(["s1", "stream.chunk", 3, 1, 3, 240]),
            json!(["s2", "stream.chunk", 3, 1, 3, 230]),
        ]
    );
}
