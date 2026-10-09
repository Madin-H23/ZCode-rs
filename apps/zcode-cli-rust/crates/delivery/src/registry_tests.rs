use super::*;
use crate::domain::delivery;
use crate::sink::{ConnectionSink, FrameRoute};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TOPIC: &str = "conversation/s";

/// Every connection's pushes, in order.
#[derive(Default)]
struct Capture {
    lines: Mutex<Vec<String>>,
    paused: AtomicBool,
}

impl ConnectionSink for Capture {
    fn push(&self, _route: FrameRoute<'_>, frames: Vec<String>) {
        self.lines.lock().unwrap().extend(frames);
    }
    fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
}

impl Capture {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.lines.lock().unwrap())
    }
}

fn target(sink: &Arc<Capture>, topic: &str, connection: &str, profile: Profile) -> Target {
    Target {
        topic: topic.into(),
        connection: Connection {
            id: connection.into(),
            sink: sink.clone(),
        },
        profile,
        workspace_key: String::new(),
    }
}
fn frames(lines: &[String]) -> Vec<Value> {
    lines
        .iter()
        .map(|l| {
            let wire = serde_json::from_str::<Value>(l).unwrap();
            let mut frame = wire["frame"].clone();
            frame["deliveryKind"] = wire["deliveryKind"].clone();
            frame
        })
        .collect()
}
fn snapshot(seq: u64) -> Value {
    json!({"epoch":"e","seq":seq,"mode":"snapshot","snapshot":{"seq":seq}})
}
fn text(append: &str) -> (Value, usize) {
    let delta = json!({"op":"row.delta","rowId":1,"path":"text","append":append});
    let size = delivery::json_bytes(&delta);
    (delta, size)
}
fn input(append: &str) -> (Value, usize) {
    let delta = json!({"op":"row.delta","rowId":2,"path":"inputText","append":append});
    let size = delivery::json_bytes(&delta);
    (delta, size)
}
fn range(frame: &Value) -> (u64, u64) {
    (
        frame["fromSeq"].as_u64().unwrap(),
        frame["toSeq"].as_u64().unwrap(),
    )
}
fn subscribe(
    d: &mut Delivery,
    sink: &Arc<Capture>,
    connection: &str,
    profile: Profile,
    state: &Value,
) -> Subscribed {
    d.register(&target(sink, TOPIC, connection, profile), state)
        .unwrap()
}
/// Frames pushed by a flush at `now`, and the recoveries it requested.
fn flush(d: &mut Delivery, sink: &Capture, now: Instant) -> (Vec<String>, Vec<(String, String)>) {
    let out = d.flush_due(now);
    (sink.take(), out.recover)
}

#[test]
fn deltas_are_buffered_until_the_window_ends_and_coalesced() {
    let (mut d, sink) = (Delivery::default(), Arc::new(Capture::default()));
    let t0 = Instant::now();
    let sub = subscribe(&mut d, &sink, "c", Profile::Continuous, &snapshot(5));
    assert_eq!(sub.mode, "snapshot");
    assert_eq!(frames(&sub.lines)[0]["deliveryKind"], "initial");
    // 快照之前产生的增量已包含在快照内。
    d.deltas(TOPIC, (3, 5), &[text("old")], t0);
    d.deltas(TOPIC, (5, 6), &[text("a")], t0);
    d.deltas(TOPIC, (6, 7), &[text("b")], t0 + Duration::from_millis(10));
    assert!(
        flush(&mut d, &sink, t0 + Duration::from_millis(29))
            .0
            .is_empty()
    );
    assert_eq!(d.next_due(), Some(t0 + Duration::from_millis(30)));
    let sent = frames(&flush(&mut d, &sink, t0 + Duration::from_millis(30)).0);
    assert_eq!(range(&sent[0]), (5, 7));
    assert_eq!(sent[0]["deliveryKind"], "online");
    assert_eq!(sent[0]["payload"]["deltas"], json!([text("ab").0]));
    assert!(
        flush(&mut d, &sink, t0 + Duration::from_secs(1))
            .0
            .is_empty()
    );
}

#[test]
fn replayable_subscribers_keep_seq_continuity_over_filtered_deltas() {
    let (mut d, sink) = (Delivery::default(), Arc::new(Capture::default()));
    let t0 = Instant::now();
    subscribe(&mut d, &sink, "m", Profile::Replayable, &snapshot(0));
    d.deltas(TOPIC, (0, 1), &[input("x")], t0);
    assert!(
        flush(&mut d, &sink, t0 + Duration::from_millis(149))
            .0
            .is_empty()
    );
    let sent = frames(&flush(&mut d, &sink, t0 + Duration::from_millis(150)).0);
    assert_eq!(range(&sent[0]), (0, 1));
    assert_eq!(sent[0]["payload"]["deltas"], json!([]));
}

#[test]
fn gaps_recover_by_snapshot_and_replies_cover_deltas_in_flight() {
    let (mut d, sink) = (Delivery::default(), Arc::new(Capture::default()));
    let t0 = Instant::now();
    let sub = subscribe(&mut d, &sink, "c", Profile::Continuous, &snapshot(5));
    d.deltas(TOPIC, (9, 10), &[text("x")], t0);
    let (lines, recover) = flush(&mut d, &sink, t0 + Duration::from_millis(30));
    assert!(lines.is_empty());
    assert_eq!(recover, vec![(sub.id.clone(), TOPIC.to_owned())]);
    // 回复在途：期间的增量都已含在回复中。
    d.deltas(TOPIC, (10, 11), &[text("y")], t0);
    assert!(
        flush(&mut d, &sink, t0 + Duration::from_secs(1))
            .1
            .is_empty()
    );
    let lines = d.recovered(&sub.id, "online", &snapshot(11)).unwrap();
    assert_eq!(range(&frames(&lines)[0]), (0, 11));
    d.deltas(TOPIC, (11, 12), &[text("z")], t0);
    let sent = frames(&flush(&mut d, &sink, t0 + Duration::from_secs(2)).0);
    assert_eq!(range(&sent[0]), (11, 12));
}

#[test]
fn saturated_connections_buffer_until_drained_or_overflow() {
    let (mut d, sink) = (Delivery::default(), Arc::new(Capture::default()));
    let t0 = Instant::now();
    let desktop = subscribe(&mut d, &sink, "desktop", Profile::Continuous, &snapshot(0));
    subscribe(&mut d, &sink, "mobile", Profile::Replayable, &snapshot(0));
    d.saturated("mobile");
    d.deltas(TOPIC, (0, 1), &[text("a")], t0);
    let later = t0 + Duration::from_secs(1);
    let sent = frames(&flush(&mut d, &sink, later).0);
    assert_eq!(sent.len(), 1, "only the desktop flushes");
    assert_eq!(sent[0]["subscriptionId"], desktop.id.as_str());
    d.drained("mobile", later);
    let sent = frames(&flush(&mut d, &sink, later).0);
    assert_eq!(range(&sent[0]), (0, 1), "drained sends the buffered deltas");
    assert_eq!(sent[0]["payload"]["kind"], "deltas");

    d.saturated("mobile");
    for i in 1..=delivery::MAX_OPS as u64 + 1 {
        let delta = json!({"op":"row.upserted","row":{"rowId":i}});
        let size = delivery::json_bytes(&delta);
        d.deltas(TOPIC, (i, i + 1), &[(delta, size)], later);
    }
    d.drained("mobile", later);
    let (lines, recover) = flush(&mut d, &sink, later);
    assert_eq!(recover.len(), 1, "overflow recovers by snapshot");
    assert!(lines.is_empty(), "the desktop window has not ended");
}

#[test]
fn resume_replays_filtered_deltas_or_sends_nothing_when_aligned() {
    let (mut d, sink) = (Delivery::default(), Arc::new(Capture::default()));
    let aligned = json!({"epoch":"e","seq":4,"mode":"resume","from":4,"deltas":[]});
    let sub = subscribe(&mut d, &sink, "m", Profile::Replayable, &aligned);
    assert_eq!((sub.mode, sub.lines.len()), ("resume", 0));
    let behind = json!({"epoch":"e","seq":6,"mode":"resume","from":4,
        "deltas":[text("a").0, input("i").0, text("b").0]});
    let sub = subscribe(&mut d, &sink, "m", Profile::Replayable, &behind);
    assert!(sub.replaced);
    let sent = frames(&sub.lines);
    assert_eq!(range(&sent[0]), (4, 6));
    assert_eq!(sent[0]["payload"]["deltas"], json!([text("ab").0]));
    // resync 已对齐时也发 (N,N] 空帧收口客户端恢复。
    d.resync_started(&sub.id);
    let aligned = json!({"epoch":"e","seq":6,"mode":"resume","from":6,"deltas":[]});
    let sent = frames(&d.recovered(&sub.id, "recovery", &aligned).unwrap());
    assert_eq!(range(&sent[0]), (6, 6));
    assert_eq!(sent[0]["deliveryKind"], "recovery");
}

#[test]
fn index_topics_flush_without_a_window_and_connections_release_topics() {
    let (mut d, sink) = (Delivery::default(), Arc::new(Capture::default()));
    let t0 = Instant::now();
    let index = target(&sink, "sessions-index/w", "c", Profile::Continuous);
    d.register(&index, &snapshot(0)).unwrap();
    let delta = json!({"op":"session.removed","sessionId":"s"});
    let size = delivery::json_bytes(&delta);
    d.deltas("sessions-index/w", (0, 1), &[(delta, size)], t0);
    assert_eq!(frames(&flush(&mut d, &sink, t0).0).len(), 1);
    subscribe(&mut d, &sink, "c", Profile::Continuous, &snapshot(0));
    let mut released = Released::default();
    d.close_connection("c", &mut released);
    released.0.sort();
    assert_eq!(
        released.0,
        vec![TOPIC.to_owned(), "sessions-index/w".to_owned()]
    );
}

#[test]
fn backlog_invalidation_recovers_after_relief() {
    let (mut d, sink) = (Delivery::default(), Arc::new(Capture::default()));
    let t0 = Instant::now();
    let sub = subscribe(&mut d, &sink, "c", Profile::Continuous, &snapshot(0));
    d.deltas(TOPIC, (0, 1), &[text("a")], t0);
    sink.paused.store(true, Ordering::Relaxed);
    d.congest();
    assert_eq!(d.next_due(), None);
    d.relieve(t0);
    assert_eq!(d.next_due(), None, "still paused");
    sink.paused.store(false, Ordering::Relaxed);
    d.relieve(t0);
    assert_eq!(flush(&mut d, &sink, t0).1, vec![(sub.id, TOPIC.to_owned())]);
}

/// Pin releases of a step; nothing else is expected.
#[derive(Default)]
struct Released(Vec<String>);

impl crate::Link<()> for Released {
    fn request(&mut self, method: crate::contract::Method, _: Value, _: crate::Call<()>) {
        panic!("unexpected request {method:?}");
    }
    fn release(&mut self, topic: String) {
        self.0.push(topic);
    }
    fn answer(&mut self, (): (), _: Result<Value, crate::contract::RuntimeError>) {
        panic!("unexpected answer");
    }
}
