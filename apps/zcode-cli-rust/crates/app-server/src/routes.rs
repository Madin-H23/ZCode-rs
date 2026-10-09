//! Delivery routes of the App Server: subscription and flow requests,
//! translation of runtime output into wire lines, and the stdout valve.
//! Delivery itself is `zcode-cli-delivery` (spec rust-u3-stage-b §3.2); this
//! front-end parses its request parameters, gives it the connection and
//! profile, and measures the writer backlog.
use super::server::{Pending, Server, required, response};
use crate::contract::{ClientMsg, RuntimeError, ServerMsg};
use crate::domain::delivery::Profile;
use crate::domain::protocol::RequestId;
use anyhow::Result;
use serde_json::{Value, json};
use std::time::Instant;
use zcode_cli_delivery::{Connection, Resync, Subscribe};

/// Unwritten output above which every connection drops its buffers and
/// recovers by snapshot.
const HIGH_WATERMARK: usize = 64 * 1024 * 1024;
/// Backlog below which congested connections are recovered.
const LOW_WATERMARK: usize = 16 * 1024 * 1024;

impl Server {
    pub(super) fn subscribe(
        &mut self,
        id: Option<RequestId>,
        p: &Value,
    ) -> Result<Option<Value>, RuntimeError> {
        let topic = required(p, "topic")?.to_owned();
        let connection = required(p, "connectionId")?.to_owned();
        let Some(mode) = p["clientMode"]
            .as_str()
            .filter(|m| matches!(*m, "desktop-continuous" | "web-remote-replayable"))
        else {
            return Err(RuntimeError::InvalidParams("clientMode: invalid".into()));
        };
        let request = Subscribe {
            connection: Connection {
                id: connection,
                sink: self.up.outlet.clone(),
            },
            profile: Profile::from_client_mode(mode),
            topic,
            // stdio 只有一条输出，不按 workspace 分发帧。
            workspace_key: String::new(),
            base: p["base"].clone(),
            workspace: p["workspace"].clone(),
        };
        self.delivery.subscribe(request, id, &mut self.up);
        Ok(None)
    }

    pub(super) fn resync(
        &mut self,
        id: Option<RequestId>,
        p: &Value,
    ) -> Result<Option<Value>, RuntimeError> {
        let subscription = required(p, "subscriptionId")?.to_owned();
        let topic = required(p, "topic")?.to_owned();
        let connection = required(p, "connectionId")?.to_owned();
        let request = Resync {
            connection,
            subscription,
            topic,
            base: p["base"].clone(),
            force_snapshot: p["forceSnapshot"].clone(),
        };
        self.delivery.resync(request, id, &mut self.up)?;
        Ok(None)
    }

    pub(super) fn unsubscribe(&mut self, p: &Value) -> Result<Value, RuntimeError> {
        let subscription = required(p, "subscriptionId")?;
        // 缺少或不是字符串的 connectionId 不拥有任何订阅：登记的连接 id 都非空。
        let connection = p["connectionId"].as_str().unwrap_or_default();
        self.delivery
            .unsubscribe(connection, subscription, &mut self.up)?;
        Ok(json!({}))
    }

    pub(super) fn flow(&mut self, p: &Value) -> Result<Value, RuntimeError> {
        let connection = required(p, "connectionId")?.to_owned();
        match required(p, "state")? {
            "closed" => {
                self.delivery.close_connection(&connection, &mut self.up);
                self.up
                    .to_runtime
                    .push_back(ClientMsg::ConnectionClosed { connection });
            }
            "saturated" => self.delivery.saturated(&connection),
            // 立即到期：事件循环随即刷新该连接的缓冲（需要时请求快照）。
            "drained" => self.delivery.drained(&connection, Instant::now()),
            _ => return Err(RuntimeError::InvalidParams("state: invalid".into())),
        }
        Ok(json!({}))
    }

    /// A delivery flush; above the writer backlog every connection is
    /// congested instead, never buffering without bound or blocking the actor.
    fn flush_step(&mut self) {
        if !self.up.outlet.congested() && self.sink.backlog() > HIGH_WATERMARK {
            self.up.outlet.congest();
        }
        self.delivery.flush(Instant::now(), &mut self.up);
    }

    /// A flush window ended.
    pub(super) async fn flush(&mut self) -> Result<()> {
        self.flush_step();
        self.send_step().await
    }

    /// Runtime output ended: write what subscribers still buffer.
    pub(super) async fn flush_final(&mut self) -> Result<()> {
        self.delivery.flush_final(Instant::now());
        self.send_step().await
    }

    pub(super) fn relieve(&mut self) {
        if self.sink.backlog() < LOW_WATERMARK {
            self.up.outlet.relieve();
            self.delivery.relieve(Instant::now());
        }
    }

    pub(super) async fn on_runtime(&mut self, batch: Vec<ServerMsg>) -> Result<()> {
        for message in batch {
            match message {
                ServerMsg::Reply { token, result } => match self.up.calls.remove(&token) {
                    Some(Pending::Rpc(Some(id))) => self.up.outlet.line(response(id, result)),
                    Some(Pending::Delivery(call)) => {
                        self.delivery
                            .on_reply(call, result, Instant::now(), &mut self.up);
                    }
                    Some(Pending::Rpc(None)) | None => {}
                },
                ServerMsg::Event(event) => {
                    self.delivery.on_event(event, Instant::now(), &mut self.up);
                }
                ServerMsg::HostRequest { id, method, params } => {
                    let line = json!({"id":id,"method":method,"params":params}).to_string();
                    self.up.outlet.line(line);
                }
                ServerMsg::HostNotification { method, params } => {
                    let line = json!({"method":method,"params":params}).to_string();
                    self.up.outlet.line(line);
                }
            }
        }
        // 窗口为 0 的订阅（sessions-index）在本批之后立即刷新。
        self.flush_step();
        self.send_step().await
    }
}
