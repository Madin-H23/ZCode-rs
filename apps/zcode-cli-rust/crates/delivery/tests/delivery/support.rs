//! A test embedder: one sink per connection and a link whose runtime is the
//! test itself. Answers and pushes go to one log, so the order an embedder
//! observes them in is asserted directly.
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use zcode_cli_core_api::{Method, RuntimeError, RuntimeEvent};
use zcode_cli_delivery::{Call, Connection, ConnectionSink, Delivery, FrameRoute, Link, Subscribe};
use zcode_cli_domain::delivery::{Profile, json_bytes};

/// One logical frame as a client sees it (`sentAt` left out).
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub delivery: String,
    pub subscription: String,
    pub range: (u64, u64),
    pub payload: Value,
    pub ttft: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Seen {
    Answer(u32, Result<Value, RuntimeError>),
    Frames {
        connection: String,
        topic: String,
        workspace_key: String,
        frames: Vec<Frame>,
    },
}

type Log = Arc<Mutex<Vec<Seen>>>;

pub struct Sink {
    connection: String,
    log: Log,
    paused: AtomicBool,
}

impl ConnectionSink for Sink {
    fn push(&self, route: FrameRoute<'_>, frames: Vec<String>) {
        let frames = frames.iter().map(|wire| frame(wire)).collect();
        self.log.lock().unwrap().push(Seen::Frames {
            connection: self.connection.clone(),
            topic: route.topic.into(),
            workspace_key: route.workspace_key.into(),
            frames,
        });
    }
    fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
}

impl Sink {
    /// The connection's backlog crosses the high watermark.
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
    }

    /// The backlog falls below the low watermark.
    pub fn unpause(&self) {
        self.paused.store(false, Ordering::Relaxed);
    }
}

fn frame(wire: &str) -> Frame {
    let wire: Value = serde_json::from_str(wire).unwrap();
    assert_eq!(
        (&wire["wireVersion"], &wire["kind"]),
        (&json!(3), &json!("complete"))
    );
    let frame = &wire["frame"];
    Frame {
        delivery: wire["deliveryKind"].as_str().unwrap().into(),
        subscription: frame["subscriptionId"].as_str().unwrap().into(),
        range: (
            frame["fromSeq"].as_u64().unwrap(),
            frame["toSeq"].as_u64().unwrap(),
        ),
        payload: frame["payload"].clone(),
        ttft: frame.get("ttft").cloned(),
    }
}

/// The runtime side of the link: requests wait for the test to answer them,
/// and pins follow `TopicOpen` replies and releases like the Engine's.
#[derive(Default)]
pub struct Runtime {
    log: Log,
    pub requests: VecDeque<(Method, Value, Call<u32>)>,
    pub pins: BTreeMap<String, i64>,
    pub released: Vec<String>,
}

impl Link<u32> for Runtime {
    fn request(&mut self, method: Method, params: Value, call: Call<u32>) {
        self.requests.push_back((method, params, call));
    }
    fn release(&mut self, topic: String) {
        *self.pins.entry(topic.clone()).or_default() -= 1;
        self.released.push(topic);
    }
    fn answer(&mut self, reply: u32, result: Result<Value, RuntimeError>) {
        self.log.lock().unwrap().push(Seen::Answer(reply, result));
    }
}

pub struct Harness {
    pub delivery: Delivery,
    pub runtime: Runtime,
    pub t0: Instant,
    log: Log,
    next_request: u32,
}

impl Default for Harness {
    fn default() -> Self {
        let log = Log::default();
        Self {
            delivery: Delivery::default(),
            runtime: Runtime {
                log: log.clone(),
                ..Runtime::default()
            },
            t0: Instant::now(),
            log,
            next_request: 0,
        }
    }
}

impl Harness {
    pub fn connection(&self, id: &str) -> (Connection, Arc<Sink>) {
        let sink = Arc::new(Sink {
            connection: id.into(),
            log: self.log.clone(),
            paused: AtomicBool::new(false),
        });
        let connection = Connection {
            id: id.into(),
            sink: sink.clone(),
        };
        (connection, sink)
    }

    /// What the embedder saw since the last call.
    pub fn seen(&self) -> Vec<Seen> {
        std::mem::take(&mut self.log.lock().unwrap())
    }

    /// Subscribes and leaves the `TopicOpen` request waiting; returns the request id.
    pub fn subscribe(
        &mut self,
        connection: &Connection,
        profile: Profile,
        topic: &str,
        base: Value,
    ) -> u32 {
        self.next_request += 1;
        let request = Subscribe {
            connection: connection.clone(),
            profile,
            topic: topic.into(),
            workspace_key: "/w".into(),
            base,
            workspace: json!({"workspacePath":"/w"}),
        };
        self.delivery
            .subscribe(request, self.next_request, &mut self.runtime);
        self.next_request
    }

    /// Answers the oldest runtime request; a successful `TopicOpen` takes a pin.
    pub fn reply(&mut self, result: Result<Value, RuntimeError>) -> (Method, Value) {
        let (method, params, call) = self.runtime.requests.pop_front().expect("a request");
        if method == Method::TopicOpen && result.is_ok() {
            let topic = params["topic"].as_str().unwrap().to_owned();
            *self.runtime.pins.entry(topic).or_default() += 1;
        }
        self.delivery
            .on_reply(call, result, self.t0, &mut self.runtime);
        (method, params)
    }

    /// Subscribes, answers `TopicOpen` with `state` and returns the subscription id.
    pub fn subscribed(
        &mut self,
        connection: &Connection,
        profile: Profile,
        topic: &str,
        state: Value,
    ) -> String {
        self.subscribe(connection, profile, topic, Value::Null);
        self.reply(Ok(state));
        let seen = self.seen();
        let Some(Seen::Answer(_, Ok(ack))) = seen.first() else {
            panic!("expected an ACK first: {seen:?}");
        };
        ack["ack"]["subscriptionId"].as_str().unwrap().into()
    }

    pub fn event(&mut self, event: RuntimeEvent, after_ms: u64) {
        let now = self.t0 + std::time::Duration::from_millis(after_ms);
        self.delivery.on_event(event, now, &mut self.runtime);
    }

    pub fn flush(&mut self, after_ms: u64) -> Vec<Seen> {
        let now = self.t0 + std::time::Duration::from_millis(after_ms);
        self.delivery.flush(now, &mut self.runtime);
        self.seen()
    }

    pub fn due_ms(&self) -> Option<u64> {
        let due = self.delivery.next_due()?;
        Some(due.duration_since(self.t0).as_millis() as u64)
    }
}

pub fn snapshot(seq: u64) -> Value {
    json!({"epoch":"e","seq":seq,"mode":"snapshot","snapshot":{"seq":seq}})
}

pub fn text(append: &str) -> Value {
    json!({"op":"row.delta","rowId":1,"path":"text","append":append})
}

pub fn input(append: &str) -> Value {
    json!({"op":"row.delta","rowId":2,"path":"inputText","append":append})
}

pub fn conversation(from: u64, to: u64, deltas: &[Value], ttft: Vec<Value>) -> RuntimeEvent {
    RuntimeEvent::ConversationDeltas {
        session: "s".into(),
        from,
        to,
        deltas: deltas.iter().map(|d| (d.clone(), json_bytes(d))).collect(),
        ttft,
    }
}

pub fn deltas(subscription: &str, delivery: &str, range: (u64, u64), deltas: Value) -> Frame {
    Frame {
        delivery: delivery.into(),
        subscription: subscription.into(),
        range,
        payload: json!({"kind":"deltas","deltas":deltas}),
        ttft: None,
    }
}

pub fn frames(connection: &str, topic: &str, frames: Vec<Frame>) -> Seen {
    Seen::Frames {
        connection: connection.into(),
        topic: topic.into(),
        workspace_key: "/w".into(),
        frames,
    }
}
