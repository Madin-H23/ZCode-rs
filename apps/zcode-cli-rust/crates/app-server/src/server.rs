//! App Server event loop: JSON-RPC routing, delivery and host bridging.
//!
//! Ordering rules:
//! - The loop never awaits the runtime. Requests are queued and sent when the
//!   runtime can accept them, while runtime output is always drained, so the
//!   two bounded channels cannot deadlock.
//! - A runtime reply is written before the events that follow it in the same
//!   runtime batch, matching the Node runtime (response line, then frames).
use super::{Sink, outlet::Outlet};
use crate::contract::{ClientMsg, Input, Method, RuntimeError, ServerMsg};
use crate::domain::protocol::{Request, RequestId};
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::mpsc;
use zcode_cli_delivery::{Call, Delivery, Link};

/// Queued runtime requests above which stdin is not read (backpressure to the reader thread).
const MAX_QUEUED_REQUESTS: usize = 64;
const RUNTIME_CHANNEL: usize = 64;

pub(super) enum Pending {
    Rpc(Option<RequestId>),
    /// A runtime request of delivery (subscribe, resync, snapshot recovery).
    Delivery(Call<Option<RequestId>>),
}

/// The runtime link: requests waiting to be sent, their pending replies, and
/// the output that delivery's answers go to.
pub(super) struct Upstream {
    pub(super) to_runtime: VecDeque<ClientMsg>,
    pub(super) calls: HashMap<u64, Pending>,
    next_token: u64,
    pub(super) outlet: Arc<Outlet>,
}

impl Upstream {
    pub(super) fn forward(&mut self, method: Method, params: Value, pending: Pending) {
        self.next_token += 1;
        self.calls.insert(self.next_token, pending);
        self.to_runtime.push_back(ClientMsg::Request {
            token: self.next_token,
            method,
            params,
        });
    }
}

impl Link<Option<RequestId>> for Upstream {
    fn request(&mut self, method: Method, params: Value, call: Call<Option<RequestId>>) {
        self.forward(method, params, Pending::Delivery(call));
    }

    fn release(&mut self, topic: String) {
        self.to_runtime
            .push_back(ClientMsg::TopicReleased { topic });
    }

    /// 写进本步的输出：回复行排在随后 push 的初始帧之前（与 Node 相同）。
    fn answer(&mut self, reply: Option<RequestId>, result: Result<Value, RuntimeError>) {
        if let Some(id) = reply {
            self.outlet.line(response(id, result));
        }
    }
}

pub(super) struct Server {
    pub(super) sink: Sink,
    pub(super) delivery: Delivery,
    pub(super) up: Upstream,
    pub(super) input_open: bool,
}

/// Run the App Server over `runtime`, reading parsed wire input and writing to `sink`.
/// Returns the runtime's result after it stops and its output is drained. On
/// every path, the runtime has stopped before this returns.
pub async fn serve<F, Fut>(runtime: F, mut input: mpsc::Receiver<Input>, sink: Sink) -> Result<()>
where
    F: FnOnce(mpsc::Receiver<ClientMsg>, mpsc::Sender<Vec<ServerMsg>>) -> Fut,
    Fut: std::future::Future<Output = Result<()>> + Send + 'static,
{
    let (client_tx, client_rx) = mpsc::channel(RUNTIME_CHANNEL);
    let (server_tx, mut server_rx) = mpsc::channel(RUNTIME_CHANNEL);
    let running = zcode_async::owned(runtime(client_rx, server_tx));
    let mut server = Server {
        sink,
        delivery: Delivery::default(),
        up: Upstream {
            to_runtime: VecDeque::new(),
            calls: HashMap::new(),
            next_token: 0,
            outlet: Arc::default(),
        },
        input_open: true,
    };
    let mut drain = tokio::time::interval(std::time::Duration::from_millis(100));
    drain.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let served = async {
        loop {
            // 订阅者缓冲的刷新窗口（spec rust-m8-delivery §5）；拥塞的连接不参与，由 drain 恢复。
            let due = server.delivery.next_due();
            let flush_at =
                due.map_or_else(tokio::time::Instant::now, tokio::time::Instant::from_std);
            tokio::select! {
                biased;
                batch = server_rx.recv() => match batch {
                    Some(batch) => server.on_runtime(batch).await?,
                    None => {
                        server.flush_final().await?;
                        return Ok::<(), anyhow::Error>(());
                    }
                },
                _ = tokio::time::sleep_until(flush_at), if due.is_some() => server.flush().await?,
                permit = client_tx.reserve(), if !server.up.to_runtime.is_empty() => match permit {
                    Ok(permit) => permit.send(server.up.to_runtime.pop_front().expect("queue is not empty")),
                    // runtime 已停止接收；剩余请求没有 owner，等待其输出关闭后退出。
                    Err(_) => server.up.to_runtime.clear(),
                },
                message = input.recv(), if server.input_open && server.up.to_runtime.len() < MAX_QUEUED_REQUESTS => match message {
                    Some(message) => server.on_input(message).await?,
                    None => server.end_input(),
                },
                _ = drain.tick(), if server.up.outlet.congested() => server.relieve(),
            }
        }
    }
    .await;
    // 修复：输出写入失败（对端关闭、stdout 断管）时这里原来直接返回，丢弃 runtime 的句柄，Engine
    // 在实例结束之后继续运行——取消 run、回收工具进程树、提交中断态都发生在实例之外；二进制随即
    // 退出时，这段收口被直接打断，TERM 不响应的后代进程留了下来。现在先关闭两条通道（runtime
    // 看到输入结束、输出关闭，照常收口），等它结束后再返回。
    drop(client_tx);
    drop(server_rx);
    let stopped = running.await;
    served?;
    stopped?
}

pub(super) fn response(id: RequestId, result: Result<Value, RuntimeError>) -> String {
    match result {
        Ok(value) => json!({"id":id,"result":value}),
        Err(error) => json!({"id":id,"error":error.to_json()}),
    }
    .to_string()
}

pub(super) fn required<'a>(p: &'a Value, key: &str) -> Result<&'a str, RuntimeError> {
    p[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RuntimeError::InvalidParams(format!("{key}: required")))
}

impl Server {
    fn end_input(&mut self) {
        if self.input_open {
            self.input_open = false;
            self.up.to_runtime.push_back(ClientMsg::Eof);
        }
    }

    async fn reply(
        &mut self,
        id: Option<RequestId>,
        result: Result<Value, RuntimeError>,
    ) -> Result<()> {
        match id {
            Some(id) => {
                self.up.outlet.line(response(id, result));
                self.send_step().await
            }
            None => Ok(()),
        }
    }

    /// Writes the lines collected in the outlet during this step.
    pub(super) async fn send_step(&mut self) -> Result<()> {
        let lines = self.up.outlet.take();
        self.sink.send(lines).await
    }

    async fn on_input(&mut self, message: Input) -> Result<()> {
        let invalid = || Some(RequestId::Text("invalid-message".into()));
        match message {
            Input::Request(request) => self.on_request(request).await,
            Input::Response { id, result } => {
                self.up
                    .to_runtime
                    .push_back(ClientMsg::HostReply { id, result });
                Ok(())
            }
            Input::Invalid => {
                let error = RuntimeError::Coded {
                    code: -32700,
                    message: "Invalid protocol request".into(),
                };
                self.reply(invalid(), Err(error)).await
            }
            Input::TooLarge => {
                let error = RuntimeError::Coded {
                    code: -32600,
                    message: "Request exceeds size limit".into(),
                };
                self.end_input();
                self.reply(invalid(), Err(error)).await
            }
            Input::Eof => {
                self.end_input();
                Ok(())
            }
        }
    }

    async fn on_request(&mut self, request: Request) -> Result<()> {
        let Request {
            id, method, params, ..
        } = request;
        let routed = match method.as_str() {
            "v4/conversation/subscribe" => self.subscribe(id.clone(), &params),
            "v4/conversation/resync" => self.resync(id.clone(), &params),
            "v4/conversation/unsubscribe" => self.unsubscribe(&params).map(Some),
            "v4/connection/flow" => self.flow(&params).map(Some),
            name => match Method::parse(name) {
                Some(method) => {
                    self.up.forward(method, params, Pending::Rpc(id.clone()));
                    Ok(None)
                }
                None => Err(RuntimeError::MethodNotFound(name.into())),
            },
        };
        match routed {
            Ok(None) => Ok(()),
            Ok(Some(value)) => self.reply(id, Ok(value)).await,
            Err(error) => self.reply(id, Err(error)).await,
        }
    }
}
