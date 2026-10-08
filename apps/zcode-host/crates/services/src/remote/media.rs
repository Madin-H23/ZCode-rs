//! 远端媒体预览代理（`remoteMediaPreviewProxy.ts`、`remoteMediaPreviewProxyHelpers.ts`），规则见
//! rust-host-remote.md §2.8：只给 desktop-continuous 的远端 attachment；Renderer 的 `<video>` 经
//! 127.0.0.1 上的 Range URL 播放，Host 按 1 MiB 分块读远端 `file.readFileRange`。
//!
//! 每个 attachment 一个代理（随连接关闭释放）；并发上限每租约 2 个、整个 Host 4 个 GET。
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::AbortHandle;
use zcode_host_kit::{
    CallContext, CallFuture, EventStream, Reply, RpcError, RpcValue, ServiceChannel, log,
};
use zcode_host_rpc::RpcClient;

use super::registry::RemoteScope;

const ROUTE_PREFIX: &str = "/__zcode_media/";
const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_CONCURRENT_PER_LEASE: usize = 2;
const MAX_CONCURRENT_PER_HOST: usize = 4;
const CHUNK_BYTES: u64 = 1024 * 1024;
const IDLE_TTL_MS: u64 = 10 * 60 * 1000;
const TTL_MS: u64 = 2 * 60 * 60 * 1000;
const MAX_REQUEST_HEAD: usize = 16 * 1024;

/// 整个 Host 的远端媒体并发 GET（`hostRemoteMediaRequestLimiter`）。
static HOST_ACTIVE: AtomicUsize = AtomicUsize::new(0);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn fault(message: impl Into<String>) -> RpcError {
    RpcError::new("Error", message)
}

/// `MEDIA_PREVIEW_FORMATS`（packages/shared/src/media-preview.ts）：(扩展名, kind, mediaType)。
const FORMATS: &[(&str, &str, &str)] = &[
    (".mp4", "video", "video/mp4"),
    (".mov", "video", "video/quicktime"),
    (".webm", "video", "video/webm"),
    (".m4v", "video", "video/x-m4v"),
    (".mp3", "audio", "audio/mpeg"),
    (".wav", "audio", "audio/wav"),
    (".m4a", "audio", "audio/mp4"),
    (".ogg", "audio", "audio/ogg"),
    (".opus", "audio", "audio/opus"),
    (".flac", "audio", "audio/flac"),
    (".weba", "audio", "audio/webm"),
];

fn media_format(path: &str) -> Option<(&'static str, &'static str)> {
    let normalized = path.replace('\\', "/").to_lowercase();
    FORMATS
        .iter()
        .find(|(extension, _, _)| normalized.ends_with(extension))
        .map(|(_, kind, media_type)| (*kind, *media_type))
}

/// `isPathWithinWorkspace`。
fn within_workspace(path: &str, workspace: &str) -> bool {
    let normalize = |value: &str| {
        let unified = value.replace('\\', "/");
        let mut collapsed = String::with_capacity(unified.len());
        for character in unified.chars() {
            if character == '/' && collapsed.ends_with('/') {
                continue;
            }
            collapsed.push(character);
        }
        if collapsed.len() > 1 && collapsed.ends_with('/') {
            collapsed.pop();
        }
        collapsed
    };
    let (candidate, root) = (normalize(path), normalize(workspace));
    if root == "/" {
        return candidate.starts_with('/');
    }
    candidate == root || candidate.starts_with(&format!("{root}/"))
}

#[derive(Debug, PartialEq, Eq)]
enum Range {
    Full,
    Invalid,
    Bytes(u64, u64),
}

/// JS `Number(text)` 对十进制整数的结果；不是非负整数时为 None。
fn js_integer(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    let number = if trimmed.is_empty() {
        0.0
    } else {
        trimmed.parse::<f64>().ok()?
    };
    (number.is_finite() && number.fract() == 0.0).then_some(number)
}

/// `parseRange`：单段 bytes 区间、后缀区间；越界或多段为 Invalid。
fn parse_range(value: Option<&str>, size: u64) -> Range {
    let Some(value) = value else {
        return Range::Full;
    };
    let Some(spec) = value.strip_prefix("bytes=") else {
        return Range::Invalid;
    };
    if spec.contains(',') {
        return Range::Invalid;
    }
    let mut parts = spec.splitn(2, '-');
    let start_text = parts.next().unwrap_or_default();
    let end_text = parts.next();
    if start_text.is_empty() && end_text.is_none_or(str::is_empty) {
        return Range::Invalid;
    }
    if size == 0 {
        return Range::Invalid;
    }
    let size_f = size as f64;
    if start_text.is_empty() {
        let Some(suffix) = end_text.and_then(js_integer).filter(|suffix| *suffix > 0.0) else {
            return Range::Invalid;
        };
        return Range::Bytes((size_f - suffix).max(0.0) as u64, size - 1);
    }
    let start = js_integer(start_text);
    let end = match end_text.filter(|text| !text.is_empty()) {
        Some(text) => js_integer(text),
        None => Some(size_f - 1.0),
    };
    match (start, end) {
        (Some(start), Some(end)) if start >= 0.0 && start < size_f && end >= start => {
            Range::Bytes(start as u64, end.min(size_f - 1.0) as u64)
        }
        _ => Range::Invalid,
    }
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("operating system random source unavailable");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

struct Lease {
    preview_id: String,
    token: String,
    path: String,
    media_type: &'static str,
    size: u64,
    expires_at: u64,
    last_access: u64,
    active: usize,
    stale_logged: bool,
}

impl Lease {
    fn alive(&self, now: u64) -> bool {
        self.expires_at > now && self.last_access + IDLE_TTL_MS > now
    }
}

#[derive(Default)]
struct State {
    /// previewId → lease
    leases: HashMap<String, Lease>,
    /// token → previewId
    tokens: HashMap<String, String>,
    port: Option<u16>,
    server: Option<AbortHandle>,
}

impl State {
    fn remove(&mut self, preview_id: &str) {
        if let Some(lease) = self.leases.remove(preview_id) {
            self.tokens.remove(&lease.token);
        }
    }
}

pub struct MediaPreviewProxy {
    me: Weak<MediaPreviewProxy>,
    client: RpcClient,
    scope: RemoteScope,
    state: Mutex<State>,
    disposed: AtomicBool,
    listening: tokio::sync::Mutex<()>,
}

/// 一个请求槽：每租约与整个 Host 两级上限，释放时同时归还。
struct Slot {
    proxy: Arc<MediaPreviewProxy>,
    preview_id: String,
}

impl Drop for Slot {
    fn drop(&mut self) {
        HOST_ACTIVE.fetch_sub(1, Ordering::SeqCst);
        if let Some(lease) = self.proxy.lock().leases.get_mut(&self.preview_id) {
            lease.active = lease.active.saturating_sub(1);
        }
    }
}

impl MediaPreviewProxy {
    pub fn new(client: RpcClient, scope: RemoteScope) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            client,
            scope,
            state: Mutex::new(State::default()),
            disposed: AtomicBool::new(false),
            listening: tokio::sync::Mutex::new(()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn url(&self, token: &str) -> Result<String, RpcError> {
        let port = self
            .lock()
            .port
            .ok_or_else(|| fault("Remote media preview proxy is not listening"))?;
        Ok(format!("http://127.0.0.1:{port}{ROUTE_PREFIX}{token}"))
    }

    async fn file_call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        Ok(self
            .client
            .call("file", method, vec![RpcValue::Json(params)])
            .await?
            .into_json())
    }

    async fn stat(&self, path: &str) -> Result<(bool, Option<u64>), RpcError> {
        let stat = self.file_call("stat", json!({"path": path})).await?;
        Ok((stat["type"] == "file", stat["size"].as_u64()))
    }

    async fn ensure_listening(&self) -> Result<(), RpcError> {
        let _guard = self.listening.lock().await;
        if self.lock().port.is_some() {
            return Ok(());
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(|error| {
            fault(format!(
                "Remote media preview proxy failed to listen: {error}"
            ))
        })?;
        let port = listener
            .local_addr()
            .map_err(|error| fault(error.to_string()))?
            .port();
        let proxy = self.me.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let Some(proxy) = proxy.upgrade() else { return };
                tokio::spawn(async move { proxy.serve(stream).await });
            }
        });
        let mut state = self.lock();
        state.port = Some(port);
        state.server = Some(server.abort_handle());
        Ok(())
    }

    async fn prepare(&self, params: Value) -> Result<Value, RpcError> {
        let path = params["path"]
            .as_str()
            .ok_or_else(|| fault("invalid argument path: is required"))?
            .to_owned();
        let expected = params["expectedKind"].as_str().unwrap_or_default();
        let resolved = self
            .file_call("resolvePath", json!({"path": path}))
            .await?
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if !within_workspace(&resolved, &self.scope.workspace_path) {
            return Err(fault("Media preview path is outside remote workspace"));
        }
        let Some((kind, media_type)) = media_format(&resolved) else {
            return Err(fault(format!("Unsupported media preview format: {path}")));
        };
        let (is_file, size) = self.stat(&resolved).await?;
        let Some(size) = size.filter(|_| is_file) else {
            return Err(fault(format!("Path is not a media file: {path}")));
        };
        if size > MAX_FILE_BYTES {
            return Err(fault(format!(
                "Media file is too large for remote preview: {path}"
            )));
        }
        if kind != expected {
            return Err(fault(format!("Unsupported media preview format: {path}")));
        }
        self.ensure_listening().await?;
        let now = now_ms();
        let lease = Lease {
            preview_id: crate::agent::service::uuid_v4(),
            token: random_token(),
            path: resolved.clone(),
            media_type,
            size,
            expires_at: now + TTL_MS,
            last_access: now,
            active: 0,
            stale_logged: false,
        };
        let url = self.url(&lease.token)?;
        let preview_id = lease.preview_id.clone();
        let expires_at = lease.expires_at;
        {
            let mut state = self.lock();
            state.tokens.insert(lease.token.clone(), preview_id.clone());
            state.leases.insert(preview_id.clone(), lease);
        }
        Ok(json!({
            "kind": "host-range-url",
            "previewId": preview_id,
            "mediaType": media_type,
            "path": resolved,
            "size": size,
            "url": url,
            "urlExpiresAt": expires_at,
        }))
    }

    fn refresh(&self, params: &Value) -> Result<Value, RpcError> {
        let preview_id = params["previewId"].as_str().unwrap_or_default();
        let now = now_ms();
        let token = {
            let mut state = self.lock();
            let alive = !self.disposed.load(Ordering::SeqCst)
                && state
                    .leases
                    .get(preview_id)
                    .is_some_and(|lease| lease.alive(now));
            if !alive {
                state.remove(preview_id);
                return Err(fault("Remote media preview lease expired"));
            }
            let token = random_token();
            let lease = state.leases.get_mut(preview_id).expect("lease checked");
            let old = std::mem::replace(&mut lease.token, token.clone());
            lease.last_access = now;
            lease.expires_at = now + TTL_MS;
            state.tokens.remove(&old);
            state.tokens.insert(token.clone(), preview_id.to_owned());
            token
        };
        Ok(json!({"url": self.url(&token)?, "expiresAt": now + TTL_MS}))
    }

    fn release(&self, params: &Value) {
        if let Some(preview_id) = params["previewId"].as_str() {
            self.lock().remove(preview_id);
        }
    }

    pub fn dispose(&self) {
        self.disposed.store(true, Ordering::SeqCst);
        let mut state = self.lock();
        state.leases.clear();
        state.tokens.clear();
        if let Some(server) = state.server.take() {
            server.abort();
        }
    }

    fn acquire(self: &Arc<Self>, preview_id: &str) -> Option<Slot> {
        let mut state = self.lock();
        let lease = state.leases.get_mut(preview_id)?;
        // 并发检查与递增之间不能有 await，否则多个请求会同时越过上限。
        if lease.active >= MAX_CONCURRENT_PER_LEASE {
            return None;
        }
        if HOST_ACTIVE.fetch_add(1, Ordering::SeqCst) >= MAX_CONCURRENT_PER_HOST {
            HOST_ACTIVE.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        lease.active += 1;
        Some(Slot {
            proxy: self.clone(),
            preview_id: preview_id.to_owned(),
        })
    }

    fn mark_stale(&self, preview_id: &str) {
        let mut state = self.lock();
        if let Some(lease) = state.leases.get_mut(preview_id)
            && !lease.stale_logged
        {
            lease.stale_logged = true;
            log::warn(format!("media-preview.range STALE previewId={preview_id}"));
        }
        state.remove(preview_id);
    }

    async fn serve(self: Arc<Self>, mut stream: TcpStream) {
        let mut head = Vec::new();
        let mut chunk = [0u8; 4096];
        let end = loop {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(read) => head.extend_from_slice(&chunk[..read]),
            }
            if let Some(position) = head.windows(4).position(|window| window == b"\r\n\r\n") {
                break position;
            }
            if head.len() > MAX_REQUEST_HEAD {
                return;
            }
        };
        let text = String::from_utf8_lossy(&head[..end]).into_owned();
        let mut lines = text.split("\r\n");
        let mut request_line = lines.next().unwrap_or_default().split(' ');
        let method = request_line.next().unwrap_or_default().to_owned();
        let target = request_line.next().unwrap_or_default().to_owned();
        let range = lines.find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("range")
                .then(|| value.trim().to_owned())
        });
        self.handle(&mut stream, &method, &target, range.as_deref())
            .await;
        let _ = stream.shutdown().await;
    }

    async fn handle(
        self: &Arc<Self>,
        stream: &mut TcpStream,
        method: &str,
        target: &str,
        range: Option<&str>,
    ) {
        let path = target.split('?').next().unwrap_or_default();
        let Some(token) = path.strip_prefix(ROUTE_PREFIX) else {
            return write_error(stream, 404, "Not found", &[]).await;
        };
        if method != "GET" && method != "HEAD" {
            return write_error(
                stream,
                405,
                "Method not allowed",
                &[("Allow", "GET, HEAD".into())],
            )
            .await;
        }
        let now = now_ms();
        let lease = {
            let mut state = self.lock();
            let preview_id = state.tokens.get(token).cloned();
            let lease = preview_id
                .as_ref()
                .and_then(|id| state.leases.get(id))
                .map(|lease| {
                    (
                        lease.preview_id.clone(),
                        lease.path.clone(),
                        lease.size,
                        lease.media_type,
                        lease.alive(now),
                    )
                });
            match lease {
                Some((preview_id, path, size, media_type, true))
                    if !self.disposed.load(Ordering::SeqCst) =>
                {
                    Some((preview_id, path, size, media_type))
                }
                Some((preview_id, ..)) => {
                    state.remove(&preview_id);
                    None
                }
                None => None,
            }
        };
        let Some((preview_id, path, size, media_type)) = lease else {
            return write_error(stream, 404, "Media preview lease expired", &[]).await;
        };
        let (start, end) = match parse_range(range, size) {
            Range::Invalid => {
                return write_error(
                    stream,
                    416,
                    "Invalid media range",
                    &[("Content-Range", format!("bytes */{size}"))],
                )
                .await;
            }
            Range::Full => (0, size.saturating_sub(1)),
            Range::Bytes(start, end) => (start, end),
        };
        let partial = !matches!(parse_range(range, size), Range::Full);
        let length = if size == 0 { 0 } else { end - start + 1 };
        let _slot = if method == "GET" && length > 0 {
            match self.acquire(&preview_id) {
                Some(slot) => Some(slot),
                None => {
                    log::warn("media-preview.range BUSY");
                    return write_error(
                        stream,
                        429,
                        "Too many media range requests",
                        &[("Retry-After", "1".into())],
                    )
                    .await;
                }
            }
        } else {
            None
        };
        match self.stat(&path).await {
            Ok((true, Some(current))) if current == size => {}
            _ => {
                self.mark_stale(&preview_id);
                return write_error(stream, 409, "Remote media file changed during preview", &[])
                    .await;
            }
        }
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nAccept-Ranges: bytes\r\nCache-Control: no-store\r\nContent-Disposition: inline\r\nContent-Length: {length}\r\nContent-Type: {media_type}\r\n",
            if partial { 206 } else { 200 },
            if partial { "Partial Content" } else { "OK" },
        );
        if partial {
            head.push_str(&format!("Content-Range: bytes {start}-{end}/{size}\r\n"));
        }
        head.push_str("Connection: close\r\n\r\n");
        if let Some(lease) = self.lock().leases.get_mut(&preview_id) {
            lease.last_access = now_ms();
        }
        if stream.write_all(head.as_bytes()).await.is_err() || method == "HEAD" || length == 0 {
            return;
        }
        let mut offset = start;
        while offset <= end {
            let requested = CHUNK_BYTES.min(end - offset + 1);
            let chunk = match self
                .client
                .call(
                    "file",
                    "readFileRange",
                    vec![RpcValue::Json(
                        json!({"path": path, "offset": offset, "length": requested}),
                    )],
                )
                .await
            {
                Ok(RpcValue::Bytes(bytes)) => bytes,
                _ => {
                    self.mark_stale(&preview_id);
                    return;
                }
            };
            if chunk.len() as u64 != requested {
                self.mark_stale(&preview_id);
                return;
            }
            offset += requested;
            // 写失败即客户端断开：停止读取远端并归还请求槽。
            if stream.write_all(&chunk).await.is_err() {
                return;
            }
        }
        if !matches!(self.stat(&path).await, Ok((true, Some(current))) if current == size) {
            self.mark_stale(&preview_id);
        }
    }
}

async fn write_error(
    stream: &mut TcpStream,
    status: u16,
    message: &str,
    headers: &[(&str, String)],
) {
    let reason = match status {
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        416 => "Range Not Satisfiable",
        429 => "Too Many Requests",
        _ => "Error",
    };
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n",
        message.len()
    );
    for (name, value) in headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str("\r\n");
    response.push_str(message);
    let _ = stream.write_all(response.as_bytes()).await;
}

/// `media-preview` channel：只有 prepare、refreshPlaybackUrl、release 三个成员。
pub struct MediaPreviewChannel(pub Arc<MediaPreviewProxy>);

impl ServiceChannel for MediaPreviewChannel {
    fn call(&self, _context: &CallContext, method: &str, args: Vec<RpcValue>) -> CallFuture {
        let proxy = self.0.clone();
        let params = args
            .into_iter()
            .next()
            .map(RpcValue::into_json)
            .unwrap_or(Value::Null);
        let method = method.to_owned();
        Box::pin(async move {
            match method.as_str() {
                "prepare" => proxy.prepare(params).await.map(Reply::Value),
                "refreshPlaybackUrl" => proxy.refresh(&params).map(Reply::Value),
                "release" => {
                    proxy.release(&params);
                    Ok(Reply::Undefined)
                }
                other => Err(fault(format!("Method not found: {other}"))),
            }
        })
    }

    fn listen(
        &self,
        _context: &CallContext,
        event: &str,
        _arg: RpcValue,
    ) -> Result<EventStream, RpcError> {
        Err(fault(format!("Event not found: {event}")))
    }

    fn connection_closed(&self, _context: &CallContext) {
        self.0.dispose();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_follow_parse_range() {
        assert_eq!(parse_range(None, 10), Range::Full);
        assert_eq!(parse_range(Some("bytes=0-4"), 10), Range::Bytes(0, 4));
        assert_eq!(parse_range(Some("bytes=5-"), 10), Range::Bytes(5, 9));
        assert_eq!(parse_range(Some("bytes=-3"), 10), Range::Bytes(7, 9));
        assert_eq!(parse_range(Some("bytes=-30"), 10), Range::Bytes(0, 9));
        assert_eq!(parse_range(Some("bytes=8-100"), 10), Range::Bytes(8, 9));
        assert_eq!(parse_range(Some("bytes=10-11"), 10), Range::Invalid);
        assert_eq!(parse_range(Some("bytes=4-2"), 10), Range::Invalid);
        assert_eq!(parse_range(Some("bytes=0-1,3-4"), 10), Range::Invalid);
        assert_eq!(parse_range(Some("items=0-1"), 10), Range::Invalid);
        assert_eq!(parse_range(Some("bytes=-"), 10), Range::Invalid);
        assert_eq!(parse_range(Some("bytes=-0"), 10), Range::Invalid);
        assert_eq!(parse_range(Some("bytes=0-1"), 0), Range::Invalid);
    }

    #[test]
    fn formats_and_workspace_containment() {
        assert_eq!(media_format("/w/A.MP4"), Some(("video", "video/mp4")));
        assert_eq!(media_format("C:\\w\\a.weba"), Some(("audio", "audio/webm")));
        assert_eq!(media_format("/w/a.png"), None);
        assert!(within_workspace("/w/a.mp4", "/w/"));
        assert!(within_workspace("/w//x/a.mp4", "/w"));
        assert!(!within_workspace("/wx/a.mp4", "/w"));
        assert!(within_workspace("/any", "/"));
    }
}
