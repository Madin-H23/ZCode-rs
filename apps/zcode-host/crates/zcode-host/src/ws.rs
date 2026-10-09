//! Renderer 的 WebSocket 接入（docs/specs/rust-host-r0.md §4.3）。
//!
//! 握手校验：路径 `/rpc`、`Host` 等于 `127.0.0.1:<port>`（防 DNS rebinding）、浏览器带来的 `Origin`
//! 在白名单内、子协议里的 capability 未用且未过期。非浏览器客户端不带 `Origin`，仍须出示 capability。
//! 每条 WebSocket 二进制消息承载 `SocketProtocol` 字节流，读端按字节流重组。
//!
//! transport 只做握手、鉴权与分帧（docs/specs/rust-u2-gateway.md §2.1）：已鉴权的连接交给 Gateway，
//! 同一个任务里并发地读（完整 rpc 消息 → `Inbound`）和写（`Outbound` → WebSocket 帧）。
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode, header};
use zcode_host_core::{AttachmentMeta, HostCore};
use zcode_host_gateway::Connection;
use zcode_host_kit::log;
use zcode_host_rpc::{FrameReader, frame};

pub const RPC_SUBPROTOCOL: &str = "zcode-rpc.v1";
const CAPABILITY_PREFIX: &str = "zcode-cap.";
/// App 级 Host（rust-u4-app-host.md）的连接上限，握手前的连接也占名额。每个主窗口一条本地 attachment，
/// 加上每个已打开的远端 workspace 一条远端 scope attachment，reload 时新旧连接短暂并存；产品只有一个主窗口，
/// 开发开关最多两个（§2.9）。64 给多窗口留出余量，同时仍限制本机其他进程开大量空连接占资源。
const MAX_CONNECTIONS: usize = 64;
/// 连接被 Gateway 关闭后发送 WebSocket Close 的时限。对端可能已经不读（挂起的界面进程、越过出站上限），
/// 发送缓冲写满时 Close 永远写不出去；到时直接断开 TCP，不让 socket 任务一直挂着。
const CLOSE_FRAME_GRACE: Duration = Duration::from_secs(1);

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
    // 每条 socket 一个任务，属于 `host/websocket`；监听任务结束（Host 退出）时随之中止。
    let sockets = core.spawner().child("websocket");
    let spawner = sockets.spawner();
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
        drop(spawner.spawn("socket", async move {
            handle(stream, core, policy).await;
            drop(permit);
        }));
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

    let Connection {
        inbound, outbound, ..
    } = core.connect(meta);
    let (mut sink, mut source) = socket.split();
    // 读：重组出完整的 rpc 消息交给 Gateway。入站队列满时在这里等待，socket 随之不再被读（TCP 背压）。
    // 结束时（对端关闭、读错误、Gateway 已关闭连接）释放 `inbound`，Gateway 随之按顺序收口。
    let reading = async move {
        let mut frames = FrameReader::default();
        loop {
            let item = tokio::select! {
                item = source.next() => item,
                () = inbound.closed() => return,
            };
            match item {
                Some(Ok(Message::Binary(data))) => {
                    frames.push(&data);
                    while let Some(message) = frames.next_message() {
                        if inbound.send(message).await.is_err() {
                            return;
                        }
                    }
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(_)) => {}
            }
        }
    };
    // 写：按出站队列的顺序发送，发完一条报告一次（积压水位由 Gateway 计算）。写失败时关闭连接。
    let writing = async {
        while let Some(message) = outbound.next().await {
            let size = message.len();
            if sink
                .send(Message::Binary(frame(&message).into()))
                .await
                .is_err()
            {
                outbound.close();
                return;
            }
            outbound.written(size);
        }
        let _ = tokio::time::timeout(CLOSE_FRAME_GRACE, sink.close()).await;
    };
    tokio::join!(reading, writing);
}
