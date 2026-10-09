//! 测试用的进程内远端 `zcode-server`：按 `channel.method` 登记应答，记录调用，按 `channel.event`
//! 向登记的 listen 推送事件。经 [`RpcClient`] 直连，不经任何传输。
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use zcode_host_kit::{RpcError, RpcValue};
use zcode_host_rpc::codec::{self, RESPONSE_EVENT_FIRE};
use zcode_host_rpc::{RequestKind, RpcClient, read_request};

use super::transport::{CloseEvent, ConnectParams, Connection, Transport};

type Handler = Arc<dyn Fn(&FakeRemote, Value) -> Result<Value, RpcError> + Send + Sync>;

#[derive(Default)]
struct State {
    handlers: HashMap<String, Handler>,
    /// (id, "channel.event", arg)
    listeners: Vec<(i32, String, Value)>,
    calls: Vec<(String, Value)>,
}

#[derive(Clone)]
pub(crate) struct FakeRemote {
    to_client: Arc<Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>>,
    state: Arc<Mutex<State>>,
    closed: watch::Sender<Option<CloseEvent>>,
}

impl FakeRemote {
    /// 返回 (远端, 连到它的客户端)。
    pub(crate) fn start() -> (Self, RpcClient) {
        let (outbound, mut requests) = mpsc::unbounded_channel::<Vec<u8>>();
        let (to_client, inbound) = mpsc::unbounded_channel();
        let client = RpcClient::start(outbound, inbound);
        let _ = to_client.send(codec::initialize());
        let remote = Self {
            to_client: Arc::new(Mutex::new(Some(to_client))),
            state: Arc::default(),
            closed: watch::channel(None).0,
        };
        let server = remote.clone();
        tokio::spawn(async move {
            while let Some(message) = requests.recv().await {
                let Ok(request) = read_request(&message) else {
                    continue;
                };
                let key = format!("{}.{}", request.channel, request.name);
                match request.kind {
                    RequestKind::Promise => {
                        let params = match request.body {
                            RpcValue::Array(items) => items
                                .into_iter()
                                .next()
                                .map(RpcValue::into_json)
                                .unwrap_or(Value::Null),
                            other => other.into_json(),
                        };
                        server.lock().calls.push((key.clone(), params.clone()));
                        let handler = server.lock().handlers.get(&key).cloned();
                        let result = match handler {
                            Some(handler) => handler(&server, params),
                            None => Ok(Value::Null),
                        };
                        let response = match result {
                            Ok(value) => codec::success_value(request.id, &value),
                            Err(error) => codec::promise_error(request.id, &error),
                        };
                        server.send(response);
                    }
                    RequestKind::EventListen => {
                        server
                            .lock()
                            .listeners
                            .push((request.id, key, request.body.into_json()));
                    }
                    RequestKind::EventDispose => {
                        server.lock().listeners.retain(|(id, ..)| *id != request.id);
                    }
                    RequestKind::PromiseCancel => {}
                }
            }
        });
        (remote, client)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    fn send(&self, message: Vec<u8>) {
        if let Some(sender) = self.to_client.lock().unwrap().as_ref() {
            let _ = sender.send(message);
        }
    }

    pub(crate) fn on(
        &self,
        key: &str,
        handler: impl Fn(&FakeRemote, Value) -> Result<Value, RpcError> + Send + Sync + 'static,
    ) {
        self.lock()
            .handlers
            .insert(key.to_owned(), Arc::new(handler));
    }

    /// 向 `channel.event` 的全部 listen 推送一个事件。
    pub(crate) fn fire(&self, key: &str, payload: Value) {
        let ids: Vec<i32> = self
            .lock()
            .listeners
            .iter()
            .filter(|(_, listened, _)| listened == key)
            .map(|(id, ..)| *id)
            .collect();
        for id in ids {
            self.send(codec::response_value(RESPONSE_EVENT_FIRE, id, &payload));
        }
    }

    pub(crate) fn calls(&self, key: &str) -> Vec<Value> {
        self.lock()
            .calls
            .iter()
            .filter(|(called, _)| called == key)
            .map(|(_, params)| params.clone())
            .collect()
    }

    /// 底层连接关闭（远端 server 退出）。
    pub(crate) fn close(&self, exit_code: i64) {
        self.to_client.lock().unwrap().take();
        self.closed.send_replace(Some(CloseEvent {
            exit_code: Some(exit_code),
            error: None,
        }));
    }

    pub(crate) fn connection(&self, client: RpcClient) -> Connection {
        Connection::for_test(client, None, self.closed.subscribe())
    }
}

/// 依次交出预先准备的连接。
pub(crate) struct FakeTransport(pub(crate) Mutex<Vec<Connection>>);

impl Transport for FakeTransport {
    fn connect(&self, _params: ConnectParams) -> BoxFuture<'static, Result<Connection, String>> {
        let next = self.0.lock().unwrap().pop();
        Box::pin(async move { next.ok_or_else(|| "no fake connection left".to_owned()) })
    }
}

pub(crate) fn unavailable() -> RpcError {
    RpcError::new("Error", "ZCode agent runtime is unavailable")
        .with_code(json!("ZCODE_AGENT_RUNTIME_UNAVAILABLE"))
}
