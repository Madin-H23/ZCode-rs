//! Renderer 的 WebSocket 接入（docs/specs/rust-host-r0.md §4.3）。
//!
//! 握手校验：路径 `/rpc`、`Host` 等于 `127.0.0.1:<port>`（防 DNS rebinding）、浏览器带来的 `Origin`
//! 在白名单内、子协议里的 capability 未用且未过期。非浏览器客户端不带 `Origin`，仍须出示 capability。
//! 每条 WebSocket 二进制消息承载 `SocketProtocol` 字节流，读端按字节流重组。
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode, header};
use zcode_host_core::{AttachmentMeta, HostCore};
use zcode_host_kit::log;
use zcode_host_rpc::{FrameReader, frame};

use crate::flow::FlowGauge;

pub const RPC_SUBPROTOCOL: &str = "zcode-rpc.v1";
const CAPABILITY_PREFIX: &str = "zcode-cap.";
/// 单窗口只有少量 attachment；上限防止本机其他进程开大量空连接占资源。
const MAX_CONNECTIONS: usize = 32;

pub struct Policy {
    host: String,
    allowed_origins: Vec<String>,
}

impl Policy {
    pub fn new(port: u16, configured_origins: Vec<String>) -> Self {
        let mut allowed_origins = Vec::new();
        for origin in configured_origins {
            // Chromium 对 file:// 页面发出的 Origin 可能是 "file://" 或 "null"，两者同等对待。
            if origin == "file://" {
                allowed_origins.push("null".to_owned());
            }
            allowed_origins.push(origin);
        }
        Self {
            host: format!("127.0.0.1:{port}"),
            allowed_origins,
        }
    }
}

pub async fn serve(listener: TcpListener, core: HostCore, policy: Arc<Policy>) {
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                log::warn(format!("accept failed: {error}"));
                continue;
            }
        };
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            log::warn("too many connections; dropping");
            continue;
        };
        let (core, policy) = (core.clone(), policy.clone());
        tokio::spawn(async move {
            handle(stream, core, policy).await;
            drop(permit);
        });
    }
}

fn header_text(request: &Request, name: header::HeaderName) -> Option<&str> {
    request
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
}

/// 依次检查路径、Host、Origin，最后才兑换 capability，避免不合规的请求白白耗掉 capability。
fn authorize(
    request: &Request,
    policy: &Policy,
    core: &HostCore,
) -> Result<AttachmentMeta, String> {
    if request.uri().path() != "/rpc" {
        return Err(format!("unexpected path {}", request.uri().path()));
    }
    if header_text(request, header::HOST) != Some(policy.host.as_str()) {
        return Err("unexpected Host header".to_owned());
    }
    if let Some(origin) = header_text(request, header::ORIGIN)
        && !policy
            .allowed_origins
            .iter()
            .any(|allowed| allowed == origin)
    {
        return Err(format!("origin {origin} is not allowed"));
    }
    let protocols: Vec<&str> = header_text(request, header::SEC_WEBSOCKET_PROTOCOL)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .collect();
    if !protocols.contains(&RPC_SUBPROTOCOL) {
        return Err("missing rpc subprotocol".to_owned());
    }
    let mut capabilities = protocols
        .iter()
        .filter_map(|protocol| protocol.strip_prefix(CAPABILITY_PREFIX));
    let (Some(capability), None) = (capabilities.next(), capabilities.next()) else {
        return Err("expected exactly one capability".to_owned());
    };
    core.redeem_capability(capability)
        .ok_or_else(|| "capability is unknown, used or expired".to_owned())
}

fn forbidden() -> ErrorResponse {
    let mut response = ErrorResponse::new(None);
    *response.status_mut() = StatusCode::FORBIDDEN;
    response
}

async fn handle(stream: TcpStream, core: HostCore, policy: Arc<Policy>) {
    let _ = stream.set_nodelay(true);
    let granted: Arc<Mutex<Option<AttachmentMeta>>> = Arc::new(Mutex::new(None));
    // 回调签名由 tungstenite 规定，Err 必须是 ErrorResponse（一个 http::Response）。
    #[allow(clippy::result_large_err)]
    let callback = {
        let (granted, core, policy) = (granted.clone(), core.clone(), policy.clone());
        move |request: &Request, mut response: Response| match authorize(request, &policy, &core) {
            Ok(meta) => {
                *granted
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(meta);
                response.headers_mut().insert(
                    header::SEC_WEBSOCKET_PROTOCOL,
                    HeaderValue::from_static(RPC_SUBPROTOCOL),
                );
                Ok(response)
            }
            Err(reason) => {
                // 不记录 capability 本身。
                log::warn(format!("websocket handshake rejected: {reason}"));
                Err(forbidden())
            }
        }
    };
    let Ok(socket) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
        return;
    };
    let Some(meta) = granted
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    else {
        return;
    };
    log::info(format!("attachment {} connected", meta.attachment_id));

    let (mut sink, mut source) = socket.split();
    let (to_core, inbound) = mpsc::unbounded_channel();
    let (outbound, mut from_core) = mpsc::unbounded_channel::<Vec<u8>>();
    let (gauge, flow) = FlowGauge::new();
    let gauge = Arc::new(gauge);
    core.attach_with_flow(meta, inbound, outbound, flow);

    // core → 计数 → 写任务：计数任务只搬运消息，积压因此等于“已交出、未写进 socket”的字节数。
    let (to_writer, mut writer_queue) = mpsc::unbounded_channel::<Vec<u8>>();
    let counted = gauge.clone();
    tokio::spawn(async move {
        while let Some(message) = from_core.recv().await {
            counted.queued(message.len());
            if to_writer.send(message).is_err() {
                return;
            }
        }
    });
    tokio::spawn(async move {
        while let Some(message) = writer_queue.recv().await {
            let size = message.len();
            if sink
                .send(Message::Binary(frame(&message).into()))
                .await
                .is_err()
            {
                break;
            }
            gauge.written(size);
        }
        let _ = sink.close().await;
    });

    let mut frames = FrameReader::default();
    while let Some(item) = source.next().await {
        match item {
            Ok(Message::Binary(data)) => {
                frames.push(&data);
                while let Some(message) = frames.next_message() {
                    if to_core.send(message).is_err() {
                        return;
                    }
                }
            }
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
    // to_core 在此 drop：服务端循环结束，取消这条连接上的调用与订阅，写任务随之结束。
}
