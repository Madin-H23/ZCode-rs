//! Delivery connections: their outlets, flow control (`v4/connection/flow`)
//! and backlog (spec rust-m8-delivery §6, rust-u3-stage-b §3.2).
//!
//! A connection pauses in two independent ways:
//! - `saturated` until `drained`: flush timers stop and buffers keep
//!   accumulating until they overflow (Node `pausedConnections`);
//! - congested while its sink reports a backlog: its buffers are dropped,
//!   later deltas are skipped, and its subscriptions recover by snapshot once
//!   the sink no longer reports one, so a slow connection never makes delivery
//!   buffer without bound.
use crate::registry::Delivery;
use crate::sink::{Connection, ConnectionSink, FrameRoute, Link};
use crate::subscription::Subscription;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

/// The outlet of a connection with registered subscriptions.
pub(crate) struct Outlet {
    sink: Arc<dyn ConnectionSink>,
    /// The sink reported a backlog and has not been relieved since.
    congested: bool,
}

pub(crate) fn congested(connections: &BTreeMap<String, Outlet>, connection: &str) -> bool {
    connections.get(connection).is_some_and(|o| o.congested)
}

/// Pushes frames of `sub` to its connection's sink.
pub(crate) fn push_to(
    connections: &BTreeMap<String, Outlet>,
    sub: &Subscription,
    lines: Vec<String>,
) {
    if lines.is_empty() {
        return;
    }
    let outlet = connections
        .get(&sub.connection)
        .expect("a registered subscription's connection has an outlet");
    let route = FrameRoute {
        topic: &sub.topic,
        workspace_key: &sub.workspace_key,
    };
    outlet.sink.push(route, lines);
}

impl Delivery {
    /// A connection's first subscription keeps its sink; a connection that
    /// is already paused then starts congested (with nothing to drop yet).
    pub(crate) fn attach(&mut self, connection: &Connection) {
        self.connections
            .entry(connection.id.clone())
            .or_insert_with(|| Outlet {
                congested: connection.sink.paused(),
                sink: connection.sink.clone(),
            });
    }

    /// Connections without subscriptions keep no outlet: a later
    /// registration starts from the sink's state again.
    pub(crate) fn detach_unused(&mut self) {
        let subscriptions = &self.subscriptions;
        self.connections
            .retain(|id, _| subscriptions.values().any(|s| s.connection == *id));
    }

    /// `v4/connection/flow` `saturated`: timers stop, buffers keep
    /// accumulating until they overflow.
    pub fn saturated(&mut self, connection: &str) {
        self.saturated.insert(connection.into());
        for sub in self.subscriptions.values_mut() {
            if sub.connection == connection {
                sub.due = None;
            }
        }
    }

    /// `v4/connection/flow` `drained`: every subscription of the connection
    /// flushes now.
    pub fn drained(&mut self, connection: &str, now: Instant) {
        if !self.saturated.remove(connection) {
            return;
        }
        for sub in self.subscriptions.values_mut() {
            if sub.connection == connection {
                sub.due = Some(now);
            }
        }
    }

    /// The connection closed: each of its subscriptions ends and releases its
    /// pin, so a topic no other connection subscribes is released, and its
    /// subscribe requests still in flight register nothing when their reply
    /// arrives. The embedder then tells the runtime the connection closed.
    pub fn close_connection<R>(&mut self, connection: &str, link: &mut impl Link<R>) {
        self.saturated.remove(connection);
        self.connections.remove(connection);
        self.opening.retain(|_, c| c != connection);
        for id in self.ids(|s| s.connection == connection) {
            if let Some(sub) = self.subscriptions.remove(&id) {
                link.release(sub.topic);
            }
        }
    }

    /// Connections whose sink reports a backlog become congested: their
    /// buffers are dropped and recovered by snapshot later.
    pub(crate) fn congest(&mut self) {
        for (id, outlet) in &mut self.connections {
            if outlet.congested || !outlet.sink.paused() {
                continue;
            }
            outlet.congested = true;
            for sub in self.subscriptions.values_mut() {
                if sub.connection == *id {
                    sub.buffer.clear();
                    sub.resync = true;
                    sub.due = None;
                }
            }
        }
    }

    /// Congested connections whose sink no longer reports a backlog recover:
    /// their subscriptions waiting for a snapshot become due.
    pub fn relieve(&mut self, now: Instant) {
        for (id, outlet) in &mut self.connections {
            if !outlet.congested || outlet.sink.paused() {
                continue;
            }
            outlet.congested = false;
            if self.saturated.contains(id) {
                continue;
            }
            for sub in self.subscriptions.values_mut() {
                if sub.connection == *id && sub.resync {
                    sub.due = Some(now);
                }
            }
        }
    }
}
