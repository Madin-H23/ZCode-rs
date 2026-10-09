//! Connections: backlog pause and recovery, flow pause, close and pins.
use crate::support::{Frame, Harness, Sink, conversation, deltas, frames, snapshot, text};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use zcode_cli_core_api::{Method, RuntimeError};
use zcode_cli_domain::delivery::Profile;

const TOPIC: &str = "conversation/s";
const INDEX: &str = "sessions-index/w";

/// desktop 与 mobile 订阅同一 topic，mobile 的 sink 在第一次刷新前报告积压。
fn paused_mobile() -> (Harness, Arc<Sink>, String, String) {
    let mut h = Harness::default();
    let (desktop, _) = h.connection("desktop");
    let (mobile, mobile_sink) = h.connection("mobile");
    let c = h.subscribed(&desktop, Profile::Continuous, TOPIC, snapshot(0));
    let m = h.subscribed(&mobile, Profile::Continuous, TOPIC, snapshot(0));
    h.event(conversation(0, 1, &[text("a")], vec![]), 0);
    mobile_sink.pause();
    let sent = frames(
        "desktop",
        TOPIC,
        vec![deltas(&c, "online", (0, 1), json!([text("a")]))],
    );
    assert_eq!(h.flush(30), vec![sent], "only the desktop receives");
    (h, mobile_sink, c, m)
}

/// 积压的连接停止接收、丢弃缓冲并等待快照恢复；同一 topic 的其他连接照常接收。
#[test]
fn a_paused_connection_stops_and_is_marked_for_resync_while_others_keep_receiving() {
    let (mut h, _, c, _) = paused_mobile();
    h.event(conversation(1, 2, &[text("b")], vec![]), 40);
    assert_eq!(h.due_ms(), Some(70), "only the desktop has a window");
    let sent = frames(
        "desktop",
        TOPIC,
        vec![deltas(&c, "online", (1, 2), json!([text("b")]))],
    );
    assert_eq!(h.flush(1000), vec![sent]);
    assert!(
        h.runtime.requests.is_empty(),
        "a paused connection requests no snapshot"
    );
}

/// 积压解除后，需要恢复的订阅立即到期，经快照恢复，之后的增量照常接续。
#[test]
fn resuming_a_paused_connection_recovers_it_by_snapshot() {
    let (mut h, mobile_sink, c, m) = paused_mobile();
    h.event(conversation(1, 2, &[text("b")], vec![]), 40);
    h.flush(70);
    h.delivery.relieve(h.t0);
    assert_eq!(h.due_ms(), None, "still paused");
    mobile_sink.unpause();
    h.delivery
        .relieve(h.t0 + std::time::Duration::from_millis(2000));
    assert_eq!(h.due_ms(), Some(2000));
    assert_eq!(h.flush(2000), vec![]);
    let request = h.reply(Ok(snapshot(2)));
    assert_eq!(request, (Method::TopicSnapshot, json!({"topic":TOPIC})));
    let recovered = Frame {
        delivery: "online".into(),
        subscription: m.clone(),
        range: (0, 2),
        payload: json!({"kind":"snapshot","snapshot":{"seq":2}}),
        ttft: None,
    };
    assert_eq!(h.seen(), vec![frames("mobile", TOPIC, vec![recovered])]);

    h.event(conversation(2, 3, &[text("c")], vec![]), 2000);
    let next = json!([text("c")]);
    assert_eq!(
        h.flush(2030),
        vec![
            frames(
                "desktop",
                TOPIC,
                vec![deltas(&c, "online", (2, 3), next.clone())]
            ),
            frames("mobile", TOPIC, vec![deltas(&m, "online", (2, 3), next)]),
        ]
    );
}

/// 拥塞期间才登记的订阅、拥塞期间经历史重置拿到新快照的订阅，跳过增量后同样要在拥塞解除时
/// 经快照恢复，而不是停在旧状态等下一次增量（修复前失败）。
#[test]
fn subscriptions_that_skip_deltas_while_paused_recover_after_relief() {
    let mut h = Harness::default();
    let (desktop, desktop_sink) = h.connection("desktop");
    let (mobile, mobile_sink) = h.connection("mobile");
    let c = h.subscribed(&desktop, Profile::Continuous, TOPIC, snapshot(0));
    desktop_sink.pause();
    mobile_sink.pause();
    assert_eq!(h.flush(0), vec![]);
    let m = h.subscribed(&mobile, Profile::Replayable, TOPIC, snapshot(0));
    let reset = zcode_cli_core_api::RuntimeEvent::ConversationReset {
        session: "s".into(),
        seq: 0,
        snapshot: json!({"seq":0}),
    };
    h.event(reset, 0);
    let reset = |subscription: &str| Frame {
        delivery: "recovery".into(),
        subscription: subscription.into(),
        range: (0, 0),
        payload: json!({"kind":"snapshot","snapshot":{"seq":0}}),
        ttft: None,
    };
    assert_eq!(
        h.seen(),
        vec![
            frames("desktop", TOPIC, vec![reset(&c)]),
            frames("mobile", TOPIC, vec![reset(&m)]),
        ],
        "a history reset still goes to paused connections"
    );
    h.event(conversation(0, 1, &[text("a")], vec![]), 0);
    assert_eq!(h.flush(1000), vec![]);

    desktop_sink.unpause();
    mobile_sink.unpause();
    h.delivery
        .relieve(h.t0 + std::time::Duration::from_millis(1000));
    assert_eq!(h.flush(1000), vec![]);
    let recovered = |subscription: &str| Frame {
        delivery: "online".into(),
        subscription: subscription.into(),
        range: (0, 1),
        payload: json!({"kind":"snapshot","snapshot":{"seq":1}}),
        ttft: None,
    };
    h.reply(Ok(snapshot(1)));
    h.reply(Ok(snapshot(1)));
    assert_eq!(
        h.seen(),
        vec![
            frames("desktop", TOPIC, vec![recovered(&c)]),
            frames("mobile", TOPIC, vec![recovered(&m)]),
        ]
    );
}

/// `saturated` 只暂停刷新、保留缓冲，`drained` 后直接发增量，不需要快照。
#[test]
fn a_saturated_connection_keeps_its_buffer_until_drained() {
    let mut h = Harness::default();
    let (mobile, _) = h.connection("mobile");
    let m = h.subscribed(&mobile, Profile::Replayable, TOPIC, snapshot(0));
    h.delivery.saturated("mobile");
    h.event(conversation(0, 1, &[text("a")], vec![]), 0);
    assert_eq!(h.due_ms(), None);
    assert_eq!(h.flush(1000), vec![]);
    h.delivery
        .drained("mobile", h.t0 + std::time::Duration::from_millis(1000));
    let sent = frames(
        "mobile",
        TOPIC,
        vec![deltas(&m, "online", (0, 1), json!([text("a")]))],
    );
    assert_eq!(h.flush(1000), vec![sent]);
    assert!(h.runtime.requests.is_empty());
}

/// 关闭连接释放它每个订阅的 pin；topic 的最后一个订阅结束后 runtime 不再保留它。
#[test]
fn closing_a_connection_releases_its_pins_and_the_last_subscriber_releases_the_topic() {
    let mut h = Harness::default();
    let (a, _) = h.connection("a");
    let (b, _) = h.connection("b");
    h.subscribed(&a, Profile::Continuous, TOPIC, snapshot(0));
    h.subscribed(&a, Profile::Continuous, INDEX, snapshot(0));
    let kept = h.subscribed(&b, Profile::Replayable, TOPIC, snapshot(0));
    let pins =
        |topic: i64, index: i64| BTreeMap::from([(TOPIC.into(), topic), (INDEX.into(), index)]);
    assert_eq!(h.runtime.pins, pins(2, 1));

    h.delivery.close_connection("a", &mut h.runtime);
    assert_eq!(h.runtime.released, vec![TOPIC.to_owned(), INDEX.to_owned()]);
    assert_eq!(h.runtime.pins, pins(1, 0));
    h.event(conversation(0, 1, &[text("a")], vec![]), 0);
    let sent = frames(
        "b",
        TOPIC,
        vec![deltas(&kept, "online", (0, 1), json!([text("a")]))],
    );
    assert_eq!(
        h.flush(150),
        vec![sent],
        "the other connection keeps receiving"
    );

    let not_owned = RuntimeError::Fault {
        message: "fault.subscription.notOwned".into(),
        code: None,
    };
    let refused = h.delivery.unsubscribe("a", &kept, &mut h.runtime);
    assert_eq!(refused, Err(not_owned));
    h.delivery.unsubscribe("b", &kept, &mut h.runtime).unwrap();
    assert_eq!(h.runtime.pins, pins(0, 0));
    h.event(conversation(1, 2, &[text("b")], vec![]), 200);
    assert_eq!(
        h.flush(1000),
        vec![],
        "an ended subscription receives nothing"
    );
    h.delivery.close_connection("b", &mut h.runtime);
    assert_eq!(h.runtime.pins, pins(0, 0));
}
