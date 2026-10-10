//! The requests delivery serves and the runtime requests it issues for them.
//!
//! A subscription is registered only when the runtime's `TopicOpen` reply
//! arrives, and only if its connection has not closed since; its answer (the
//! ACK) goes to the embedder before its initial frames are pushed. Resync and
//! snapshot recovery replies cover every delta published while they were in
//! flight.
use crate::contract::{Method, RuntimeError, RuntimeEvent};
use crate::domain::delivery::Profile;
use crate::registry::{Delivery, Output, Target};
use crate::sink::{Connection, Link};
use serde_json::{Value, json};
use std::time::Instant;

/// `v4/conversation/subscribe`, with the connection and profile its owner gives.
pub struct Subscribe {
    pub connection: Connection,
    pub profile: Profile,
    pub topic: String,
    /// Handed back with every frame of the subscription ([`crate::FrameRoute`]).
    pub workspace_key: String,
    /// The client's resume point, passed to the runtime as given.
    pub base: Value,
    /// The workspace the runtime loads a conversation in, passed as given.
    pub workspace: Value,
}

/// `v4/conversation/resync` of one subscription.
pub struct Resync {
    pub connection: String,
    pub subscription: String,
    pub topic: String,
    /// The client's recovery point, passed to the runtime as given.
    pub base: Value,
    pub force_snapshot: Value,
}

/// A runtime request delivery issued, with what handling its reply needs.
/// The embedder keeps it until the reply arrives ([`Link::request`]).
pub struct Call<R>(Pending<R>);

enum Pending<R> {
    Subscribe { reply: R, target: Target, open: u64 },
    Resync { reply: R, subscription: String },
    Recover { subscription: String },
}

fn not_owned() -> RuntimeError {
    RuntimeError::Fault {
        message: "fault.subscription.notOwned".into(),
        code: None,
    }
}

/// The answer to a subscribe request whose connection closed while it was in
/// flight (the Host's, and Node `assertOpen`'s, for a closed connection).
fn connection_closed() -> RuntimeError {
    RuntimeError::Fault {
        message: "fault.connection.closed".into(),
        code: None,
    }
}

fn fault(error: &anyhow::Error) -> RuntimeError {
    RuntimeError::Fault {
        message: error.to_string(),
        code: None,
    }
}

impl Delivery {
    /// Opens the topic on the runtime; the subscription is registered and
    /// answered when the reply arrives.
    pub fn subscribe<R>(&mut self, request: Subscribe, reply: R, link: &mut impl Link<R>) {
        let Subscribe {
            connection,
            profile,
            topic,
            workspace_key,
            base,
            workspace,
        } = request;
        // 会话话题带上 attachment 的 workspace：冷加载只接受该 workspace 的会话。
        let params = json!({"topic":topic,"base":base,"workspace":workspace});
        let open = self.open_started(&connection.id);
        let target = Target {
            topic,
            connection,
            profile,
            workspace_key,
        };
        link.request(
            Method::TopicOpen,
            params,
            Call(Pending::Subscribe {
                reply,
                target,
                open,
            }),
        );
    }

    /// Recovers a subscription of `request.connection` from the client's
    /// base. On error nothing was sent and `reply` is dropped; the caller
    /// answers with the error.
    pub fn resync<R>(
        &mut self,
        request: Resync,
        reply: R,
        link: &mut impl Link<R>,
    ) -> Result<(), RuntimeError> {
        let Resync {
            connection,
            subscription,
            topic,
            base,
            force_snapshot,
        } = request;
        let owned = self
            .get(&subscription)
            .is_some_and(|s| s.topic == topic && s.connection == connection);
        if !owned {
            return Err(not_owned());
        }
        // 客户端 base 是唯一恢复起点；在途回复覆盖此后到达的全部增量。
        self.resync_started(&subscription);
        link.request(
            Method::TopicSnapshot,
            json!({"topic":topic,"base":base,"forceSnapshot":force_snapshot}),
            Call(Pending::Resync {
                reply,
                subscription,
            }),
        );
        Ok(())
    }

    /// Ends a subscription of `connection`; an unknown one has already ended.
    pub fn unsubscribe<R>(
        &mut self,
        connection: &str,
        subscription: &str,
        link: &mut impl Link<R>,
    ) -> Result<(), RuntimeError> {
        if let Some(sub) = self.get(subscription) {
            if sub.connection != connection {
                return Err(not_owned());
            }
            if let Some(sub) = self.remove(subscription) {
                link.release(sub.topic);
            }
        }
        Ok(())
    }

    /// Handles the runtime's reply to a request delivery issued.
    pub fn on_reply<R>(
        &mut self,
        call: Call<R>,
        result: Result<Value, RuntimeError>,
        now: Instant,
        link: &mut impl Link<R>,
    ) {
        match call.0 {
            Pending::Subscribe {
                reply,
                target,
                open,
            } => {
                let still_open = self.open_finished(open);
                let state = match result {
                    Ok(state) => state,
                    Err(error) => return link.answer(reply, Err(error)),
                };
                if !still_open {
                    // 修复：连接在 topicOpen 在途时关闭，回复晚于 close_connection 到达。以前照常登记，
                    // 已关闭的连接持有 pin、继续收到帧，要靠 Host 在 ACK 后补发退订才清掉。现在不登记、
                    // 不推初始帧，释放这次 open 取得的 pin，按已关闭的连接回答。
                    link.release(target.topic);
                    return link.answer(reply, Err(connection_closed()));
                }
                match self.register(&target, &state) {
                    Ok(subscribed) => {
                        if subscribed.replaced {
                            // 同一连接重复订阅：旧订阅被替换，释放它持有的 pin。
                            link.release(target.topic);
                        }
                        let ack = json!({"ack":{"subscriptionId":subscribed.id,"mode":subscribed.mode,"logEpoch":state["epoch"]}});
                        link.answer(reply, Ok(ack));
                        self.push(&subscribed.id, subscribed.lines);
                    }
                    Err(error) => {
                        // 首帧无法编码：不登记新订阅，释放 topicOpen 取得的 pin。
                        link.release(target.topic);
                        link.answer(reply, Err(fault(&error)));
                    }
                }
            }
            Pending::Resync {
                reply,
                subscription,
            } => match result {
                Ok(state) if self.get(&subscription).is_some() => {
                    match self.recovered(&subscription, "recovery", &state) {
                        Ok(frames) => {
                            let ack = json!({"ack":{"subscriptionId":subscription,"mode":state["mode"],"logEpoch":state["epoch"]}});
                            link.answer(reply, Ok(ack));
                            self.push(&subscription, frames);
                        }
                        Err(error) => link.answer(reply, Err(fault(&error))),
                    }
                }
                Ok(_) => link.answer(reply, Err(not_owned())),
                Err(error) => {
                    self.recovery_failed(&subscription, now);
                    link.answer(reply, Err(error));
                }
            },
            Pending::Recover { subscription } => {
                let frames = result.map_err(|e| anyhow::anyhow!(e.to_json().to_string()));
                match frames.and_then(|s| self.recovered(&subscription, "online", &s)) {
                    Ok(frames) => self.push(&subscription, frames),
                    // 无法取得或编码快照的订阅不能再保证连续性；结束它而不是反复重试。
                    Err(_) => {
                        if let Some(sub) = self.remove(&subscription) {
                            link.release(sub.topic);
                        }
                    }
                }
            }
        }
    }

    /// Applies one runtime event: deltas wait in their subscribers' buffers,
    /// history resets and configuration changes go out as snapshots.
    pub fn on_event<R>(&mut self, event: RuntimeEvent, now: Instant, link: &mut impl Link<R>) {
        match event {
            RuntimeEvent::ConversationDeltas {
                session,
                from,
                to,
                deltas,
                ttft,
            } => {
                let topic = format!("conversation/{session}");
                self.deltas(&topic, (from, to), &deltas, now);
                self.ttft(&topic, &ttft);
            }
            RuntimeEvent::IndexChanged {
                workspace,
                from,
                to,
                deltas,
            } => self.deltas(
                &format!("sessions-index/{workspace}"),
                (from, to),
                &deltas,
                now,
            ),
            RuntimeEvent::ConversationReset {
                session,
                seq,
                snapshot,
            } => {
                let topic = format!("conversation/{session}");
                let out = self.snapshot_topic(&topic, "recovery", seq, &snapshot);
                self.route(out, link);
            }
            RuntimeEvent::TopicClosed { topic } => self.close_topic(&topic),
            RuntimeEvent::ConfigChanged {
                workspace,
                seq,
                snapshot,
            } => {
                let topic = format!("workspace-config/{workspace}");
                let out = self.snapshot_topic(&topic, "online", seq, &snapshot);
                self.route(out, link);
            }
        }
    }

    /// A flush step: connections whose sink reports a backlog become
    /// congested; due subscriptions of the others push their buffered deltas
    /// or request a snapshot. Call it when [`Delivery::next_due`] is reached
    /// and after each batch of runtime output (windows of 0 flush then).
    pub fn flush<R>(&mut self, now: Instant, link: &mut impl Link<R>) {
        self.congest();
        let out = self.flush_due(now);
        self.route(out, link);
    }

    /// Runtime output ended: pushes what subscribers still buffer.
    pub fn flush_final(&mut self, now: Instant) {
        self.flush_all(now);
    }

    /// Issues the runtime requests and pin releases of a delivery step.
    fn route<R>(&mut self, out: Output, link: &mut impl Link<R>) {
        for (subscription, topic) in out.recover {
            link.request(
                Method::TopicSnapshot,
                json!({"topic":topic}),
                Call(Pending::Recover { subscription }),
            );
        }
        for topic in out.released {
            link.release(topic);
        }
    }
}
