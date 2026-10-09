//! Topic subscriptions per (topic, connection) (spec rust-m8-delivery §5–6).
//!
//! The runtime owns topic facts, sequence numbers and the retained log; this
//! registry decides which subscriber receives which frame and when. Each
//! subscriber buffers its profile's deltas until its flush window ends. A
//! delta is accepted only when it continues the subscriber's watermark; gaps,
//! overflow and backlog turn into snapshot recovery, and deltas that arrive
//! while a runtime reply is in flight are covered by that reply, so
//! correctness never depends on the relative timing of replies and events.
use crate::connections::{Outlet, congested, push_to};
use crate::domain::delivery::Profile;
use crate::sink::Connection;
use crate::subscription::{Subscription, snapshot_payload};
use anyhow::Result;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

/// What a subscription is registered for, as the embedder gave it.
pub(crate) struct Target {
    pub(crate) topic: String,
    pub(crate) connection: Connection,
    pub(crate) profile: Profile,
    pub(crate) workspace_key: String,
}

/// A registered subscription and its initial frames (not yet pushed).
pub(crate) struct Subscribed {
    pub(crate) id: String,
    pub(crate) mode: &'static str,
    pub(crate) replaced: bool,
    pub(crate) lines: Vec<String>,
}

/// Runtime requests produced by one delivery step.
#[derive(Default)]
pub(crate) struct Output {
    /// `(subscription, topic)` needing a snapshot from the runtime.
    pub(crate) recover: Vec<(String, String)>,
    /// Topics whose subscription ended because its snapshot could not be encoded.
    pub(crate) released: Vec<String>,
}

/// Delivery for the topics of one runtime: its subscriptions and connections.
pub struct Delivery {
    pub(crate) subscriptions: BTreeMap<String, Subscription>,
    /// Outlets of the connections that have a subscription.
    pub(crate) connections: BTreeMap<String, Outlet>,
    /// Connections reported `saturated` (Node `pausedConnections`).
    pub(crate) saturated: BTreeSet<String>,
    epoch: String,
    next_id: u64,
}

impl Default for Delivery {
    fn default() -> Self {
        Self {
            subscriptions: BTreeMap::new(),
            connections: BTreeMap::new(),
            saturated: BTreeSet::new(),
            epoch: uuid::Uuid::new_v4().simple().to_string()[..12].to_owned(),
            next_id: 0,
        }
    }
}

impl Delivery {
    pub(crate) fn get(&self, id: &str) -> Option<&Subscription> {
        self.subscriptions.get(id)
    }

    /// Registers the subscription answered by the runtime's `topicOpen` reply.
    /// Only once its initial frame is encoded does it replace the connection's
    /// previous subscription on the topic (Node rollback keeps the old one).
    pub(crate) fn register(&mut self, target: &Target, state: &Value) -> Result<Subscribed> {
        self.next_id += 1;
        let id = format!("sub-{}-{}", self.epoch, self.next_id);
        let seq = state["seq"].as_u64().unwrap_or(0);
        let mut sub = Subscription::new(target, seq);
        let lines = sub.apply(&id, "initial", state, false)?;
        let (topic, connection) = (&target.topic, &target.connection.id);
        let before = self.subscriptions.len();
        self.subscriptions
            .retain(|_, s| s.topic != *topic || s.connection != *connection);
        let replaced = self.subscriptions.len() != before;
        self.subscriptions.insert(id.clone(), sub);
        self.attach(&target.connection);
        let mode = if state["mode"] == "resume" {
            "resume"
        } else {
            "snapshot"
        };
        Ok(Subscribed {
            id,
            mode,
            replaced,
            lines,
        })
    }

    pub(crate) fn remove(&mut self, id: &str) -> Option<Subscription> {
        let sub = self.subscriptions.remove(id)?;
        self.detach_unused();
        Some(sub)
    }

    /// Runtime ended the topic; its pins are already gone.
    pub(crate) fn close_topic(&mut self, topic: &str) {
        self.subscriptions.retain(|_, s| s.topic != topic);
        self.detach_unused();
    }

    /// Client resync: the reply replaces whatever the subscription buffered.
    pub(crate) fn resync_started(&mut self, id: &str) {
        if let Some(sub) = self.subscriptions.get_mut(id) {
            sub.buffer.clear();
            sub.due = None;
            sub.recovering += 1;
        }
    }

    /// Applies the runtime reply of a resync (`recovery`) or snapshot recovery (`online`).
    pub(crate) fn recovered(&mut self, id: &str, kind: &str, state: &Value) -> Result<Vec<String>> {
        let Some(sub) = self.subscriptions.get_mut(id) else {
            return Ok(vec![]);
        };
        sub.recovering = sub.recovering.saturating_sub(1);
        let applied = sub.apply(id, kind, state, true);
        if applied.is_err() {
            sub.resync = true;
        }
        applied
    }

    /// A recovery reply failed; the subscription recovers by snapshot on its next flush.
    pub(crate) fn recovery_failed(&mut self, id: &str, now: Instant) {
        if let Some(sub) = self.subscriptions.get_mut(id) {
            sub.recovering = sub.recovering.saturating_sub(1);
            sub.buffer.clear();
            sub.resync = true;
            if !self.saturated.contains(&sub.connection) {
                sub.due = Some(now);
            }
        }
    }

    /// Buffers one runtime publication `(from, to]` for every subscriber of
    /// `topic` whose connection is not congested.
    pub(crate) fn deltas(
        &mut self,
        topic: &str,
        (from, to): (u64, u64),
        deltas: &[(Value, usize)],
        now: Instant,
    ) {
        for sub in self.subscriptions.values_mut() {
            if sub.topic != topic
                // 积压期间该连接的订阅都等待快照恢复，增量无需缓冲。
                || congested(&self.connections, &sub.connection)
                || sub.resync
                || sub.recovering > 0
                || from < sub.seq
            {
                continue;
            }
            if from > sub.seq {
                // 序号断档说明中间增量未送达：不能伪造连续水位，改由快照补齐。
                sub.buffer.clear();
                sub.resync = true;
            } else {
                let profile = sub.profile;
                let batch = deltas.iter().filter(|(d, _)| profile.keeps(d)).cloned();
                if !sub.buffer.append(batch) {
                    // 慢订阅者只保留恢复意图，不继续积压（Node overflow）。
                    sub.buffer.clear();
                    sub.resync = true;
                }
                sub.seq = to;
            }
            if sub.due.is_none() && !self.saturated.contains(&sub.connection) {
                sub.due = Some(now + sub.window);
            }
        }
    }

    /// Local TTFT facts for the next online frame of every continuous
    /// subscriber of `topic` (Node attaches them to `continuous` + `online` frames).
    pub(crate) fn ttft(&mut self, topic: &str, observations: &[Value]) {
        let Some((first, related)) = observations.split_first() else {
            return;
        };
        let mut fields = format!(r#","ttft":{first}"#);
        if !related.is_empty() {
            fields.push_str(&format!(
                r#","ttftRelated":{}"#,
                Value::from(related.to_vec())
            ));
        }
        for sub in self.subscriptions.values_mut() {
            if sub.topic == topic
                && sub.profile == Profile::Continuous
                && !sub.resync
                && !congested(&self.connections, &sub.connection)
            {
                sub.ttft = Some(fields.clone());
            }
        }
    }

    /// Snapshot to every subscriber of `topic` (history reset, config change),
    /// serialized once; saturated subscribers recover when drained.
    pub(crate) fn snapshot_topic(
        &mut self,
        topic: &str,
        kind: &str,
        seq: u64,
        snapshot: &Value,
    ) -> Output {
        let payload = snapshot_payload(snapshot);
        let mut out = Output::default();
        for id in self.ids(|s| s.topic == topic) {
            let sub = self
                .subscriptions
                .get_mut(&id)
                .expect("listed subscription");
            if self.saturated.contains(&sub.connection) {
                sub.buffer.clear();
                sub.resync = true;
                continue;
            }
            match sub.snapshot_frame(&id, kind, seq, &payload) {
                Ok(lines) => push_to(&self.connections, sub, lines),
                Err(_) => {
                    self.remove(&id);
                    out.released.push(topic.into());
                }
            }
        }
        out
    }

    /// Pushes the frames of every due subscription and returns the snapshots
    /// they need. Subscriptions of saturated or congested connections wait.
    pub(crate) fn flush_due(&mut self, now: Instant) -> Output {
        let mut out = Output::default();
        for (id, sub) in &mut self.subscriptions {
            if sub.due.is_none_or(|due| due > now)
                || self.saturated.contains(&sub.connection)
                || congested(&self.connections, &sub.connection)
            {
                continue;
            }
            sub.due = None;
            if sub.recovering > 0 {
                continue;
            }
            if !sub.resync && (!sub.buffer.is_empty() || sub.sent_seq != sub.seq) {
                let deltas = sub.buffer.take();
                // 被 profile 过滤掉的 seq 也在区间内，客户端连续性判定不受过滤影响。
                match sub.online_frame(id, (sub.sent_seq, sub.seq), &deltas) {
                    Ok(lines) => push_to(&self.connections, sub, lines),
                    Err(_) => sub.resync = true,
                }
            }
            if sub.resync {
                sub.recovering += 1;
                out.recover.push((id.clone(), sub.topic.clone()));
            }
        }
        out
    }

    /// Output ended: every buffered delta is pushed; snapshots can no longer
    /// be served. A congested connection has nothing buffered: its deltas
    /// were skipped and its frames since were pushed directly.
    pub(crate) fn flush_all(&mut self, now: Instant) {
        for sub in self.subscriptions.values_mut() {
            sub.due = Some(now);
        }
        self.flush_due(now);
    }

    /// When the earliest flush window of a connection that can receive ends.
    pub fn next_due(&self) -> Option<Instant> {
        self.subscriptions
            .values()
            .filter(|s| {
                !self.saturated.contains(&s.connection)
                    && !congested(&self.connections, &s.connection)
            })
            .filter_map(|s| s.due)
            .min()
    }

    /// Pushes frames of subscription `id` to its connection.
    pub(crate) fn push(&self, id: &str, lines: Vec<String>) {
        if let Some(sub) = self.subscriptions.get(id) {
            push_to(&self.connections, sub, lines);
        }
    }

    pub(crate) fn ids(&self, filter: impl Fn(&Subscription) -> bool) -> Vec<String> {
        self.subscriptions
            .iter()
            .filter(|(_, s)| filter(s))
            .map(|(id, _)| id.clone())
            .collect()
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
