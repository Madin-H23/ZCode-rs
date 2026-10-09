//! Actor-owned topic facts: snapshots, sequence numbers, retained logs and
//! delivery pins.
//!
//! Subscriptions, framing, flow control and fan-out belong to the frontend
//! (App Server). The actor only guarantees that a snapshot and its seq are
//! produced atomically and that every later change is emitted with a
//! contiguous `(from, to]` range while the topic is open.
use super::Engine;
use crate::contract::{RuntimeError, RuntimeEvent, ServerMsg};
use crate::domain::topic_log::sized;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

impl Engine {
    /// Delivery opened `topic`: load the session if needed, pin it and return
    /// its resume range or snapshot.
    pub(super) async fn open_topic(&mut self, p: &Value) -> Result<Value> {
        let topic = topic_param(p)?;
        if let Some(id) = topic.strip_prefix("conversation/") {
            // 订阅带 workspace 时只接受该 workspace 的会话（同路径不同 identity 隔离）。
            let named = self.named_workspace(p)?;
            self.ensure_session_in(id, named.as_ref()).await?;
        }
        // 列表与配置话题按话题中的 workspace id 打开作用域（任意 workspace，TS CLI 相同）。
        if let Some(id) = topic
            .strip_prefix("sessions-index/")
            .or_else(|| topic.strip_prefix("workspace-config/"))
        {
            let workspace = self.workspace_of_id(id)?;
            self.open_scope(&workspace).await?;
        }
        // 先取快照再计数：快照失败不能留下 pin。
        let state = self.topic_snapshot_value(p)?;
        *self.interest.entry(topic.to_owned()).or_default() += 1;
        Ok(state)
    }

    /// One delivery subscription on `topic` ended.
    pub(super) fn release_topic(&mut self, topic: &str) {
        if let Some(count) = self.interest.get_mut(topic) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.interest.remove(topic);
            }
        }
    }

    /// `{epoch, seq, mode, deltas | snapshot}` of a topic as one consistent
    /// value: `resume` with the retained deltas after `base`, or `snapshot`
    /// (spec rust-m8-delivery §3).
    pub(super) fn topic_snapshot_value(&mut self, p: &Value) -> Result<Value> {
        let topic = topic_param(p)?;
        let base = (p["forceSnapshot"] != true)
            .then(|| {
                p["base"]["logEpoch"]
                    .as_str()
                    .zip(p["base"]["seq"].as_u64())
            })
            .flatten();
        let resumable = |epoch: &str| base.filter(|(e, _)| *e == epoch).map(|(_, seq)| seq);
        if let Some(id) = topic.strip_prefix("conversation/") {
            // 差量 patch 的基线必须等于快照中的 state：先发布尚未发布的 state 变化。
            self.publish(id, vec![])?;
            let session = self.sessions.get(id).context("Session unavailable")?;
            let replay = resumable(&session.epoch).and_then(|seq| {
                let deltas = session.topic.log.replay(&session.epoch, seq, session.seq)?;
                Some((seq, deltas))
            });
            return Ok(reply(&session.epoch, session.seq, replay, || {
                session.snapshot()
            }));
        }
        if let Some(workspace) = topic.strip_prefix("sessions-index/") {
            // 打开的话题钉住作用域；未打开过的 workspace 不能直接取快照。
            let scope = self.scope(workspace)?;
            let replay = resumable(&scope.epoch).and_then(|seq| {
                let deltas = scope.index_log.replay(&scope.epoch, seq, scope.index_seq)?;
                Some((seq, deltas))
            });
            return Ok(reply(&scope.epoch, scope.index_seq, replay, || {
                index_snapshot(scope)
            }));
        }
        if let Some(workspace) = topic.strip_prefix("workspace-config/") {
            let scope = self.scope(workspace)?;
            // 整体替换态：只有水位对齐时才续传，否则快照（两者等价）。
            let replay = resumable(&scope.epoch)
                .filter(|seq| *seq == scope.config_seq)
                .map(|seq| (seq, vec![]));
            return Ok(reply(&scope.epoch, scope.config_seq, replay, || {
                self.config_snapshot(scope)
            }));
        }
        bail!("Unsupported topic")
    }

    fn config_snapshot(&self, scope: &super::workspace_scope::Scope) -> Value {
        json!({"protocolVersion":1,"workspaceId":scope.workspace.key,"logEpoch":scope.epoch,"config":self.workspace_config()})
    }

    fn watched(&self, topic: &str) -> bool {
        self.interest.contains_key(topic)
    }

    pub(super) fn publish(&mut self, id: &str, mut deltas: Vec<Value>) -> Result<()> {
        let session = self.sessions.get_mut(id).context("Session unavailable")?;
        let text_only = deltas.iter().all(|d| d["op"] == "row.delta");
        if !text_only {
            deltas.extend(session.history_actions());
        }
        // 客户端按在场键整体替换 patch：只发变化的顶层键，流式文本块因此只剩 row.delta。
        let patch = session.patch();
        if let Some(patch) = session.topic.changed(&session.epoch, patch) {
            deltas.push(json!({"op":"state.updated","patch":patch}));
        }
        // 纯文本增量不改变列表事实；lastActivityAt 在下一个语义边界随摘要一并发布，
        // 避免每个流式分片都推送一帧 sessions-index。
        let summary = (!text_only).then(|| {
            (
                session.workspace.clone(),
                session.listed && !session.archived,
                session.summary(),
            )
        });
        if !deltas.is_empty() {
            let from = session.seq;
            session.seq += deltas.len() as u64;
            let to = session.seq;
            let ttft = self.ttft_observations(id, &deltas);
            let session = self.sessions.get_mut(id).unwrap();
            let deltas = sized(deltas);
            // 日志始终记账，与订阅无关：手机重连时即使无人订阅过也能续传。
            session
                .topic
                .log
                .record(&session.epoch, from, to, deltas.clone());
            if self.watched(&format!("conversation/{id}")) {
                self.outbox
                    .push(ServerMsg::Event(RuntimeEvent::ConversationDeltas {
                        session: id.into(),
                        from,
                        to,
                        deltas,
                        ttft,
                    }));
            }
        }
        match summary {
            Some((workspace, listed, summary)) => {
                self.publish_index(&workspace, id, listed.then_some(summary))
            }
            None => Ok(()),
        }
    }

    /// Upserts (`Some`) or removes a session in workspace `key`'s sessions-index.
    pub(super) fn publish_index(
        &mut self,
        key: &str,
        id: &str,
        summary: Option<Value>,
    ) -> Result<()> {
        let watched = self.watched(&format!("sessions-index/{key}"));
        let scope = self.scope_mut(key)?;
        // 与 Node summariesEqual 一致：摘要未变化不推进 seq，也不产生帧。
        // 删除保持幂等广播：从未入索引的草稿关闭时，客户端仍需收到移除事实。
        if summary
            .as_ref()
            .is_some_and(|summary| scope.index.get(id) == Some(summary))
        {
            return Ok(());
        }
        let delta = match summary {
            Some(summary) => {
                scope.index.insert(id.into(), summary.clone());
                json!({"op":"session.upserted","session":summary})
            }
            None => {
                scope.index.remove(id);
                json!({"op":"session.removed","sessionId":id})
            }
        };
        let from = scope.index_seq;
        scope.index_seq += 1;
        let to = scope.index_seq;
        let deltas = sized(vec![delta]);
        scope
            .index_log
            .record(&scope.epoch, from, to, deltas.clone());
        if watched {
            self.outbox
                .push(ServerMsg::Event(RuntimeEvent::IndexChanged {
                    workspace: key.into(),
                    from,
                    to,
                    deltas,
                }));
        }
        Ok(())
    }

    /// The model catalog changed: every workspace's config topic advances.
    pub(super) fn publish_config(&mut self) {
        let keys: Vec<String> = self.scopes.keys().cloned().collect();
        for key in keys {
            let scope = self.scopes.get_mut(&key).unwrap();
            scope.config_seq += 1;
            if self.watched(&format!("workspace-config/{key}")) {
                let scope = &self.scopes[&key];
                let snapshot = self.config_snapshot(scope);
                self.outbox
                    .push(ServerMsg::Event(RuntimeEvent::ConfigChanged {
                        workspace: key,
                        seq: scope.config_seq,
                        snapshot,
                    }));
            }
        }
    }

    /// History was rewritten under a new epoch: subscribers replace their state.
    pub(super) fn reset_topic(&mut self, id: &str) -> Result<()> {
        let session = self.sessions.get_mut(id).context("Session unavailable")?;
        // 所有订阅者都以这份快照为准：差量基线直接对齐当前 state，不另发 patch。
        let patch = session.patch();
        session.topic.changed(&session.epoch, patch);
        if self.watched(&format!("conversation/{id}")) {
            let session = self.sessions.get(id).context("Session unavailable")?;
            self.outbox
                .push(ServerMsg::Event(RuntimeEvent::ConversationReset {
                    session: id.into(),
                    seq: session.seq,
                    snapshot: session.snapshot(),
                }));
        }
        let session = &self.sessions[id];
        let (key, summary) = (session.workspace.clone(), session.summary());
        self.publish_index(&key, id, Some(summary))
    }

    /// The conversation topic ends (session deleted); drop its pin and subscriptions.
    pub(super) fn close_topic(&mut self, id: &str) {
        let topic = format!("conversation/{id}");
        if self.interest.remove(&topic).is_some() {
            self.outbox
                .push(ServerMsg::Event(RuntimeEvent::TopicClosed { topic }));
        }
    }

    pub(super) fn pinned_topics(&self) -> impl Iterator<Item = &str> {
        self.interest
            .keys()
            .filter_map(|topic| topic.strip_prefix("conversation/"))
    }
}

fn index_snapshot(scope: &super::workspace_scope::Scope) -> Value {
    json!({"protocolVersion":1,"workspaceId":scope.workspace.key,"logEpoch":scope.epoch,"sessions":scope.index.values().collect::<Vec<_>>()})
}

/// `resume` carries the retained deltas after `from` (the client's base).
fn reply(
    epoch: &str,
    seq: u64,
    replay: Option<(u64, Vec<Value>)>,
    snapshot: impl FnOnce() -> Value,
) -> Value {
    match replay {
        Some((from, deltas)) => {
            json!({"epoch":epoch,"seq":seq,"mode":"resume","from":from,"deltas":deltas})
        }
        None => json!({"epoch":epoch,"seq":seq,"mode":"snapshot","snapshot":snapshot()}),
    }
}

fn topic_param(p: &Value) -> Result<&str> {
    p["topic"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RuntimeError::invalid_params("topic: required"))
}
