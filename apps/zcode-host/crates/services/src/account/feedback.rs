//! `feedback` channel（packages/services/src/feedback/**）。
//!
//! 工单增删查、附件经 OSS 表单直传（带进度与取消）、匿名工单本地索引（`feedback/tickets.json`）、
//! 当天诊断日志的压缩归档（脱敏后打 zip）；“完整日志”归档委托 Main（`feedback-log-archive-request`
//! → `feedback-log-archive-result`），与 Node Host 相同。
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use tokio::sync::watch;
use zcode_host_control::ControlRouter;
use zcode_host_kit::{EventHub, Reply, RpcError, ServiceBuilder, log, replies};

use super::credential::{CredentialStore, node_platform};
use super::net::{ApiClient, ApiRequest, node_arch, uuid_v4};
use super::oauth_repo::{ZCODE_JWT_TOKEN_KEY, encode_uri_component};
use super::privacy::redact_feedback_text;
use crate::main_bridge::MainBridge;

const MAX_ATTACHMENT_BYTES: u64 = 100 * 1024 * 1024;
const MAX_LOG_ATTACHMENT_BYTES: u64 = 1024 * 1024 * 1024;
const CREATE_TIMEOUT_MS: u64 = 30_000;
const MAX_LOG_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_LOG_TOTAL_BYTES: u64 = 32 * 1024 * 1024;
const UPLOAD_CANCELED: &str = "Feedback upload canceled";
const FEEDBACK_STATUSES: [&str; 9] = [
    "已提交", "信息不足", "已采纳", "答复关闭", "已归档", "已拒绝", "开发中", "已解决", "已上线",
];

fn error(message: impl Into<String>) -> RpcError {
    RpcError::new("Error", message)
}

fn read_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn iso_now() -> String {
    super::onboarding::iso_now()
}

/// `normalizeFeedbackTime`。
fn normalize_time(value: &Value, fallback: Option<&str>) -> String {
    if let Some(seconds) = value.as_f64().filter(|v| v.is_finite()) {
        return super::onboarding::iso_from_ms((seconds * 1000.0) as i64);
    }
    let Some(text) = read_string(value) else {
        return fallback.map(str::to_owned).unwrap_or_else(iso_now);
    };
    if text.bytes().all(|b| b.is_ascii_digit()) {
        let seconds: f64 = text.parse().unwrap_or(0.0);
        return super::onboarding::iso_from_ms((seconds * 1000.0) as i64);
    }
    let iso = regex::Regex::new(r"^\d{4}-\d{2}-\d{2}T").expect("static regex");
    let zoned = regex::Regex::new(r"(?:Z|[+-]\d{2}:?\d{2})$").expect("static regex");
    if iso.is_match(&text) && !zoned.is_match(&text) {
        return format!("{text}Z");
    }
    text
}

/// `toStableNumericId`：字符串按 `hash*31 + charCode` 取 32 位。
fn stable_numeric_id(value: &Value, fallback: u64) -> Value {
    if value.is_number() {
        return value.clone();
    }
    let Some(text) = read_string(value) else {
        return json!(fallback);
    };
    let mut hash: u32 = 0;
    for unit in text.encode_utf16() {
        hash = hash.wrapping_mul(31).wrapping_add(u32::from(unit));
    }
    json!(if hash == 0 { fallback } else { u64::from(hash) })
}

fn map_status(status: &Value) -> &'static str {
    match read_string(status).as_deref() {
        Some("closed" | "已归档") => "已归档",
        _ => "已提交",
    }
}

fn infer_attachment_kind(filename: &str, content_type: Option<&str>) -> &'static str {
    if content_type.is_some_and(|t| t.starts_with("image/")) {
        return "image";
    }
    let lower = filename.to_lowercase();
    if lower.ends_with(".zip") {
        return "log";
    }
    if [".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp"].iter().any(|ext| lower.ends_with(ext)) {
        return "image";
    }
    "other"
}

fn map_attachment(attachment: &Value, fallback: u64) -> Value {
    let filename = read_string(&attachment["file_name"])
        .or_else(|| read_string(&attachment["filename"]))
        .unwrap_or_else(|| "attachment".to_owned());
    let content_type = read_string(&attachment["content_type"]);
    let id = if attachment["attachment_id"].is_null() {
        &attachment["id"]
    } else {
        &attachment["attachment_id"]
    };
    json!({
        "id": stable_numeric_id(id, fallback),
        "kind": infer_attachment_kind(&filename, content_type.as_deref()),
        "filename": filename,
        "size": if attachment["size"].is_number() { attachment["size"].clone() } else { json!(0) },
        "redacted": false,
        "content_type": content_type,
        "download_url": read_string(&attachment["download_url"]),
        "preview_url": read_string(&attachment["preview_url"]),
        "created_at": normalize_time(&attachment["created_at"], None),
    })
}

fn map_comment(message: &Value, fallback: u64) -> Value {
    let sender = read_string(&message["sender_type"]);
    let attachments: Vec<Value> = message["attachments"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, attachment)| {
            let mapped = map_attachment(attachment, index as u64 + 1);
            json!({
                "id": mapped["id"], "filename": mapped["filename"], "size": mapped["size"],
                "content_type": mapped["content_type"], "download_url": mapped["download_url"],
                "preview_url": mapped["preview_url"],
            })
        })
        .collect();
    json!({
        "id": stable_numeric_id(&message["message_id"], fallback),
        "message_id": message["message_id"],
        "body": read_string(&message["content"]["text"]).unwrap_or_default(),
        "is_staff": matches!(sender.as_deref(), Some("staff" | "admin")),
        "attachments": attachments,
        "created_at": normalize_time(&message["created_at"], None),
    })
}

fn map_summary(ticket: &Value) -> Value {
    let created = normalize_time(&ticket["created_at"], None);
    json!({
        "id": ticket["ticket_id"],
        "title": read_string(&ticket["title"]).map(Value::String).unwrap_or_else(|| ticket["ticket_id"].clone()),
        "type": "bug",
        "status": map_status(&ticket["status"]),
        "updated_at": normalize_time(&ticket["updated_at"], Some(&created)),
        "created_at": created,
    })
}

fn reorder_summary(summary: Value) -> Value {
    // TS 对象键序：id、title、type、status、created_at、updated_at。
    let mut ordered = Map::new();
    for key in ["id", "title", "type", "status", "created_at", "updated_at"] {
        if let Some(value) = summary.get(key) {
            ordered.insert(key.to_owned(), value.clone());
        }
    }
    Value::Object(ordered)
}

fn environment_to_device(environment: &Value) -> Value {
    let Some(environment) = environment.as_object() else {
        return Value::Null;
    };
    let mut device = Map::new();
    for (key, source) in [
        ("appVersion", "app_version"),
        ("buildCommitId", "build_commit_id"),
        ("buildTime", "build_time"),
        ("electronVersion", "electron_version"),
        ("nodeVersion", "node_version"),
        ("osType", "os_type"),
        ("osPlatform", "os_platform"),
        ("osRelease", "os_release"),
        ("osVersion", "os_version"),
        ("osArch", "os_arch"),
        ("agentProvider", "agent_provider"),
        ("agentFramework", "agent_framework"),
        ("agentModel", "agent_model"),
        ("agentModelDisplay", "agent_model_display"),
    ] {
        let value = if key == "osPlatform" && environment.get(source).is_none_or(Value::is_null) {
            environment.get("os_category")
        } else {
            environment.get(source)
        };
        if let Some(text) = value.and_then(read_string) {
            device.insert(key.to_owned(), json!(text));
        }
    }
    if let Some(count) = environment.get("agent_model_option_count").filter(|v| v.is_number()) {
        device.insert("agentModelOptionCount".into(), count.clone());
    }
    if let Some(preview) = environment.get("agent_model_options_preview").and_then(Value::as_array) {
        device.insert(
            "agentModelOptionsPreview".into(),
            Value::Array(preview.iter().filter(|item| item.is_string()).cloned().collect()),
        );
    }
    if device.is_empty() {
        Value::Null
    } else {
        Value::Object(device)
    }
}

fn map_detail(ticket: &Value) -> Value {
    let summary = map_summary(ticket);
    let kind = match read_string(&ticket["content"]["category"]).as_deref() {
        Some(kind @ ("usage" | "feature" | "performance" | "bug")) => kind.to_owned(),
        _ => "bug".to_owned(),
    };
    let comments: Vec<Value> = ticket["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, message)| map_comment(message, index as u64 + 1))
        .collect();
    let created = summary["created_at"].as_str().unwrap_or("").to_owned();
    let mut events = vec![json!({"id": 1, "type": "created", "summary": "已提交", "created_at": created})];
    for (index, comment) in comments.iter().enumerate() {
        let staff = comment["is_staff"] == true;
        events.push(json!({
            "id": index + 2,
            "type": if staff { "staff_replied" } else { "user_replied" },
            "summary": if staff { "客服已回复" } else { "用户补充了反馈" },
            "payload": {"comment_id": comment["id"]},
            "created_at": comment["created_at"],
        }));
    }
    let mut detail = reorder_summary(summary);
    detail["type"] = json!(kind);
    detail["description"] = json!(read_string(&ticket["content"]["description"]).unwrap_or_default());
    detail["attachments"] = Value::Array(
        ticket["attachments"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, attachment)| map_attachment(attachment, index as u64 + 1))
            .collect(),
    );
    detail["comments"] = Value::Array(comments);
    detail["events"] = Value::Array(events);
    if let Some(severity @ ("P1-高" | "P2-中" | "P3-低")) = read_string(&ticket["content"]["severity"]).as_deref() {
        detail["severity"] = json!(severity);
    }
    detail["device"] = environment_to_device(&ticket["environment"]);
    detail
}

/// `os.type()`。
fn os_type() -> &'static str {
    match std::env::consts::OS {
        "macos" => "Darwin",
        "windows" => "Windows_NT",
        "linux" => "Linux",
        "freebsd" => "FreeBSD",
        _ => "Unknown",
    }
}

/// `os.release()`。
#[cfg(unix)]
fn os_release() -> String {
    #[allow(unsafe_code)]
    // SAFETY: uname 只写入调用方提供的结构体；失败时不读取其内容。
    let name = unsafe {
        let mut name: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut name) != 0 {
            return String::new();
        }
        name
    };
    let bytes: Vec<u8> = name.release.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8(bytes).unwrap_or_default()
}

#[cfg(not(unix))]
fn os_release() -> String {
    String::new()
}

/// `clientPlatform.resolveClientConfigPlatform`。
fn client_config_platform(platform: Option<&str>, arch: Option<&str>) -> String {
    let platform = platform.unwrap_or(node_platform());
    let arch = arch.unwrap_or(node_arch());
    let target = match platform {
        "darwin" => "darwin",
        "linux" => "linux",
        "win32" => "windows",
        other => other,
    };
    let arch = match arch {
        "x64" => "x86_64",
        "arm64" => "aarch64",
        other => other,
    };
    format!("{target}-{arch}")
}

#[derive(Clone)]
pub struct Feedback {
    api: ApiClient,
    credentials: CredentialStore,
    bridge: MainBridge,
    api_base: String,
    device_mid: Option<String>,
    app_config_dir: PathBuf,
    feedback_root: PathBuf,
    app_version: String,
    progress: EventHub<Value>,
    uploads: Arc<Mutex<HashMap<String, watch::Sender<bool>>>>,
    creates: Arc<Mutex<HashMap<String, watch::Sender<bool>>>>,
}

impl Feedback {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        api: ApiClient,
        credentials: CredentialStore,
        bridge: MainBridge,
        router: &mut ControlRouter,
        explicit_api_base: Option<&str>,
        device_mid: Option<String>,
        data_root: &Path,
        app_config_dir: PathBuf,
    ) -> Self {
        let api_base = explicit_api_base
            .map(str::trim)
            .filter(|base| !base.is_empty())
            .map(str::to_owned)
            .or_else(|| api.endpoints().env("ZCODE_FEEDBACK_API_BASE").map(str::to_owned))
            .unwrap_or_else(|| api.endpoints().zcode_api_url("/api/v1"));
        let app_version = api.endpoints().app_version();
        let feedback = Self {
            api,
            credentials,
            bridge: bridge.clone(),
            api_base: api_base.trim_end_matches('/').to_owned(),
            device_mid: device_mid.map(|mid| mid.trim().to_owned()).filter(|mid| !mid.is_empty()),
            app_config_dir,
            feedback_root: data_root.join("feedback"),
            app_version,
            progress: EventHub::new(),
            uploads: Arc::new(Mutex::new(HashMap::new())),
            creates: Arc::new(Mutex::new(HashMap::new())),
        };
        router.on("feedback-log-archive-result", move |message| {
            if let Some(request_id) = message.get("requestId").and_then(Value::as_str) {
                bridge.resolve(request_id, message.clone());
            }
        });
        feedback
    }

    fn device_snapshot(&self) -> Value {
        let release = os_release();
        json!({
            "appVersion": self.app_version,
            // Rust Host 没有 TS 的编译期常量：提交号取 Main 下发的 ZCODE_BUILD_COMMIT_ID，构建时间未知。
            "buildCommitId": self.api.endpoints().env("ZCODE_BUILD_COMMIT_ID").unwrap_or("unknown"),
            "buildTime": "unknown",
            "osType": os_type(),
            "osPlatform": node_platform(),
            "osRelease": release,
            "osVersion": release,
            "osArch": node_arch(),
        })
    }

    async fn jwt(&self) -> Option<String> {
        self.credentials
            .load(ZCODE_JWT_TOKEN_KEY)
            .await
            .ok()
            .flatten()
            .map(|jwt| jwt.trim().to_owned())
            .filter(|jwt| !jwt.is_empty())
    }

    async fn auth_headers(&self) -> Vec<(String, String)> {
        let mut headers = Vec::new();
        // device_mid 复用宿主 deviceMid（与 provider 请求头、远控同一身份）。
        if let Some(mid) = &self.device_mid {
            headers.push(("X-Device-Mid".to_owned(), mid.clone()));
        }
        if let Some(jwt) = self.jwt().await {
            headers.push(("Authorization".to_owned(), format!("Bearer {jwt}")));
        }
        headers
    }

    fn require_device_mid(&self) -> Result<String, RpcError> {
        self.device_mid.clone().ok_or_else(|| error("Missing feedback device_mid"))
    }

    /// 反馈后端请求：鉴权头 + 建连失败重试 3 次 + 信封解包。
    async fn request(&self, method: &str, path: &str, body: Option<Value>, extra: Vec<(String, String)>, timeout_ms: Option<u64>) -> Result<Value, RpcError> {
        let url = format!("{}{path}", self.api_base);
        let auth = self.auth_headers().await;
        let mut last_error = None;
        for attempt in 1..=3u64 {
            let mut request = ApiRequest::new(method, &url);
            for (name, value) in auth.iter().chain(extra.iter()) {
                request = request.header(name, value.clone());
            }
            if let Some(body) = &body {
                request = request.json(body);
            }
            if let Some(timeout) = timeout_ms {
                request = request.timeout_ms(timeout);
            }
            match self.api.request(request).await {
                Ok(response) => {
                    let status = response.status;
                    let ok = response.ok();
                    let text = response.text().await.map_err(|failure| error(failure.message))?;
                    if !ok {
                        let parsed = serde_json::from_str::<Value>(&text).ok();
                        let message = parsed
                            .as_ref()
                            .and_then(|json| read_string(&json["msg"]).or_else(|| json["detail"].as_str().map(str::to_owned)))
                            .unwrap_or_else(|| if text.is_empty() { format!("HTTP {status}") } else { text.clone() });
                        log::warn(format!("[feedback-http] 反馈 HTTP 请求失败 {method} status={status}: {message}"));
                        return Err(error(message));
                    }
                    if status == 204 {
                        return Ok(Value::Null);
                    }
                    let parsed: Value = serde_json::from_str(&text).map_err(|e| RpcError::new("SyntaxError", e.to_string()))?;
                    if parsed.is_object() && parsed.get("code").is_some() {
                        if parsed["code"].as_f64() != Some(0.0) {
                            return Err(error(
                                read_string(&parsed["msg"]).unwrap_or_else(|| format!("Feedback API error {}", parsed["code"])),
                            ));
                        }
                        return Ok(parsed["data"].clone());
                    }
                    return Ok(parsed);
                }
                Err(failure) => {
                    let retryable = failure.status.is_none()
                        && (failure.message.contains("connect") || failure.message.contains("handshake") || failure.message.contains("tls"));
                    last_error = Some(failure);
                    if attempt >= 3 || !retryable {
                        break;
                    }
                    log::warn(format!("[feedback-http] 反馈 HTTP 建连失败，准备重试 attempt={attempt}"));
                    tokio::time::sleep(Duration::from_millis(300 * attempt)).await;
                }
            }
        }
        Err(error(last_error.map(|e| e.message).unwrap_or_else(|| "fetch failed".into())))
    }

    fn environment(&self, input: &Value, device: &Value) -> Value {
        let mut environment = Map::new();
        let mut assign = |key: &str, value: Option<&Value>| {
            let Some(value) = value.filter(|v| !v.is_null() && v.as_str() != Some("")) else {
                return;
            };
            let value = match value.as_str() {
                Some(text) => json!(redact_feedback_text(text, false)),
                None => value.clone(),
            };
            environment.insert(key.to_owned(), value);
        };
        assign("app_version", device.get("appVersion"));
        let platform = json!(client_config_platform(device["osPlatform"].as_str(), device["osArch"].as_str()));
        assign("platform", Some(&platform));
        let channel = json!(self.api.endpoints().env("ZCODE_ENV").map(str::trim).filter(|v| !v.is_empty()).unwrap_or("stable"));
        assign("release_channel", Some(&channel));
        let category = device.get("osPlatform").filter(|v| !v.is_null()).cloned().unwrap_or(json!(node_platform()));
        assign("os_category", Some(&category));
        let os_version = device.get("osVersion").filter(|v| !v.is_null()).or(device.get("osRelease").filter(|v| !v.is_null())).cloned();
        assign("os_version", os_version.as_ref());
        for (key, source) in [
            ("build_commit_id", "buildCommitId"),
            ("build_time", "buildTime"),
            ("electron_version", "electronVersion"),
            ("node_version", "nodeVersion"),
            ("os_type", "osType"),
            ("os_platform", "osPlatform"),
            ("os_release", "osRelease"),
            ("os_arch", "osArch"),
            ("agent_provider", "agentProvider"),
        ] {
            assign(key, device.get(source));
        }
        let framework = device.get("agentFramework").filter(|v| !v.is_null()).or(input.get("framework"));
        assign("agent_framework", framework);
        assign("agent_model", device.get("agentModel"));
        assign("agent_model_display", device.get("agentModelDisplay"));
        assign("source", input.get("source"));
        Value::Object(environment)
    }

    pub async fn create(&self, input: &Value, operation_id: Option<String>) -> Result<Value, RpcError> {
        let device = input.get("device").filter(|d| !d.is_null()).cloned().unwrap_or_else(|| self.device_snapshot());
        let device_mid = self.require_device_mid()?;
        let (cancel, mut cancelled) = watch::channel(false);
        if let Some(id) = &operation_id {
            if let Some(previous) = self.creates.lock().unwrap_or_else(|p| p.into_inner()).insert(id.clone(), cancel.clone()) {
                let _ = previous.send(true);
            }
        }
        let mut headers = vec![("Content-Type".to_owned(), "application/json".to_owned())];
        if let Some(locale) = read_string(&input["locale"]) {
            headers.push(("Accept-Language".to_owned(), locale));
        }
        let mut content = Map::new();
        content.insert("description".into(), json!(redact_feedback_text(input["description"].as_str().unwrap_or(""), false)));
        for (key, source) in [("category", "type"), ("function", "module"), ("severity", "severity")] {
            if let Some(value) = input.get(source).filter(|v| !v.is_null()) {
                content.insert(key.into(), value.clone());
            }
        }
        let mut payload = Map::new();
        payload.insert("title".into(), json!(redact_feedback_text(input["title"].as_str().unwrap_or(""), false)));
        payload.insert("device_mid".into(), json!(device_mid));
        payload.insert("content".into(), Value::Object(content));
        if let Some(contact) = input.get("contact").filter(|v| !v.is_null()) {
            payload.insert("contact".into(), contact.clone());
        }
        payload.insert("environment".into(), self.environment(input, &device));
        let request = self.request("POST", "/feedback/ticket", Some(Value::Object(payload)), headers, Some(CREATE_TIMEOUT_MS));
        let result = tokio::select! {
            result = request => result,
            _ = cancelled.wait_for(|cancelled| *cancelled) => Err(error("This operation was aborted")),
        };
        if let Some(id) = &operation_id {
            let mut creates = self.creates.lock().unwrap_or_else(|p| p.into_inner());
            if creates.get(id).is_some_and(|current| current.same_channel(&cancel)) {
                creates.remove(id);
            }
        }
        let ticket = result?;
        let created = normalize_time(&ticket["created_at"], None);
        let mut detail = Map::new();
        detail.insert("id".into(), ticket["ticket_id"].clone());
        detail.insert("title".into(), input["title"].clone());
        detail.insert("type".into(), input["type"].clone());
        detail.insert("status".into(), json!(map_status(&ticket["status"])));
        detail.insert("created_at".into(), json!(created));
        detail.insert("updated_at".into(), json!(normalize_time(&ticket["updated_at"], Some(&created))));
        detail.insert("description".into(), input["description"].clone());
        detail.insert("attachments".into(), json!([]));
        detail.insert("comments".into(), json!([]));
        detail.insert("events".into(), json!([]));
        for key in ["severity", "module", "framework"] {
            if let Some(value) = input.get(key).filter(|v| !v.is_null() && v.as_str() != Some("")) {
                detail.insert(key.into(), value.clone());
            }
        }
        detail.insert("reporter".into(), input.get("reporter").cloned().unwrap_or(Value::Null));
        detail.insert("device".into(), device);
        let detail = Value::Object(detail);
        if self.jwt().await.is_none() {
            self.upsert_local(&device_mid, &detail).await?;
        }
        Ok(detail)
    }

    pub fn cancel_create(&self, operation_id: &str) {
        let key = operation_id.trim();
        if key.is_empty() {
            return;
        }
        if let Some(cancel) = self.creates.lock().unwrap_or_else(|p| p.into_inner()).get(key) {
            let _ = cancel.send(true);
        }
    }

    fn tickets_file(&self) -> PathBuf {
        self.feedback_root.join("tickets.json")
    }

    async fn read_local(&self) -> Vec<Value> {
        let Ok(text) = tokio::fs::read_to_string(self.tickets_file()).await else {
            return Vec::new();
        };
        let Ok(parsed) = serde_json::from_str::<Value>(&text) else {
            return Vec::new();
        };
        parsed["tickets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|ticket| ["deviceMid", "id", "title", "type", "status", "created_at", "updated_at"].iter().all(|key| ticket[*key].is_string()))
            .map(|ticket| {
                let mut ticket = ticket.clone();
                let status = ticket["status"].as_str().unwrap_or("").to_owned();
                if status == "待评估" || !FEEDBACK_STATUSES.contains(&status.as_str()) {
                    ticket["status"] = json!("已提交");
                }
                ticket
            })
            .collect()
    }

    async fn upsert_local(&self, device_mid: &str, ticket: &Value) -> Result<(), RpcError> {
        let mut tickets = self.read_local().await;
        let mut next = ticket.clone();
        next["deviceMid"] = json!(device_mid);
        tickets.retain(|current| current["deviceMid"] != device_mid || current["id"] != ticket["id"]);
        tickets.insert(0, next);
        tickets.truncate(200);
        let file = self.tickets_file();
        if let Some(parent) = file.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| error(e.to_string()))?;
        }
        let text = format!("{}\n", serde_json::to_string_pretty(&json!({"tickets": tickets})).unwrap_or_default());
        tokio::fs::write(&file, text).await.map_err(|e| error(e.to_string()))
    }

    pub async fn list(&self, query: &Value) -> Result<Value, RpcError> {
        let matches = |item: &Value| {
            (query["status"].is_null() || item["status"] == query["status"]) && (query["type"].is_null() || item["type"] == query["type"])
        };
        if self.jwt().await.is_some() {
            // 服务端支持 limit/offset；只在客户端 slice 会导致刷新列表总是拉全量。
            let params: Vec<String> = ["limit", "offset"]
                .iter()
                .filter_map(|key| query[*key].as_f64().filter(|v| *v >= 0.0).map(|v| format!("{key}={}", v as i64)))
                .collect();
            let path = if params.is_empty() {
                "/feedback/ticket".to_owned()
            } else {
                format!("/feedback/ticket?{}", params.join("&"))
            };
            let result = self.request("GET", &path, None, Vec::new(), None).await?;
            let items: Vec<Value> = result["items"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|item| reorder_summary(map_summary(item)))
                .filter(|item| matches(item))
                .collect();
            let total = items.len();
            return Ok(json!({"items": items, "total": total}));
        }
        let device_mid = self.require_device_mid()?;
        let mut tickets: Vec<Value> = self
            .read_local()
            .await
            .into_iter()
            .filter(|ticket| ticket["deviceMid"] == device_mid.as_str() && matches(ticket))
            .collect();
        tickets.sort_by(|a, b| b["updated_at"].as_str().unwrap_or("").cmp(a["updated_at"].as_str().unwrap_or("")));
        let offset = query["offset"].as_f64().map(|v| v.max(0.0) as usize).unwrap_or(0);
        let limit = query["limit"].as_f64().filter(|v| *v >= 0.0).map(|v| v as usize).unwrap_or(tickets.len());
        let items: Vec<Value> = tickets
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|mut ticket| {
                if let Some(object) = ticket.as_object_mut() {
                    object.shift_remove("deviceMid");
                }
                ticket
            })
            .collect();
        let total = items.len();
        Ok(json!({"items": items, "total": total}))
    }

    pub async fn get(&self, id: &str) -> Result<Value, RpcError> {
        let ticket = self
            .request("GET", &format!("/feedback/ticket/{}", encode_uri_component(id)), None, Vec::new(), None)
            .await?;
        Ok(map_detail(&ticket))
    }

    pub async fn comment(&self, id: &str, body: &str) -> Result<Value, RpcError> {
        let message = self
            .request(
                "POST",
                &format!("/feedback/ticket/{}/message", encode_uri_component(id)),
                Some(json!({"content": {"text": redact_feedback_text(body, false)}})),
                vec![("Content-Type".to_owned(), "application/json".to_owned())],
                None,
            )
            .await?;
        Ok(map_comment(&message, 1))
    }

    fn emit_progress(&self, id: &str, phase: &str, uploaded: u64, total: u64) {
        self.progress.emit(json!({"id": id, "phase": phase, "uploadedBytes": uploaded, "totalBytes": total}));
    }

    /// 上传一个本地文件：申请 OSS 凭证 → multipart 表单直传。
    async fn upload_file(
        &self,
        ticket_id: &str,
        kind: &str,
        path: &Path,
        filename: &str,
        content_type: &str,
        message_id: Option<&str>,
        progress: Option<(String, watch::Receiver<bool>)>,
    ) -> Result<Value, RpcError> {
        if progress.as_ref().is_some_and(|(_, cancel)| *cancel.borrow()) {
            return Err(error(UPLOAD_CANCELED));
        }
        let size = tokio::fs::metadata(path).await.map_err(|e| error(e.to_string()))?.len();
        let max = if kind == "log" { MAX_LOG_ATTACHMENT_BYTES } else { MAX_ATTACHMENT_BYTES };
        if size > max {
            return Err(error(format!("Feedback attachment exceeds max size {max}")));
        }
        let mut body = json!({"ticket_id": ticket_id});
        if let Some(message) = message_id.filter(|m| !m.is_empty()) {
            body["message_id"] = json!(message);
        }
        body["file_name"] = json!(filename);
        body["size"] = json!(size);
        let credential = self
            .request(
                "POST",
                "/feedback/attachment/upload-credential",
                Some(body),
                vec![("Content-Type".to_owned(), "application/json".to_owned())],
                None,
            )
            .await?;
        if let Some(max_size) = credential["max_size"].as_f64()
            && size as f64 > max_size
        {
            return Err(error(format!("Feedback attachment exceeds max size {max_size}")));
        }
        self.upload_oss(&credential, path, filename, content_type, size, progress).await?;
        Ok(json!({
            "id": stable_numeric_id(&credential["attachment_id"], 1),
            "kind": kind,
            "filename": filename,
            "size": size,
            "redacted": false,
            "content_type": content_type,
            "download_url": null,
            "preview_url": null,
            "created_at": iso_now(),
        }))
    }

    async fn upload_oss(
        &self,
        credential: &Value,
        path: &Path,
        filename: &str,
        content_type: &str,
        size: u64,
        progress: Option<(String, watch::Receiver<bool>)>,
    ) -> Result<(), RpcError> {
        use base64::Engine;
        let host = credential["oss"]["host"].as_str().unwrap_or("");
        let boundary = format!("----zcode-feedback-{}", uuid_v4());
        let escape = |value: &str| value.replace(['"', '\r', '\n'], "_");
        let callback = base64::engine::general_purpose::STANDARD.encode(
            json!({
                "callbackUrl": credential["callback"]["url"],
                "callbackBody": credential["callback"]["body"],
                "callbackBodyType": credential["callback"]["content_type"],
            })
            .to_string(),
        );
        let oss = &credential["oss"];
        let fields = [
            ("key", oss["path"].as_str().unwrap_or("").to_owned()),
            ("policy", oss["policy"].as_str().unwrap_or("").to_owned()),
            ("x-oss-signature", oss["x_oss_signature"].as_str().unwrap_or("").to_owned()),
            ("x-oss-signature-version", oss["x_oss_signature_version"].as_str().unwrap_or("").to_owned()),
            ("x-oss-credential", oss["x_oss_credential"].as_str().unwrap_or("").to_owned()),
            ("x-oss-security-token", oss["x_oss_security_token"].as_str().unwrap_or("").to_owned()),
            ("x-oss-date", oss["x_oss_date"].as_str().unwrap_or("").to_owned()),
            ("callback", callback),
            ("success_action_status", "200".to_owned()),
        ];
        let mut head = String::new();
        for (name, value) in fields.iter().filter(|(_, value)| !value.is_empty()) {
            head.push_str(&format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"\r\n\r\n{value}\r\n", escape(name)));
        }
        head.push_str(&format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{}\"\r\nContent-Type: {content_type}\r\n\r\n",
            escape(filename)
        ));
        let closing = format!("\r\n--{boundary}--\r\n");
        let length = head.len() as u64 + size + closing.len() as u64;
        let file = tokio::fs::File::open(path).await.map_err(|e| error(e.to_string()))?;
        let progress_id = progress.as_ref().map(|(id, _)| id.clone());
        let this = self.clone();
        let mut uploaded = 0u64;
        let file_stream = tokio_util_read_stream(file).map(move |chunk| {
            if let Ok(bytes) = &chunk {
                uploaded += bytes.len() as u64;
                if let Some(id) = &progress_id {
                    // 只按文件内容报告进度，不把表单头尾计入用户可见进度。
                    let current = uploaded.min(size);
                    this.emit_progress(id, if current >= size { "complete" } else { "uploading" }, current, size);
                }
            }
            chunk
        });
        let stream = futures_util::stream::iter([Ok::<_, std::io::Error>(head.into_bytes())])
            .chain(file_stream)
            .chain(futures_util::stream::iter([Ok(closing.into_bytes())]));
        // OSS 直传不经 Host 代理设置（Node 用 https.request 直连）。
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder().no_proxy().build().map_err(|e| error(e.to_string()))?;
        let send = client
            .post(host)
            .header("Content-Type", format!("multipart/form-data; boundary={boundary}"))
            .header("Content-Length", length.to_string())
            .body(reqwest::Body::wrap_stream(stream))
            .send();
        let response = match progress {
            Some((_, mut cancel)) => tokio::select! {
                response = send => response,
                _ = cancel.wait_for(|cancelled| *cancelled) => return Err(error(UPLOAD_CANCELED)),
            },
            None => send.await,
        }
        .map_err(|e| error(e.to_string()))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let body = response.text().await.unwrap_or_default();
            return Err(error(format!(
                "OSS upload failed: HTTP {status}{}",
                if body.is_empty() { String::new() } else { format!(": {body}") }
            )));
        }
        Ok(())
    }

    fn default_content_type(kind: &str) -> &'static str {
        if kind == "image" { "image/png" } else { "application/zip" }
    }

    pub async fn upload_attachment(&self, id: &str, kind: &str, file: &Value, progress_id: Option<String>) -> Result<Value, RpcError> {
        let path = PathBuf::from(file["path"].as_str().unwrap_or(""));
        let filename = file["filename"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        let content_type = file["contentType"].as_str().map(str::to_owned).unwrap_or_else(|| Self::default_content_type(kind).to_owned());
        let message_id = file["messageId"].as_str();
        let Some(progress_id) = progress_id else {
            return self.upload_file(id, kind, &path, &filename, &content_type, message_id, None).await;
        };
        self.emit_progress(&progress_id, "preparing", 0, 0);
        let (cancel, cancelled) = watch::channel(false);
        self.uploads.lock().unwrap_or_else(|p| p.into_inner()).insert(progress_id.clone(), cancel);
        let result = self
            .upload_file(id, kind, &path, &filename, &content_type, message_id, Some((progress_id.clone(), cancelled)))
            .await;
        self.uploads.lock().unwrap_or_else(|p| p.into_inner()).remove(&progress_id);
        if let Err(failure) = &result
            && failure.message == UPLOAD_CANCELED
        {
            self.emit_progress(&progress_id, "canceled", 0, 0);
        }
        result
    }

    pub fn cancel_upload(&self, progress_id: &str) {
        if let Some(cancel) = self.uploads.lock().unwrap_or_else(|p| p.into_inner()).get(progress_id) {
            let _ = cancel.send(true);
        }
    }

    pub async fn upload_attachment_data(&self, id: &str, kind: &str, file: &Value) -> Result<Value, RpcError> {
        use base64::Engine;
        let root = self.feedback_root.join("attachments");
        tokio::fs::create_dir_all(&root).await.map_err(|e| error(e.to_string()))?;
        let temp_dir = root.join(format!("attachment-{}", &uuid_v4()[..8]));
        tokio::fs::create_dir_all(&temp_dir).await.map_err(|e| error(e.to_string()))?;
        // 附件名来自 renderer，不能作为本地相对路径使用。
        let raw_name = file["filename"].as_str().unwrap_or("").replace('\\', "/");
        let filename = raw_name.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("attachment").to_owned();
        let temp_path = temp_dir.join("content");
        let result = async {
            let data = base64::engine::general_purpose::STANDARD
                .decode(file["dataBase64"].as_str().unwrap_or("").trim())
                .unwrap_or_default();
            tokio::fs::write(&temp_path, data).await.map_err(|e| error(e.to_string()))?;
            self.upload_file(
                id,
                kind,
                &temp_path,
                &filename,
                file["contentType"].as_str().unwrap_or(""),
                file["messageId"].as_str(),
                None,
            )
            .await
        }
        .await;
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
        result
    }

    /// 完整日志归档委托 Main（与“导出日志”同源）；否则只打当天的诊断日志（上限 2 MiB）。
    pub async fn prepare_archive(&self, full: bool, progress_id: Option<String>) -> Result<Value, RpcError> {
        if let Some(id) = &progress_id {
            self.emit_progress(id, "preparing", 0, 0);
        }
        if full {
            let request_id = format!("feedback-log-archive-{}-{}", super::oauth_adapters::now_ms(), &uuid_v4()[..8]);
            let message = json!({
                "type": "feedback-log-archive-request",
                "requestId": request_id,
                "sourceDir": self.app_config_dir.display().to_string(),
            });
            // Node 没有超时；这里给一个很宽的上限，避免 Main 丢消息时永久挂起。
            let result = self
                .bridge
                .request(request_id.clone(), message, Duration::from_secs(30 * 60), |_| {})
                .await;
            let result = result.ok_or_else(|| error("Feedback log archive timed out"))?;
            if result["ok"] != true {
                return Err(error(result["error"].as_str().unwrap_or("Feedback log archive failed")));
            }
            return Ok(json!({"path": result["path"], "size": result["size"]}));
        }
        let archive = create_diagnostic_archive(
            &self.app_config_dir.join("logs"),
            &self.feedback_root.join("logs"),
            2 * 1024 * 1024,
            &self.app_version,
        )
        .await?;
        if let Some(id) = &progress_id {
            let size = archive["size"].as_u64().unwrap_or(0);
            self.emit_progress(id, "preparing", size, size);
        }
        Ok(archive)
    }

    pub async fn cleanup_archive(&self, path: &str) {
        if let Some(parent) = Path::new(path).parent() {
            let _ = tokio::fs::remove_dir_all(parent).await;
        }
    }

    pub async fn attach_logs(&self, id: &str, full: bool) -> Result<Value, RpcError> {
        let archive = self.prepare_archive(full, None).await?;
        let path = archive["path"].as_str().unwrap_or("").to_owned();
        let filename = Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let result = self.upload_file(id, "log", Path::new(&path), &filename, "application/zip", None, None).await;
        self.cleanup_archive(&path).await;
        result
    }
}

/// 把文件读成字节流（每块 64 KiB）。
fn tokio_util_read_stream(
    file: tokio::fs::File,
) -> impl futures_util::Stream<Item = Result<Vec<u8>, std::io::Error>> + Send + 'static {
    use tokio::io::AsyncReadExt;
    futures_util::stream::unfold(Some(file), |file| async move {
        let mut file = file?;
        let mut buffer = vec![0u8; 64 * 1024];
        match file.read(&mut buffer).await {
            Ok(0) => None,
            Ok(read) => {
                buffer.truncate(read);
                Some((Ok(buffer), Some(file)))
            }
            Err(error) => Some((Err(error), None)),
        }
    })
}

fn decode_log(bytes: &[u8]) -> Option<String> {
    let text = if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        let little = bytes[0] == 0xff;
        let units: Vec<u16> = bytes[2..]
            .chunks(2)
            .map(|pair| if pair.len() == 2 { if little { u16::from_le_bytes([pair[0], pair[1]]) } else { u16::from_be_bytes([pair[0], pair[1]]) } } else { 0xFFFF })
            .collect();
        String::from_utf16(&units).ok()?
    } else {
        String::from_utf8(bytes.to_vec()).ok()?
    };
    let text = text.strip_prefix('\u{feff}').map(str::to_owned).unwrap_or(text);
    if text.chars().any(|c| (c as u32) < 32 && !matches!(c, '\t' | '\n' | '\r')) {
        return None;
    }
    Some(text)
}

/// `createFeedbackDiagnosticArchive`：当天修改的诊断文本日志，脱敏后打 zip（0600）。
async fn create_diagnostic_archive(source: &Path, output_root: &Path, max_total: u64, app_version: &str) -> Result<Value, RpcError> {
    let source = source.to_path_buf();
    let output_root = output_root.to_path_buf();
    let app_version = app_version.to_owned();
    tokio::task::spawn_blocking(move || -> Result<Value, String> {
        use std::io::Write;
        let budget = max_total.min(MAX_LOG_TOTAL_BYTES);
        std::fs::create_dir_all(&output_root).map_err(|e| e.to_string())?;
        let output_dir = output_root.join(format!("archive-{}", &uuid_v4()[..8]));
        std::fs::create_dir_all(&output_dir).map_err(|e| e.to_string())?;
        let path = output_dir.join("zcode-diagnostic-logs.zip");
        let today = chrono::Local::now().date_naive();
        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
        let mut skipped: std::collections::BTreeMap<&'static str, u64> = std::collections::BTreeMap::new();
        let mut total = 0u64;
        let mut visited = 0usize;
        let log_name = regex::Regex::new(r"(?i)(?:\.log(?:\.\d+)?|\.jsonl|\.ndjson)$").expect("static regex");
        let root_ok = std::fs::symlink_metadata(&source).map(|m| m.is_dir()).unwrap_or(false);
        let mut stack = vec![(source.clone(), "logs".to_owned(), 0usize)];
        while let Some((directory, prefix, depth)) = stack.pop() {
            if !root_ok || depth > 4 || visited >= 2000 {
                continue;
            }
            let mut names: Vec<_> = std::fs::read_dir(&directory).map(|d| d.flatten().collect()).unwrap_or_default();
            names.sort_by_key(|entry: &std::fs::DirEntry| entry.file_name());
            let mut subdirectories = Vec::new();
            for entry in names {
                visited += 1;
                if visited > 2000 {
                    break;
                }
                let Ok(kind) = entry.file_type() else { continue };
                if kind.is_symlink() {
                    continue;
                }
                let name = format!("{prefix}/{}", entry.file_name().to_string_lossy());
                if kind.is_dir() {
                    subdirectories.push((entry.path(), name, depth + 1));
                    continue;
                }
                if !kind.is_file() || !log_name.is_match(&entry.file_name().to_string_lossy()) {
                    continue;
                }
                let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
                    *skipped.entry("metadata-policy").or_default() += 1;
                    continue;
                };
                let modified_today = metadata
                    .modified()
                    .ok()
                    .map(|time| chrono::DateTime::<chrono::Local>::from(time).date_naive() == today)
                    .unwrap_or(false);
                if metadata.len() > MAX_LOG_FILE_BYTES || total + metadata.len() > budget || !modified_today {
                    *skipped.entry("metadata-policy").or_default() += 1;
                    continue;
                }
                let Ok(bytes) = std::fs::read(entry.path()) else {
                    *skipped.entry("open-failed").or_default() += 1;
                    continue;
                };
                let Some(text) = decode_log(&bytes) else {
                    *skipped.entry("unsupported-text").or_default() += 1;
                    continue;
                };
                let data = redact_feedback_text(&text, true).into_bytes();
                if data.len() as u64 > MAX_LOG_FILE_BYTES || total + data.len() as u64 > budget {
                    *skipped.entry("redacted-size-limit").or_default() += 1;
                    continue;
                }
                total += (bytes.len().max(data.len())) as u64;
                entries.push((name, data));
            }
            // 深度优先、名称有序（与 TS 递归遍历顺序一致）。
            for item in subdirectories.into_iter().rev() {
                stack.push(item);
            }
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path).map_err(|e| e.to_string())?;
        let mut zip = zip::ZipWriter::new(file);
        let file_options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in &entries {
            zip.start_file(name.as_str(), file_options).map_err(|e| e.to_string())?;
            zip.write_all(data).map_err(|e| e.to_string())?;
        }
        let skipped_total: u64 = skipped.values().sum();
        let about = [
            "ZCode diagnostic logs".to_owned(),
            format!("timestamp: {}", iso_now()),
            format!("appVersion: {app_version}"),
            "commit: unknown".to_owned(),
            "node: (rust host)".to_owned(),
            format!("os: {} {} ({})", node_platform(), os_release(), node_arch()),
            format!("includedLogFiles: {}", entries.len()),
            format!("skippedLogFiles: {skipped_total}"),
            format!("skippedLogFilesByReason: {}", serde_json::to_string(&skipped).unwrap_or_default()),
            "Scope: diagnostic text log files modified today (local time); credentials and structured payloads redacted.".to_owned(),
        ]
        .join("\n");
        zip.start_file("about.txt", file_options).map_err(|e| e.to_string())?;
        zip.write_all(about.as_bytes()).map_err(|e| e.to_string())?;
        zip.finish().map_err(|e| e.to_string())?;
        let size = std::fs::metadata(&path).map_err(|e| e.to_string())?.len();
        Ok(json!({"path": path.display().to_string(), "size": size}))
    })
    .await
    .map_err(|e| error(e.to_string()))?
    .map_err(error)
}

pub fn service(feedback: Feedback) -> ServiceBuilder {
    macro_rules! handler {
        (|$f:ident, $args:ident| $body:expr) => {{
            let shared = feedback.clone();
            move |_: zcode_host_kit::CallContext, $args: zcode_host_kit::Args| {
                let $f = shared.clone();
                async move { $body }
            }
        }};
    }
    let events = feedback.progress.clone();
    ServiceBuilder::new("feedback")
        .method("create", handler!(|f, args| {
            let input: Value = args.get(0, "input")?;
            let options: Option<Value> = args.opt(1, "options")?;
            let operation = options.as_ref().and_then(|o| read_string(&o["operationId"]));
            f.create(&input, operation).await.map(Reply::Value)
        }))
        .method("cancelCreate", handler!(|f, args| {
            let id: String = args.get(0, "operationId")?;
            f.cancel_create(&id);
            Ok(Reply::Undefined)
        }))
        .method("list", handler!(|f, args| {
            let query: Value = args.opt(0, "query")?.unwrap_or_else(|| json!({}));
            f.list(&query).await.map(Reply::Value)
        }))
        .method("get", handler!(|f, args| {
            let id: String = args.get(0, "id")?;
            f.get(&id).await.map(Reply::Value)
        }))
        .method("comment", handler!(|f, args| {
            let id: String = args.get(0, "id")?;
            let body: String = args.get(1, "body")?;
            f.comment(&id, &body).await.map(Reply::Value)
        }))
        .method("uploadAttachment", handler!(|f, args| {
            let id: String = args.get(0, "id")?;
            let kind: String = args.get(1, "kind")?;
            let file: Value = args.get(2, "file")?;
            f.upload_attachment(&id, &kind, &file, None).await.map(Reply::Value)
        }))
        .method("uploadAttachmentWithProgress", handler!(|f, args| {
            let id: String = args.get(0, "id")?;
            let kind: String = args.get(1, "kind")?;
            let file: Value = args.get(2, "file")?;
            let progress: String = args.get(3, "progressId")?;
            f.upload_attachment(&id, &kind, &file, Some(progress)).await.map(Reply::Value)
        }))
        .method("cancelUpload", handler!(|f, args| {
            let id: String = args.get(0, "progressId")?;
            f.cancel_upload(&id);
            Ok(Reply::Undefined)
        }))
        .method("uploadAttachmentData", handler!(|f, args| {
            let id: String = args.get(0, "id")?;
            let kind: String = args.get(1, "kind")?;
            let file: Value = args.get(2, "file")?;
            f.upload_attachment_data(&id, &kind, &file).await.map(Reply::Value)
        }))
        .method("attachLogsFromExport", handler!(|f, args| {
            let id: String = args.get(0, "id")?;
            let options: Value = args.opt(1, "options")?.unwrap_or_else(|| json!({}));
            f.attach_logs(&id, options["full"] == true).await.map(Reply::Value)
        }))
        .method("getDeviceSnapshot", handler!(|f, _args| Ok(Reply::Value(f.device_snapshot()))))
        .method("prepareCompactLogArchive", handler!(|f, args| {
            let options: Value = args.opt(0, "options")?.unwrap_or_else(|| json!({}));
            f.prepare_archive(options["full"] == true, read_string(&options["progressId"])).await.map(Reply::Value)
        }))
        .method("cleanupPreparedLogArchive", handler!(|f, args| {
            let path: String = args.get(0, "path")?;
            f.cleanup_archive(&path).await;
            Ok(Reply::Undefined)
        }))
        .method("revealLogArchive", |_, _| async { Ok(Reply::Undefined) })
        .event("onDynamicUploadProgress", move |_, arg| {
            let id = arg.as_str().unwrap_or("").to_owned();
            Ok(replies(events.subscribe_filtered(move |event: &Value| event["id"] == id.as_str())))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_rules_match_node() {
        assert_eq!(normalize_time(&json!(0), None), "1970-01-01T00:00:00.000Z");
        assert_eq!(normalize_time(&json!("10"), None), "1970-01-01T00:00:10.000Z");
        assert_eq!(normalize_time(&json!("2026-01-01T00:00:00"), None), "2026-01-01T00:00:00Z");
        assert_eq!(normalize_time(&json!("2026-01-01T00:00:00+08:00"), None), "2026-01-01T00:00:00+08:00");
        assert_eq!(stable_numeric_id(&json!("ab"), 7), json!(97 * 31 + 98));
        assert_eq!(stable_numeric_id(&json!(5), 7), json!(5));
        assert_eq!(map_status(&json!("closed")), "已归档");
        assert_eq!(infer_attachment_kind("logs.ZIP", None), "log");
        assert_eq!(client_config_platform(Some("win32"), Some("x64")), "windows-x86_64");
        let detail = map_detail(&json!({"ticket_id": "t1", "title": " T ", "status": "submitted", "created_at": 1,
            "content": {"category": "feature", "severity": "P2-中", "description": "d"},
            "messages": [{"message_id": "m1", "sender_type": "staff", "content": {"text": "hi"}, "created_at": 2}],
            "environment": {"os_category": "darwin", "app_version": "1.0"}}));
        assert_eq!(detail["type"], "feature");
        assert_eq!(detail["title"], "T");
        assert_eq!(detail["events"][1]["type"], "staff_replied");
        assert_eq!(detail["device"], json!({"appVersion": "1.0", "osPlatform": "darwin"}));
    }

    struct ChannelSink(tokio::sync::mpsc::UnboundedSender<Value>);

    impl zcode_host_control::ControlSink for ChannelSink {
        fn send(&self, message: zcode_host_control::HostToMain) {
            if let zcode_host_control::HostToMain::Raw(value) = message {
                let _ = self.0.send(value);
            }
        }
    }

    #[tokio::test]
    async fn tickets_uploads_and_full_archive_follow_node() {
        use super::super::net::testing::serve;
        use super::super::net::{ApiClient, NetworkOptions};
        use super::super::oauth_tests::endpoints_for;
        let oss_origin = Arc::new(Mutex::new(String::new()));
        let oss = oss_origin.clone();
        let server = serve(Arc::new(move |request| {
            let path = request.target.split('?').next().unwrap_or("");
            let body = match (request.method.as_str(), path) {
                ("POST", "/api/v1/feedback/ticket") => json!({"code": 0, "data": {"ticket_id": "t1", "status": "submitted", "created_at": 1_700_000_000}}),
                ("GET", "/api/v1/feedback/ticket") => json!({"code": 0, "data": {"items": [{"ticket_id": "t2", "title": "Remote", "status": "closed", "created_at": "2026-01-01T00:00:00"}]}}),
                ("POST", "/api/v1/feedback/attachment/upload-credential") => json!({"code": 0, "data": {
                    "attachment_id": "a1", "max_size": 1024,
                    "oss": {"host": format!("{}/oss", oss.lock().unwrap()), "path": "fb/a1", "policy": "p", "x_oss_signature": "s"},
                    "callback": {"url": "https://cb", "body": "b", "content_type": "application/json"},
                }}),
                ("POST", "/oss") => json!({"ok": true}),
                _ => return (404, json!({"code": 404, "msg": "missing"}).to_string()),
            };
            (200, body.to_string())
        }))
        .await;
        *oss_origin.lock().unwrap() = server.origin.clone();
        let dir = tempfile::tempdir().unwrap();
        let credentials = CredentialStore::new(dir.path(), "secret");
        let api = ApiClient::with_network(endpoints_for(&server.origin), dir.path().to_path_buf(), NetworkOptions::default());
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let bridge = MainBridge::new(Arc::new(ChannelSink(sender)));
        let mut router = ControlRouter::default();
        let feedback = Feedback::new(api.clone(), credentials.clone(), bridge.clone(), &mut router, None, Some("mid-1".into()), dir.path(), dir.path().join("v2"));

        // 未登录：创建走匿名设备身份，结果进本地索引。
        let created = feedback
            .create(&json!({"title": "token=abc", "description": "d", "type": "bug", "device": {"osPlatform": "darwin", "osArch": "arm64"}}), Some("op".into()))
            .await
            .unwrap();
        assert_eq!(created["id"], "t1");
        assert_eq!(created["status"], "已提交");
        assert_eq!(created["created_at"], "2023-11-14T22:13:20.000Z");
        let sent = &server.requests()[0];
        assert_eq!(sent.header("X-Device-Mid"), Some("mid-1"));
        assert_eq!(sent.header("Authorization"), None);
        let payload: Value = serde_json::from_str(&sent.body).unwrap();
        assert_eq!(payload["title"], "token=[REDACTED]");
        assert_eq!(payload["device_mid"], "mid-1");
        assert_eq!(payload["environment"]["platform"], "darwin-aarch64");
        let local = feedback.list(&json!({})).await.unwrap();
        assert_eq!(local["total"], 1);
        assert_eq!(local["items"][0]["id"], "t1");
        assert!(local["items"][0].get("deviceMid").is_none());

        // 已登录：列表走服务端并带 Bearer。
        credentials.save(ZCODE_JWT_TOKEN_KEY, "jwt-1").await.unwrap();
        let remote = feedback.list(&json!({"limit": 5})).await.unwrap();
        assert_eq!(remote["items"][0]["status"], "已归档");
        assert_eq!(remote["items"][0]["created_at"], "2026-01-01T00:00:00Z");
        let listed = server.requests().into_iter().find(|r| r.method == "GET").unwrap();
        assert_eq!(listed.target, "/api/v1/feedback/ticket?limit=5");
        assert_eq!(listed.header("Authorization"), Some("Bearer jwt-1"));

        // 附件：凭证 → OSS 表单直传，进度事件按 progressId 过滤。
        let file = dir.path().join("shot.png");
        std::fs::write(&file, b"PNGDATA").unwrap();
        let mut progress = Box::pin(feedback.progress.subscribe_filtered(|event: &Value| event["id"] == "p1"));
        let attachment = feedback
            .upload_attachment("t1", "image", &json!({"path": file.display().to_string()}), Some("p1".into()))
            .await
            .unwrap();
        assert_eq!(attachment["filename"], "shot.png");
        assert_eq!(attachment["content_type"], "image/png");
        assert_eq!(attachment["size"], 7);
        let upload = server.requests().into_iter().find(|r| r.target == "/oss").unwrap();
        assert!(upload.header("content-type").unwrap().starts_with("multipart/form-data; boundary="));
        assert!(upload.body.contains("name=\"key\"\r\n\r\nfb/a1\r\n"));
        assert!(upload.body.contains("filename=\"shot.png\"\r\nContent-Type: image/png\r\n\r\nPNGDATA\r\n"));
        assert!(!upload.body.contains("x-oss-date"));
        assert_eq!(progress.next().await.unwrap()["phase"], "preparing");
        assert_eq!(progress.next().await.unwrap()["phase"], "complete");

        // 完整日志归档委托 Main。
        let archive = tokio::spawn({
            let feedback = feedback.clone();
            async move { feedback.prepare_archive(true, None).await }
        });
        let request = requests.recv().await.unwrap();
        assert_eq!(request["type"], "feedback-log-archive-request");
        assert_eq!(request["sourceDir"], dir.path().join("v2").display().to_string());
        let request_id = request["requestId"].as_str().unwrap().to_owned();
        assert!(router.dispatch(
            "feedback-log-archive-result",
            json!({"type": "feedback-log-archive-result", "requestId": request_id, "ok": true, "path": "/tmp/x/full.zip", "size": 9}),
        ));
        assert_eq!(archive.await.unwrap().unwrap(), json!({"path": "/tmp/x/full.zip", "size": 9}));

        // 没有宿主 deviceMid 不能匿名建单。
        let anonymous = Feedback::new(api, credentials, bridge, &mut ControlRouter::default(), None, None, dir.path(), dir.path().join("v2"));
        assert_eq!(anonymous.create(&json!({"title": "t"}), None).await.unwrap_err().message, "Missing feedback device_mid");
    }

    #[tokio::test]
    async fn compact_archive_includes_todays_redacted_logs_only() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(logs.join("nested")).unwrap();
        std::fs::write(logs.join("main.log"), "token=abc\nfine line\n").unwrap();
        std::fs::write(logs.join("nested/agent.jsonl"), "{\"apiKey\":\"x\"}\n").unwrap();
        std::fs::write(logs.join("notes.txt"), "ignored").unwrap();
        let archive = create_diagnostic_archive(&logs, &dir.path().join("out"), 2 * 1024 * 1024, "1.0").await.unwrap();
        let file = std::fs::File::open(archive["path"].as_str().unwrap()).unwrap();
        let mut zip = zip::ZipArchive::new(file).unwrap();
        let names: Vec<String> = (0..zip.len()).map(|i| zip.by_index(i).unwrap().name().to_owned()).collect();
        assert_eq!(names, ["logs/main.log", "logs/nested/agent.jsonl", "about.txt"]);
        let mut content = String::new();
        std::io::Read::read_to_string(&mut zip.by_name("logs/main.log").unwrap(), &mut content).unwrap();
        assert_eq!(content, "[REDACTED]\nfine line\n");
    }
}
