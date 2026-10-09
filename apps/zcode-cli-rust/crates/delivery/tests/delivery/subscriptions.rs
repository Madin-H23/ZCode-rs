//! Subscribe, resync, flush windows and the two profiles.
use crate::support::{Frame, Harness, Seen, conversation, deltas, frames, input, snapshot, text};
use serde_json::{Value, json};
use zcode_cli_core_api::{Method, RuntimeError, RuntimeEvent};
use zcode_cli_delivery::Resync;
use zcode_cli_domain::delivery::{Profile, json_bytes};

const TOPIC: &str = "conversation/s";
const INDEX: &str = "sessions-index/w";

fn ack(subscription: &str, mode: &str) -> Value {
    json!({"ack":{"subscriptionId":subscription,"mode":mode,"logEpoch":"e"}})
}

fn answered(seen: &Seen) -> String {
    let Seen::Answer(_, Ok(ack)) = seen else {
        panic!("expected an ACK: {seen:?}");
    };
    ack["ack"]["subscriptionId"].as_str().unwrap().into()
}

/// 订阅在 runtime 回复后登记：先交出 ACK，再推送初始帧；replayable 的回放先过滤再合并。
#[test]
fn subscribe_answers_before_pushing_its_initial_frames() {
    let mut h = Harness::default();
    let (desktop, _) = h.connection("desktop");
    let (mobile, _) = h.connection("mobile");
    let (aligned, _) = h.connection("aligned");
    let base = json!({"logEpoch":"e","seq":1});
    let desktop_request = h.subscribe(&desktop, Profile::Continuous, TOPIC, Value::Null);
    let mobile_request = h.subscribe(&mobile, Profile::Replayable, TOPIC, base.clone());
    let aligned_request = h.subscribe(&aligned, Profile::Replayable, TOPIC, base);
    assert_eq!(h.seen(), vec![], "nothing before the runtime replies");

    let opened = h.reply(Ok(snapshot(3)));
    let open = json!({"topic":TOPIC,"base":null,"workspace":{"workspacePath":"/w"}});
    assert_eq!(opened, (Method::TopicOpen, open));
    let replay = json!([text("a"), input("i"), text("b")]);
    h.reply(Ok(
        json!({"epoch":"e","seq":3,"mode":"resume","from":1,"deltas":replay}),
    ));
    h.reply(Ok(
        json!({"epoch":"e","seq":3,"mode":"resume","from":3,"deltas":[]}),
    ));
    let seen = h.seen();
    let (d, m, a) = (answered(&seen[0]), answered(&seen[2]), answered(&seen[4]));
    let initial = Frame {
        delivery: "initial".into(),
        subscription: d.clone(),
        range: (0, 3),
        payload: json!({"kind":"snapshot","snapshot":{"seq":3}}),
        ttft: None,
    };
    let replayed = deltas(&m, "initial", (1, 3), json!([text("ab")]));
    assert_eq!(
        seen,
        vec![
            Seen::Answer(desktop_request, Ok(ack(&d, "snapshot"))),
            frames("desktop", TOPIC, vec![initial]),
            Seen::Answer(mobile_request, Ok(ack(&m, "resume"))),
            frames("mobile", TOPIC, vec![replayed]),
            // 已对齐的 resume 只有 ACK，没有初始帧。
            Seen::Answer(aligned_request, Ok(ack(&a, "resume"))),
        ]
    );
}

/// 刷新窗口：continuous 30 ms、replayable 150 ms，sessions-index 变更后立即刷新。
#[test]
fn flush_windows_follow_the_profile_and_index_topics_flush_at_once() {
    let mut h = Harness::default();
    let (desktop, _) = h.connection("desktop");
    let (mobile, _) = h.connection("mobile");
    let c = h.subscribed(&desktop, Profile::Continuous, TOPIC, snapshot(0));
    let m = h.subscribed(&mobile, Profile::Replayable, TOPIC, snapshot(0));
    let index = h.subscribed(&desktop, Profile::Continuous, INDEX, snapshot(0));
    h.event(conversation(0, 1, &[text("a")], vec![]), 0);
    h.event(conversation(1, 2, &[text("b")], vec![]), 10);
    assert_eq!(
        h.due_ms(),
        Some(30),
        "the window starts with the first delta"
    );
    assert_eq!(h.flush(29), vec![]);
    let ab = json!([text("ab")]);
    let sent = frames(
        "desktop",
        TOPIC,
        vec![deltas(&c, "online", (0, 2), ab.clone())],
    );
    assert_eq!(h.flush(30), vec![sent]);
    assert_eq!(h.due_ms(), Some(150));
    assert_eq!(h.flush(149), vec![]);
    let sent = frames("mobile", TOPIC, vec![deltas(&m, "online", (0, 2), ab)]);
    assert_eq!(h.flush(150), vec![sent]);

    let removed = json!({"op":"session.removed","sessionId":"s"});
    let changed = RuntimeEvent::IndexChanged {
        workspace: "w".into(),
        from: 0,
        to: 1,
        deltas: [(removed.clone(), json_bytes(&removed))].into(),
    };
    h.event(changed, 200);
    assert_eq!(h.due_ms(), Some(200));
    let sent = frames(
        "desktop",
        INDEX,
        vec![deltas(&index, "online", (0, 1), json!([removed]))],
    );
    assert_eq!(h.flush(200), vec![sent]);
}

/// replayable 只收 `text` 流，被过滤的 seq 仍在区间内；TTFT 只附在 continuous 的 online 帧上。
#[test]
fn replayable_connections_get_filtered_deltas_and_only_continuous_ones_get_ttft() {
    let mut h = Harness::default();
    let (desktop, _) = h.connection("desktop");
    let (mobile, _) = h.connection("mobile");
    let c = h.subscribed(&desktop, Profile::Continuous, TOPIC, snapshot(0));
    let m = h.subscribed(&mobile, Profile::Replayable, TOPIC, snapshot(0));
    let ttft = json!({"turnId":"t","firstTokenMs":5});
    let published = [text("a"), input("i"), text("b")];
    h.event(conversation(0, 3, &published, vec![ttft.clone()]), 0);
    let all = deltas(&c, "online", (0, 3), json!(published));
    let streamed = Frame {
        ttft: Some(ttft),
        ..all
    };
    let filtered = deltas(&m, "online", (0, 3), json!([text("ab")]));
    assert_eq!(
        h.flush(150),
        vec![
            frames("desktop", TOPIC, vec![streamed]),
            frames("mobile", TOPIC, vec![filtered]),
        ]
    );
}

/// resync 只接受订阅所属的连接；回复覆盖在途期间的增量，ACK 先于 recovery 帧。
#[test]
fn resync_answers_then_pushes_recovery_and_rejects_other_connections() {
    let mut h = Harness::default();
    let (desktop, _) = h.connection("desktop");
    let c = h.subscribed(&desktop, Profile::Continuous, TOPIC, snapshot(2));
    let base = json!({"logEpoch":"e","seq":2});
    let resync = |connection: &str| Resync {
        connection: connection.into(),
        subscription: c.clone(),
        topic: TOPIC.into(),
        base: base.clone(),
        force_snapshot: Value::Null,
    };
    let refused = h.delivery.resync(resync("mobile"), 9, &mut h.runtime);
    let not_owned = RuntimeError::Fault {
        message: "fault.subscription.notOwned".into(),
        code: None,
    };
    assert_eq!(refused, Err(not_owned));
    assert!(h.runtime.requests.is_empty());

    h.event(conversation(2, 3, &[text("x")], vec![]), 0);
    h.delivery
        .resync(resync("desktop"), 7, &mut h.runtime)
        .unwrap();
    h.event(conversation(3, 4, &[text("y")], vec![]), 0);
    assert_eq!(h.flush(1000), vec![], "the reply covers what is in flight");
    let state =
        json!({"epoch":"e","seq":4,"mode":"resume","from":2,"deltas":[text("x"), text("y")]});
    let snapshot_request = json!({"topic":TOPIC,"base":base,"forceSnapshot":null});
    assert_eq!(
        h.reply(Ok(state)),
        (Method::TopicSnapshot, snapshot_request)
    );
    let recovery = deltas(&c, "recovery", (2, 4), json!([text("xy")]));
    assert_eq!(
        h.seen(),
        vec![
            Seen::Answer(7, Ok(ack(&c, "resume"))),
            frames("desktop", TOPIC, vec![recovery]),
        ]
    );
}
