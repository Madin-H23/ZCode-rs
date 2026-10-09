//! What an embedder gives delivery: an outlet per connection and its side of
//! the runtime link.
use crate::calls::Call;
use crate::contract::{Method, RuntimeError};
use serde_json::Value;
use std::sync::Arc;

/// Where pushed frames belong: their topic and the workspace key the embedder
/// gave when subscribing. Delivery hands it back unchanged, so an embedder
/// dispatches frames without parsing them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameRoute<'a> {
    pub topic: &'a str,
    pub workspace_key: &'a str,
}

/// The outlet of one delivery connection, implemented by whoever owns the
/// connection (the stdio front-end: its single output; a Host: the outbound
/// queue of a client connection).
///
/// Implementations must:
/// - accept every push without blocking and keep the pushed order: delivery
///   pushes frames in the order it produces them, and the fragments of a
///   logical frame only reassemble in that order;
/// - drop pushes once the connection is gone (its owner then calls
///   [`crate::Delivery::close_connection`]);
/// - answer [`ConnectionSink::paused`] from state they own, with hysteresis:
///   paused once the backlog is above the high watermark, until it is below
///   the low watermark.
pub trait ConnectionSink: Send + Sync {
    /// The physical frames (wire v3 JSON: one `complete`, or a logical
    /// frame's `fragment`s) of one logical frame of the subscription at `route`.
    fn push(&self, route: FrameRoute<'_>, frames: Vec<String>);

    /// Whether the connection's backlog is above its watermark. Delivery asks
    /// when the connection registers its first subscription, before each
    /// flush and in [`crate::Delivery::relieve`]: a paused connection's
    /// buffers are dropped and it is recovered by snapshot once it is no
    /// longer paused.
    fn paused(&self) -> bool;
}

/// A delivery connection as its owner identifies it.
#[derive(Clone)]
pub struct Connection {
    /// Non-empty and unique among the connections of one [`crate::Delivery`].
    pub id: String,
    pub sink: Arc<dyn ConnectionSink>,
}

/// The embedder's side of delivery besides sinks: the runtime link and the
/// answers to the client requests delivery serves. `R` is the embedder's
/// handle for answering one request (a JSON-RPC id, a reply channel, …).
///
/// Implementations must:
/// - send requests and releases to the runtime in call order, on the ordered
///   channel that carries the embedder's other runtime requests;
/// - hand each request's reply back to [`crate::Delivery::on_reply`] with its
///   [`Call`], at most once (a call whose reply can no longer arrive is dropped);
/// - expect [`Link::answer`] for a subscribe or resync before that request's
///   initial frames are pushed: an embedder writing answers and frames to one
///   stream in call order puts the answer first.
pub trait Link<R> {
    /// Sends `method` to the runtime; its reply belongs to `call`.
    fn request(&mut self, method: Method, params: Value, call: Call<R>);

    /// One subscription on `topic` ended: release the pin its `TopicOpen`
    /// took (`ClientMsg::TopicReleased`).
    fn release(&mut self, topic: String);

    /// Answers the client request `reply` (subscribe or resync).
    fn answer(&mut self, reply: R, result: Result<Value, RuntimeError>);
}
