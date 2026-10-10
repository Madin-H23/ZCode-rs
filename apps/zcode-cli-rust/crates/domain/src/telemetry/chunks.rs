//! `stream.chunk` facts merged per time window (spec rust-m9-usage-logs
//! §5.7). Node sends one fact per model delta; the only consumer (the
//! Renderer's telemetry supervisor) reads when the first chunk, the chunks
//! that open a step and the last chunk before a pause or the end arrived.
//! A chunk that opens something goes out at once with its usual shape; the
//! rest of a session's stream is held and sent at most every `WINDOW`, with
//! `chunkCount` and `lastChunkAgeMs` (how long ago its newest delta came) so
//! the consumer can restore that delta's arrival.
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How long a session's chunk facts after one that went out are held.
const WINDOW: Duration = Duration::from_millis(250);

/// The fields that differ between the facts of one stream.
const PER_CHUNK: [&str; 7] = [
    "eventId",
    "eventSeq",
    "occurredAt",
    "chunkLength",
    "firstChunk",
    "chunkCount",
    "lastChunkAgeMs",
];

/// The held chunk facts of every session; the Engine owns it and sends what
/// its methods return, in order.
#[derive(Debug, Default)]
pub struct ChunkWindow {
    sessions: HashMap<String, Stream>,
    /// Orders held facts by when they started.
    opened: u64,
}

#[derive(Debug, Default)]
struct Stream {
    /// The last chunk fact that went out for the session, and when.
    sent: Option<(Map<String, Value>, Instant)>,
    held: Option<Held>,
}

#[derive(Debug)]
struct Held {
    /// The newest delta's fact; its length becomes the sum on the way out.
    fact: Map<String, Value>,
    count: u64,
    length: u64,
    newest: Instant,
    due: Instant,
    opened: u64,
}

impl Held {
    fn into_fact(self, now: Instant) -> Map<String, Value> {
        let mut fact = self.fact;
        fact.insert("chunkLength".into(), self.length.into());
        fact.insert("chunkCount".into(), self.count.into());
        let age = now.saturating_duration_since(self.newest);
        fact.insert(
            "lastChunkAgeMs".into(),
            ((age.as_secs_f64() * 1000.0).round() as u64).into(),
        );
        fact
    }
}

/// The same stream: the facts agree on every field but the per-fact ones
/// (session, turn, command, channel, assistant message, part, parent call).
fn same_stream(a: &Map<String, Value>, b: &Map<String, Value>) -> bool {
    let identity = |fact: &Map<String, Value>| {
        fact.iter()
            .filter(|(key, _)| !PER_CHUNK.contains(&key.as_str()))
            .count()
    };
    identity(a) == identity(b)
        && a.iter()
            .all(|(key, value)| PER_CHUNK.contains(&key.as_str()) || b.get(key) == Some(value))
}

impl ChunkWindow {
    /// The facts to send now for one normalized fact. A fact that is not a
    /// chunk first releases everything held (notification order follows
    /// event order) and ends its session's stream.
    pub fn admit(&mut self, fact: Value, now: Instant) -> Vec<Value> {
        let Value::Object(fact) = fact else {
            return vec![fact];
        };
        let session = fact["sessionId"].as_str().unwrap_or("").to_owned();
        if fact["kind"] != "stream.chunk" {
            let mut out = self.flush(now);
            self.sessions.remove(&session);
            out.push(Value::Object(fact));
            return out;
        }
        self.sessions.retain(|_, s| {
            s.held.is_some() || s.sent.as_ref().is_some_and(|(_, at)| now < *at + WINDOW)
        });
        let length = fact["chunkLength"].as_u64().unwrap_or(0);
        let mut out = Vec::new();
        let stream = self.sessions.entry(session).or_default();
        // A held fact of another stream, or one whose window passed, goes first.
        if let Some(held) = stream
            .held
            .take_if(|h| !same_stream(&h.fact, &fact) || h.due <= now)
        {
            let sent = held.into_fact(now);
            out.push(Value::Object(sent.clone()));
            stream.sent = Some((sent, now));
        }
        if let Some(held) = &mut stream.held {
            held.fact = fact;
            held.count += 1;
            held.length += length;
            held.newest = now;
            return out;
        }
        let since = match &stream.sent {
            Some((last, at)) if same_stream(last, &fact) && now < *at + WINDOW => Some(*at),
            Some(_) | None => None,
        };
        match since.filter(|_| fact["firstChunk"] != true) {
            Some(at) => {
                self.opened += 1;
                stream.held = Some(Held {
                    fact,
                    count: 1,
                    length,
                    newest: now,
                    due: at + WINDOW,
                    opened: self.opened,
                });
            }
            None => {
                stream.sent = Some((fact.clone(), now));
                out.push(Value::Object(fact));
            }
        }
        out
    }

    /// Everything held, oldest first: before a notification that must not
    /// overtake it (a computer-use operation event, a request's output).
    pub fn flush(&mut self, now: Instant) -> Vec<Value> {
        self.release(now, |_| true)
    }

    /// The held facts whose window closed by `now`, oldest first.
    pub fn due(&mut self, now: Instant) -> Vec<Value> {
        self.release(now, |held| held.due <= now)
    }

    /// When the earliest held fact's window closes.
    pub fn deadline(&self) -> Option<Instant> {
        self.sessions
            .values()
            .filter_map(|s| s.held.as_ref().map(|h| h.due))
            .min()
    }

    fn release(&mut self, now: Instant, ready: impl Fn(&Held) -> bool) -> Vec<Value> {
        let mut released: Vec<(u64, Map<String, Value>)> = Vec::new();
        for stream in self.sessions.values_mut() {
            if let Some(held) = stream.held.take_if(|h| ready(h)) {
                let opened = held.opened;
                let sent = held.into_fact(now);
                stream.sent = Some((sent.clone(), now));
                released.push((opened, sent));
            }
        }
        released.sort_by_key(|(opened, _)| *opened);
        released
            .into_iter()
            .map(|(_, fact)| Value::Object(fact))
            .collect()
    }
}

#[cfg(test)]
#[path = "chunks_tests.rs"]
mod tests;
