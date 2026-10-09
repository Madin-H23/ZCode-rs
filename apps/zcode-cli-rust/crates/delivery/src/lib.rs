//! V4 topic delivery (spec rust-u3-stage-b §3.2, rust-m8-delivery §5–6): the
//! one implementation of subscriptions per (topic, connection), flush windows,
//! watermarks, snapshot recovery, the two delivery profiles and the wire v3
//! encoding, shared by every front-end of a runtime.
//!
//! The runtime owns topic facts (seq, retained logs, snapshots); delivery
//! decides which connection receives which frame and when. It owns no
//! transport: frames go to the [`ConnectionSink`] of their connection, runtime
//! requests and request answers go through the embedder's [`Link`]. It starts
//! no task and reads no clock: callers pass `now` and schedule
//! [`Delivery::next_due`].
use zcode_cli_core_api as contract;
use zcode_cli_domain as domain;
mod calls;
mod codec;
mod connections;
mod registry;
mod sink;
mod subscription;
pub use calls::{Call, Resync, Subscribe};
pub use codec::notification;
pub use registry::Delivery;
pub use sink::{Connection, ConnectionSink, FrameRoute, Link};
