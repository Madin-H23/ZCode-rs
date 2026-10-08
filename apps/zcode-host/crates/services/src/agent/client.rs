//! 与单个 CLI 进程的 stdio NDJSON 协议客户端（zcodeStdioTransport.ts、zcodeProtocolClient.ts、
//! zcodeStorageStartupGate.ts）。
//!
//! - 只按 LF 分行（不能按 U+2028 分），去掉行尾 `\r`，空行忽略；任何非法行都关闭连接，
//!   由上层回收进程、下次请求时惰性重启（与 TS 相同）。
//! - 四种信封都是 strict：未知顶层键同样视为协议错误。
//! - Host 发出的请求用递增整数 id；CLI 的反向请求 id 原样回显（字符串保持字符串）。
//! - 普通 app-server 进程先发 `startup/storageState`：请求在存储就绪前不发出；首帧 30 s 未到、
//!   或进入 failed，则所有请求以 `SQLite startup failed: <code>` 失败。
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use serde_json::value::RawValue;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{oneshot, watch};
use zcode_host_kit::log;

pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
const STORAGE_FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(30);
const ALLOWED_KEYS: &[&str] = &["id", "method", "params", "trace", "result", "error"];

#[derive(Debug, Clone, PartialEq)]
pub enum ClientError {
    /// CLI 的 JSON-RPC 错误：`code` / `message` / `data` 原样交给调用方。
    Remote {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    Closed(String),
    Timeout(String),
    StorageFailed(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Remote { message, .. } => f.write_str(message),
            ClientError::Closed(reason) => write!(f, "ZCode agent transport closed: {reason}"),
            ClientError::Timeout(method) => write!(f, "ZCode agent request timed out: {method}"),
            ClientError::StorageFailed(code) => write!(f, "SQLite startup failed: {code}"),
        }
    }
}

/// 一条来自 CLI 的通知；`params` 保留原始 JSON（V4 帧原样转发，不重新编码）。
pub struct Notification {
    pub method: String,
    pub params: Option<Box<RawValue>>,
}

/// CLI 发来的反向请求：处理方用 [`ProtocolClient::respond`] / [`ProtocolClient::respond_error`]
/// 作答，或者不作答（权限与提问请求要一直挂着）。
pub struct ReverseRequest {
    pub id: Box<RawValue>,
    pub method: String,
    pub params: Option<Box<RawValue>>,
}

pub trait ClientHandler: Send + Sync {
    fn notification(&self, client: &Arc<ProtocolClient>, notification: Notification);
    fn request(&self, client: &Arc<ProtocolClient>, request: ReverseRequest);
}

#[derive(Debug, Clone, PartialEq)]
enum Gate {
    Waiting { received: bool },
    Open,
    Failed(String),
}

type Pending = HashMap<i64, (String, oneshot::Sender<Result<Box<RawValue>, ClientError>>)>;

pub struct ProtocolClient {
    writer: tokio::sync::Mutex<Option<Box<dyn AsyncWrite + Send + Unpin>>>,
    next_id: AtomicI64,
    pending: Mutex<Pending>,
    closed: watch::Sender<Option<String>>,
    gate: watch::Sender<Gate>,
    storage_state: watch::Sender<Option<Value>>,
    /// 最近一次请求发出或结束的时间（毫秒）；空闲回收据此判断。
    last_activity_ms: AtomicU64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[derive(serde::Deserialize)]
struct ErrorBody {
    code: i64,
    message: String,
    #[serde(default)]
    data: Option<Value>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StorageFrame {
    attempt_id: String,
    database_id: String,
    sequence: u64,
    phase: String,
    error_code: Option<String>,
}

impl ProtocolClient {
    /// 启动读循环；`requires_storage_startup` 为 true 时请求要等存储就绪。
    pub fn start(
        reader: impl AsyncRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
        requires_storage_startup: bool,
        handler: Arc<dyn ClientHandler>,
    ) -> Arc<Self> {
        let initial = if requires_storage_startup {
            Gate::Waiting { received: false }
        } else {
            Gate::Open
        };
        let (gate, _) = watch::channel(initial);
        let (closed, _) = watch::channel(None);
        let (storage_state, _) = watch::channel(None);
        let client = Arc::new(Self {
            writer: tokio::sync::Mutex::new(Some(Box::new(writer))),
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            closed,
            gate,
            storage_state,
            last_activity_ms: AtomicU64::new(now_ms()),
        });
        if requires_storage_startup {
            let weak = Arc::downgrade(&client);
            tokio::spawn(async move {
                tokio::time::sleep(STORAGE_FIRST_FRAME_TIMEOUT).await;
                if let Some(client) = weak.upgrade() {
                    client.gate.send_if_modified(|gate| {
                        if *gate == (Gate::Waiting { received: false }) {
                            *gate = Gate::Failed("startup_status_timeout".into());
                            true
                        } else {
                            false
                        }
                    });
                }
            });
        }
        let reading = client.clone();
        tokio::spawn(async move {
            let reason = reading.read_loop(reader, handler).await;
            reading.close(&reason);
        });
        client
    }

    async fn read_loop(
        self: &Arc<Self>,
        reader: impl AsyncRead + Send + Unpin,
        handler: Arc<dyn ClientHandler>,
    ) -> String {
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line).await {
                Ok(0) => return "stdout_closed".into(),
                Ok(_) => {}
                Err(error) => return format!("stdout_error: {error}"),
            }
            let mut text = line.as_slice();
            if let Some(stripped) = text.strip_suffix(b"\n") {
                text = stripped;
            }
            if let Some(stripped) = text.strip_suffix(b"\r") {
                text = stripped;
            }
            if text.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            if let Err(error) = self.handle_line(text, &handler) {
                return format!("protocol_parse_error: {error}");
            }
        }
    }

    fn handle_line(
        self: &Arc<Self>,
        text: &[u8],
        handler: &Arc<dyn ClientHandler>,
    ) -> Result<(), String> {
        let text = std::str::from_utf8(text).map_err(|error| error.to_string())?;
        let mut fields: HashMap<String, Box<RawValue>> =
            serde_json::from_str(text).map_err(|error| error.to_string())?;
        if let Some(key) = fields
            .keys()
            .find(|key| !ALLOWED_KEYS.contains(&key.as_str()))
        {
            return Err(format!("unknown envelope key {key}"));
        }
        let method = match fields.remove("method") {
            Some(raw) => Some(
                serde_json::from_str::<String>(raw.get())
                    .ok()
                    .filter(|method| !method.is_empty())
                    .ok_or("invalid method")?,
            ),
            None => None,
        };
        let params = fields.remove("params");
        match (fields.remove("id"), method) {
            (Some(id), Some(method)) => {
                handler.request(self, ReverseRequest { id, method, params });
            }
            (None, Some(method)) => {
                if method == "startup/storageState" {
                    self.accept_storage_frame(params.as_deref());
                } else {
                    handler.notification(self, Notification { method, params });
                }
            }
            (Some(id), None) => {
                let outcome = match (fields.remove("result"), fields.remove("error")) {
                    (Some(result), None) => Ok(result),
                    (None, Some(error)) => {
                        let body: ErrorBody =
                            serde_json::from_str(error.get()).map_err(|error| error.to_string())?;
                        Err(ClientError::Remote {
                            code: body.code,
                            message: body.message,
                            data: body.data,
                        })
                    }
                    _ => return Err("response needs exactly one of result / error".into()),
                };
                // 只有 Host 自己的整数 id 有对应等待者；CLI 的 parse-error 回执等无主响应直接丢弃。
                if let Ok(id) = serde_json::from_str::<i64>(id.get())
                    && let Some((_, sender)) = self.lock_pending().remove(&id)
                {
                    let _ = sender.send(outcome);
                }
            }
            (None, None) => return Err("envelope without id or method".into()),
        }
        Ok(())
    }

    fn accept_storage_frame(&self, params: Option<&RawValue>) {
        let Some(frame) =
            params.and_then(|raw| serde_json::from_str::<StorageFrame>(raw.get()).ok())
        else {
            log::warn("ignored invalid startup/storageState frame");
            return;
        };
        let state = params.and_then(|raw| serde_json::from_str::<Value>(raw.get()).ok());
        let previous = self.storage_state.borrow().clone();
        // 只接受同一次尝试里严格递增的帧，终态之后的帧丢弃。
        if let Some(previous) = &previous {
            let same_attempt = previous["attemptId"] == frame.attempt_id.as_str()
                && previous["databaseId"] == frame.database_id.as_str();
            let newer = previous["sequence"]
                .as_u64()
                .is_some_and(|seq| frame.sequence > seq);
            let terminal = matches!(previous["phase"].as_str(), Some("ready" | "failed"));
            if !same_attempt || !newer || terminal {
                return;
            }
        }
        self.storage_state.send_replace(state);
        self.gate.send_modify(|gate| {
            *gate = match frame.phase.as_str() {
                "ready" => Gate::Open,
                "failed" => Gate::Failed(frame.error_code.unwrap_or_else(|| "sql_failed".into())),
                _ => Gate::Waiting { received: true },
            };
        });
    }

    fn lock_pending(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn is_closed(&self) -> bool {
        self.closed.borrow().is_some()
    }

    /// 连接关闭时完成（返回原因）。
    pub async fn closed(&self) -> String {
        let mut receiver = self.closed.subscribe();
        match receiver.wait_for(Option::is_some).await {
            Ok(reason) => reason.clone().unwrap_or_default(),
            Err(_) => "dropped".into(),
        }
    }

    pub fn close(&self, reason: &str) {
        let changed = self.closed.send_if_modified(|closed| {
            if closed.is_none() {
                *closed = Some(reason.to_owned());
                true
            } else {
                false
            }
        });
        if !changed {
            return;
        }
        for (_, (_, sender)) in self.lock_pending().drain() {
            let _ = sender.send(Err(ClientError::Closed(reason.to_owned())));
        }
        self.gate.send_if_modified(|gate| {
            if matches!(gate, Gate::Waiting { .. }) {
                *gate = Gate::Failed("transport_closed".into());
                true
            } else {
                false
            }
        });
    }

    /// 关闭 stdin：app-server 把 EOF 当作退出请求。
    pub async fn close_input(&self) {
        if let Some(mut writer) = self.writer.lock().await.take() {
            let _ = writer.shutdown().await;
        }
    }

    async fn write_line(&self, line: String) -> Result<(), ClientError> {
        let mut writer = self.writer.lock().await;
        let Some(stream) = writer.as_mut() else {
            return Err(ClientError::Closed("stdin_closed".into()));
        };
        let result = async {
            stream.write_all(line.as_bytes()).await?;
            stream.write_all(b"\n").await?;
            stream.flush().await
        }
        .await;
        result.map_err(|error| ClientError::Closed(format!("stdin_error: {error}")))
    }

    async fn wait_storage(&self) -> Result<(), ClientError> {
        let mut gate = self.gate.subscribe();
        let state = gate
            .wait_for(|gate| !matches!(gate, Gate::Waiting { .. }))
            .await
            .map_err(|_| ClientError::Closed("dropped".into()))?
            .clone();
        match state {
            Gate::Failed(code) => Err(ClientError::StorageFailed(code)),
            _ => Ok(()),
        }
    }

    /// 发请求并等结果；`params` 是已序列化好的 JSON。
    pub async fn request_raw(
        &self,
        method: &str,
        params: &str,
        timeout: Duration,
    ) -> Result<Box<RawValue>, ClientError> {
        if let Some(reason) = self.closed.borrow().clone() {
            return Err(ClientError::Closed(reason));
        }
        self.wait_storage().await?;
        self.touch_activity();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.lock_pending().insert(id, (method.to_owned(), sender));
        let line = format!(
            "{{\"id\":{id},\"method\":{},\"params\":{params}}}",
            serde_json::to_string(method).unwrap_or_default()
        );
        if let Err(error) = self.write_line(line).await {
            self.lock_pending().remove(&id);
            self.close(&error.to_string());
            return Err(error);
        }
        let outcome = match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => Err(ClientError::Closed("dropped".into())),
            Err(_) => {
                self.lock_pending().remove(&id);
                Err(ClientError::Timeout(method.to_owned()))
            }
        };
        self.touch_activity();
        outcome
    }

    fn touch_activity(&self) {
        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// 在途请求数（空闲回收只在归零后计时）。
    pub fn in_flight(&self) -> usize {
        self.lock_pending().len()
    }

    pub fn last_activity_ms(&self) -> u64 {
        self.last_activity_ms.load(Ordering::Relaxed)
    }

    /// 存储门禁仍在等待（迁移中）：空闲回收不能打断。
    pub fn storage_waiting(&self) -> bool {
        matches!(*self.gate.borrow(), Gate::Waiting { .. })
    }

    /// 存储帧的变化（最近一帧被接受的 `startup/storageState` params）。
    pub fn watch_storage(&self) -> watch::Receiver<Option<Value>> {
        self.storage_state.subscribe()
    }

    /// 等存储就绪；failed 时返回 `SQLite startup failed: <code>`。
    pub async fn wait_storage_ready(&self) -> Result<(), ClientError> {
        self.wait_storage().await
    }

    pub async fn request(
        &self,
        method: &str,
        params: &Value,
    ) -> Result<Box<RawValue>, ClientError> {
        self.request_raw(method, &params.to_string(), DEFAULT_REQUEST_TIMEOUT)
            .await
    }

    pub async fn respond(&self, id: &RawValue, result: &Value) {
        let line = format!("{{\"id\":{},\"result\":{result}}}", id.get());
        let _ = self.write_line(line).await;
    }

    pub async fn respond_error(&self, id: &RawValue, code: i64, message: &str) {
        let error = serde_json::json!({"code": code, "message": message});
        let line = format!("{{\"id\":{},\"error\":{error}}}", id.get());
        let _ = self.write_line(line).await;
    }

    /// 带 `data` 的错误应答（`{"id", "error": {code, message, data}}`）。
    pub async fn respond_error_data(&self, id: &RawValue, code: i64, message: &str, data: &Value) {
        let error = serde_json::json!({"code": code, "message": message, "data": data});
        let line = format!("{{\"id\":{},\"error\":{error}}}", id.get());
        let _ = self.write_line(line).await;
    }

    pub fn storage_state(&self) -> Option<Value> {
        self.storage_state.borrow().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, duplex};

    #[derive(Default)]
    struct Recorder {
        notifications: Mutex<Vec<(String, String)>>,
        requests: Mutex<Vec<(String, String)>>,
    }

    impl ClientHandler for Recorder {
        fn notification(&self, _: &Arc<ProtocolClient>, n: Notification) {
            let params = n.params.map(|p| p.get().to_owned()).unwrap_or_default();
            self.notifications.lock().unwrap().push((n.method, params));
        }
        fn request(&self, client: &Arc<ProtocolClient>, r: ReverseRequest) {
            self.requests
                .lock()
                .unwrap()
                .push((r.id.get().to_owned(), r.method.clone()));
            let client = client.clone();
            tokio::spawn(async move { client.respond(&r.id, &json!({"ok": true})).await });
        }
    }

    async fn read_line(cli: &mut tokio::io::DuplexStream) -> String {
        let mut buffer = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            cli.read_exact(&mut byte).await.unwrap();
            if byte[0] == b'\n' {
                return String::from_utf8(buffer).unwrap();
            }
            buffer.push(byte[0]);
        }
    }

    #[tokio::test]
    async fn requests_wait_for_storage_and_correlate_responses() {
        let (host_out, mut cli_in) = duplex(64 * 1024);
        let (mut cli_out, host_in) = duplex(64 * 1024);
        let recorder = Arc::new(Recorder::default());
        let client = ProtocolClient::start(host_in, host_out, true, recorder.clone());

        let request = {
            let client = client.clone();
            tokio::spawn(async move { client.request("v4/command", &json!({"x": 1})).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        let frame = |seq: u64, phase: &str| json!({"method":"startup/storageState","params":{"schemaVersion":1,"attemptId":"a","sequence":seq,"databaseId":"d","databaseKind":"session","phase":phase,"elapsedMs":1}});
        cli_out
            .write_all(format!("{}\n{}\n", frame(1, "checking"), frame(2, "ready")).as_bytes())
            .await
            .unwrap();
        let sent: Value = serde_json::from_str(&read_line(&mut cli_in).await).unwrap();
        assert_eq!(
            sent,
            json!({"id": 1, "method": "v4/command", "params": {"x": 1}})
        );

        // 帧、反向请求、响应混在一起；model 文本里的 U+2028 不能被当作换行。
        cli_out
            .write_all(
                "{\"method\":\"v4/conversation/frame\",\"params\":{\"t\":\"a\u{2028}b\"}}\r\n\n{\"id\":\"server-1\",\"method\":\"session/requestRuntimePreferences\",\"params\":{}}\n{\"id\":1,\"result\":{\"status\":\"accepted\"}}\n"
                    .as_bytes(),
            )
            .await
            .unwrap();
        let result = request.await.unwrap().unwrap();
        assert_eq!(result.get(), r#"{"status":"accepted"}"#);
        let reply: Value = serde_json::from_str(&read_line(&mut cli_in).await).unwrap();
        assert_eq!(reply, json!({"id": "server-1", "result": {"ok": true}}));
        assert_eq!(
            recorder.notifications.lock().unwrap()[0].1,
            "{\"t\":\"a\u{2028}b\"}"
        );
    }

    #[tokio::test]
    async fn invalid_envelope_closes_and_rejects_pending() {
        let (host_out, _cli_in) = duplex(1024);
        let (mut cli_out, host_in) = duplex(1024);
        let client = ProtocolClient::start(host_in, host_out, false, Arc::new(Recorder::default()));
        let pending = {
            let client = client.clone();
            tokio::spawn(async move { client.request("x", &json!({})).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        cli_out
            .write_all(b"{\"id\":1,\"result\":{},\"extra\":1}\n")
            .await
            .unwrap();
        let error = pending.await.unwrap().unwrap_err();
        assert!(
            matches!(error, ClientError::Closed(reason) if reason.starts_with("protocol_parse_error"))
        );
        assert!(client.is_closed());
    }

    #[tokio::test]
    async fn failed_storage_rejects_requests() {
        let (host_out, _cli_in) = duplex(1024);
        let (mut cli_out, host_in) = duplex(4096);
        let client = ProtocolClient::start(host_in, host_out, true, Arc::new(Recorder::default()));
        cli_out
            .write_all(br#"{"method":"startup/storageState","params":{"schemaVersion":1,"attemptId":"a","sequence":1,"databaseId":"d","databaseKind":"session","phase":"failed","elapsedMs":1,"errorCode":"corrupt"}}
"#)
            .await
            .unwrap();
        let error = client.request("x", &json!({})).await.unwrap_err();
        assert_eq!(error, ClientError::StorageFailed("corrupt".into()));
        assert_eq!(error.to_string(), "SQLite startup failed: corrupt");
    }

    #[tokio::test]
    async fn remote_errors_keep_code_and_data() {
        let (host_out, mut cli_in) = duplex(4096);
        let (mut cli_out, host_in) = duplex(4096);
        let client = ProtocolClient::start(host_in, host_out, false, Arc::new(Recorder::default()));
        let pending = {
            let client = client.clone();
            tokio::spawn(async move { client.request("v4/command", &json!({})).await })
        };
        read_line(&mut cli_in).await;
        cli_out
            .write_all(br#"{"id":1,"error":{"code":-32004,"message":"proto.sessionUnavailable","data":{"x":1}}}
"#)
            .await
            .unwrap();
        assert_eq!(
            pending.await.unwrap().unwrap_err(),
            ClientError::Remote {
                code: -32004,
                message: "proto.sessionUnavailable".into(),
                data: Some(json!({"x": 1}))
            }
        );
    }
}
