//! 飞书长连接客户端（@larksuiteoapi/node-sdk `WSClient` 的协议移植 + feishuProvider.ts
//! `startFeishuBotWebSocket` 的启动语义）。
//!
//! 1. `POST <open>/callback/ws/endpoint {AppID, AppSecret}` 取 wss 地址与 ClientConfig；
//!    URL 查询参数里的 `service_id` 用于 ping 帧。
//! 2. 连上后按 `PingInterval` 发 control/ping；pong 携带新的 ClientConfig。
//! 3. data 帧（`type=event`）按 `message_id/sum/seq` 拼包后交给事件处理，处理完回同一帧作为
//!    ack（追加 `biz_rt` 头，payload 为 `{"code":200[,"data":base64(JSON)]}`）。
//! 4. 断线后按 `ReconnectNonce` 随机延迟、`ReconnectInterval` 间隔重连，`ReconnectCount` 次后终止。
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use base64::Engine;
use futures_util::future::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Map, Value, json};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use zcode_host_kit::log;

use super::api::{EndpointError, FeishuApi};
use super::frame::{Frame, METHOD_CONTROL, METHOD_DATA};

/// `FEISHU_WEBSOCKET_START_TIMEOUT_MS`。
pub const START_TIMEOUT: Duration = Duration::from_secs(20);

/// 解析后的事件（EventDispatcher 拍平后的对象，含 `event_type`）→ 同步回包（卡片回调用）。
pub type EventHandler =
    Arc<dyn Fn(String, Value) -> BoxFuture<'static, Result<Option<Value>>> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Connected,
    Reconnecting,
}

#[derive(Debug, Clone)]
struct ClientConfig {
    ping_interval: Duration,
    reconnect_count: i64,
    reconnect_interval: Duration,
    reconnect_nonce: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            ping_interval: Duration::from_secs(120),
            reconnect_count: -1,
            reconnect_interval: Duration::from_secs(120),
            reconnect_nonce: Duration::from_secs(30),
        }
    }
}

impl ClientConfig {
    fn update(&mut self, value: &Value) {
        let seconds = |key: &str| value[key].as_f64().filter(|seconds| *seconds >= 0.0);
        if let Some(seconds) = seconds("PingInterval").filter(|seconds| *seconds > 0.0) {
            self.ping_interval = Duration::from_secs_f64(seconds);
        }
        if let Some(count) = value["ReconnectCount"].as_i64() {
            self.reconnect_count = count;
        }
        if let Some(seconds) = seconds("ReconnectInterval") {
            self.reconnect_interval = Duration::from_secs_f64(seconds);
        }
        if let Some(seconds) = seconds("ReconnectNonce") {
            self.reconnect_nonce = Duration::from_secs_f64(seconds);
        }
    }
}

/// EventDispatcher `RequestHandle.parse`（未加密）：v2 schema 把 header 与 event 拍平到顶层。
pub fn flatten_event(data: &Value) -> (String, Value) {
    let Some(object) = data.as_object() else {
        return (String::new(), json!({}));
    };
    let mut out = Map::new();
    if object.contains_key("schema") {
        let event_type = data["header"]["event_type"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        for (key, value) in object {
            if key != "header" && key != "event" {
                out.insert(key.clone(), value.clone());
            }
        }
        for part in ["header", "event"] {
            if let Some(map) = data[part].as_object() {
                for (key, value) in map {
                    out.insert(key.clone(), value.clone());
                }
            }
        }
        return (event_type, Value::Object(out));
    }
    let event_type = data["event"]["type"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if let Some(map) = data["event"].as_object() {
        for (key, value) in map {
            out.insert(key.clone(), value.clone());
        }
    }
    for (key, value) in object {
        if key != "event" {
            out.insert(key.clone(), value.clone());
        }
    }
    (event_type, Value::Object(out))
}

fn query_value(url: &str, key: &str) -> Option<String> {
    url::Url::parse(url).ok().and_then(|url| {
        url.query_pairs()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.into_owned())
    })
}

/// 多帧事件的拼包缓存（`DataCache.mergeData`）。
#[derive(Default)]
struct Merger {
    parts: HashMap<String, Vec<Option<Vec<u8>>>>,
}

impl Merger {
    fn merge(&mut self, message_id: &str, sum: usize, seq: usize, data: Vec<u8>) -> Option<Value> {
        let sum = sum.max(1);
        let buffer = self
            .parts
            .entry(message_id.to_owned())
            .or_insert_with(|| vec![None; sum]);
        if seq < buffer.len() {
            buffer[seq] = Some(data);
        }
        if buffer.iter().all(Option::is_some) {
            let merged: Vec<u8> = self
                .parts
                .remove(message_id)?
                .into_iter()
                .flatten()
                .flatten()
                .collect();
            return serde_json::from_slice(&merged).ok();
        }
        None
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub struct Connection {
    pub provider: String,
    pub app_id: String,
    pub app_secret: String,
}

enum Ended {
    Stopped,
    Disconnected,
}

impl Connection {
    async fn connect(&self, api: &FeishuApi) -> Result<(Socket, i32, Value), EndpointError> {
        let (url, client_config) = api
            .ws_endpoint(&self.provider, &self.app_id, &self.app_secret)
            .await?;
        let service_id = query_value(&url, "service_id")
            .and_then(|value| value.parse::<i32>().ok())
            .unwrap_or_default();
        let (socket, _) = tokio_tungstenite::connect_async(url.as_str())
            .await
            .map_err(|error| EndpointError::Retryable(error.to_string()))?;
        Ok((socket, service_id, client_config))
    }

    /// 启动并保持长连接，直到 `stop` 变为 true（返回 Ok）或重连耗尽 / 鉴权失败（返回 Err）。
    /// 首次连接受 20 秒超时约束，超时或失败直接返回 Err（由调用方 5 秒后重试，同 Node）。
    pub async fn run(
        &self,
        api: &FeishuApi,
        handler: EventHandler,
        mut stop: watch::Receiver<bool>,
        on_state: impl Fn(ConnectionState) + Send + Sync,
    ) -> Result<()> {
        let first = tokio::select! {
            result = tokio::time::timeout(START_TIMEOUT, self.connect(api)) => result,
            _ = stop.wait_for(|stopped| *stopped) => return Ok(()),
        };
        let (mut socket, mut service_id, initial) = match first {
            Err(_) => bail!(
                "Feishu WebSocket startup timed out after {}ms.",
                START_TIMEOUT.as_millis()
            ),
            Ok(Err(error)) => bail!("{error}"),
            Ok(Ok(connected)) => connected,
        };
        let mut config = ClientConfig::default();
        config.update(&initial);
        on_state(ConnectionState::Connected);
        loop {
            match self
                .serve(&mut socket, service_id, &mut config, &handler, &mut stop)
                .await
            {
                Ended::Stopped => {
                    let _ = socket.close(None).await;
                    return Ok(());
                }
                Ended::Disconnected => {}
            }
            on_state(ConnectionState::Reconnecting);
            let nonce = config.reconnect_nonce.mul_f64(random_unit());
            if wait_or_stop(nonce, &mut stop).await {
                return Ok(());
            }
            let mut attempts = 0i64;
            loop {
                attempts += 1;
                match self.connect(api).await {
                    Ok((next, next_service, next_config)) => {
                        socket = next;
                        service_id = next_service;
                        config.update(&next_config);
                        on_state(ConnectionState::Connected);
                        break;
                    }
                    Err(EndpointError::Fatal(message)) => bail!("{message}"),
                    Err(EndpointError::Retryable(message)) => {
                        log::info(format!(
                            "feishu websocket reconnect attempt failed attempts={attempts}: {message}"
                        ));
                        if config.reconnect_count >= 0 && attempts >= config.reconnect_count {
                            return Err(anyhow!("Feishu WebSocket reconnect exhausted."));
                        }
                        if wait_or_stop(config.reconnect_interval, &mut stop).await {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    async fn serve(
        &self,
        socket: &mut Socket,
        service_id: i32,
        config: &mut ClientConfig,
        handler: &EventHandler,
        stop: &mut watch::Receiver<bool>,
    ) -> Ended {
        let mut merger = Merger::default();
        let mut next_ping = tokio::time::Instant::now() + config.ping_interval;
        loop {
            // select 的各分支只产出值，不在分支里 await（watch::Ref 不能跨 await 持有）。
            enum Next {
                Stop,
                Ping,
                Message(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>),
            }
            let next = tokio::select! {
                _ = stop.wait_for(|stopped| *stopped) => Next::Stop,
                _ = tokio::time::sleep_until(next_ping) => Next::Ping,
                message = socket.next() => Next::Message(message),
            };
            let message = match next {
                Next::Stop => return Ended::Stopped,
                Next::Ping => {
                    let ping = Frame {
                        service: service_id,
                        method: METHOD_CONTROL,
                        headers: vec![("type".into(), "ping".into())],
                        ..Frame::default()
                    };
                    if socket
                        .send(Message::Binary(ping.encode().into()))
                        .await
                        .is_err()
                    {
                        return Ended::Disconnected;
                    }
                    next_ping = tokio::time::Instant::now() + config.ping_interval;
                    continue;
                }
                Next::Message(message) => message,
            };
            let bytes = match message {
                Some(Ok(Message::Binary(bytes))) => bytes,
                Some(Ok(Message::Ping(payload))) => {
                    let _ = socket.send(Message::Pong(payload)).await;
                    continue;
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return Ended::Disconnected,
                Some(Ok(_)) => continue,
            };
            let Ok(frame) = Frame::decode(&bytes) else {
                log::warn("feishu websocket frame decode failed");
                continue;
            };
            if frame.method == METHOD_CONTROL {
                if frame.header("type") == Some("pong")
                    && let Some(payload) = &frame.payload
                    && let Ok(value) = serde_json::from_slice::<Value>(payload)
                {
                    config.update(&value);
                }
                continue;
            }
            if frame.method != METHOD_DATA || frame.header("type") != Some("event") {
                continue;
            }
            let message_id = frame.header("message_id").unwrap_or_default().to_owned();
            let sum = frame
                .header("sum")
                .and_then(|value| value.parse().ok())
                .unwrap_or(1);
            let seq = frame
                .header("seq")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            let Some(data) = merger.merge(
                &message_id,
                sum,
                seq,
                frame.payload.clone().unwrap_or_default(),
            ) else {
                continue;
            };
            let started = std::time::Instant::now();
            let (event_type, event) = flatten_event(&data);
            let mut response = json!({"code": 200});
            match handler(event_type, event).await {
                Ok(Some(result)) => {
                    response["data"] =
                        json!(base64::engine::general_purpose::STANDARD.encode(result.to_string()));
                }
                Ok(None) => {}
                Err(error) => {
                    response["code"] = json!(500);
                    log::error(format!(
                        "feishu websocket invoke event failed message_id={message_id}: {error:#}"
                    ));
                }
            }
            // SDK 写的是 startTime - endTime（非正数），按原样回传。
            let elapsed = started.elapsed().as_millis() as i64;
            let mut ack = frame.clone();
            ack.headers.push(("biz_rt".into(), (-elapsed).to_string()));
            ack.payload = Some(response.to_string().into_bytes());
            if socket
                .send(Message::Binary(ack.encode().into()))
                .await
                .is_err()
            {
                return Ended::Disconnected;
            }
        }
    }
}

/// 等待 `duration`；期间被要求停止返回 true。
async fn wait_or_stop(duration: Duration, stop: &mut watch::Receiver<bool>) -> bool {
    let stopped = tokio::select! {
        _ = tokio::time::sleep(duration) => false,
        _ = stop.wait_for(|stopped| *stopped) => true,
    };
    stopped || *stop.borrow()
}

fn random_unit() -> f64 {
    let mut bytes = [0u8; 4];
    let _ = getrandom::fill(&mut bytes);
    f64::from(u32::from_le_bytes(bytes)) / f64::from(u32::MAX)
}
