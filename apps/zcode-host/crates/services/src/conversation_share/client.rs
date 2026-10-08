//! 分享服务端客户端（conversationShareHttpClient.ts）：统一的鉴权、错误归一化与成功信封解析。
//! Rust Host 还没有账号（R4），token provider 恒为空：`required` 端点在发请求前就以
//! `authentication_required` 失败，与 Node 未登录时相同；`optional` 端点（预览、续接）照常请求。
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use zcode_host_kit::log;

use super::errors::ClientError;
use super::integrity::verify;
use crate::agent::service::uuid_v4;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// 本端产出与理解的分享载荷版本。
pub const SCHEMA_VERSION: i64 = 1;
const KNOWN_ROW_KINDS: [&str; 9] = [
    "turnHeader",
    "userInput",
    "assistantText",
    "reasoning",
    "toolCall",
    "subagent",
    "hookInvocation",
    "timelineMarker",
    "artifact",
];
const ARTIFACT_TYPES: [&str; 8] = ["pdf", "pptx", "docx", "xlsx", "image", "html", "md", "text"];
const ACCESS_MODES: [&str; 3] = ["private", "public_readonly", "public_importable"];

pub type TokenProvider = Arc<dyn Fn() -> BoxFuture<'static, Option<String>> + Send + Sync>;

/// `ERROR_KIND_BY_CODE`。
fn error_kind(code: i64) -> Option<&'static str> {
    Some(match code {
        3001 | 3203 | 3204 => "invalid_contract",
        3002 => "rate_limited",
        3200 => "feature_disabled",
        3201 | 3213 => "authentication_required",
        3205 => "invalid_conversation",
        3206 => "disclosure_required",
        3207 => "unsafe_structure",
        3208 => "artifact_not_allowed",
        3209 => "limit_exceeded",
        3210 => "upload_incomplete",
        3211 => "not_found",
        3212 => "expired",
        3214 => "import_not_allowed",
        3215 => "safety_check_pending",
        _ => return None,
    })
}

fn normalize_request_id(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    (!value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-')))
    .then(|| value.to_owned())
}

/// `parseRetryAfterMs`：只认整数秒（HTTP-date 形式由 httpdate 规则严格解析）。
fn retry_after_ms(value: Option<&str>) -> Option<i64> {
    let value = value?.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        let ms = value.parse::<i64>().ok()?.checked_mul(1000)?;
        return (ms > 0).then_some(ms);
    }
    None
}

/// `decodeConversationShareRows`：认得的 row kind 保留，其余计数跳过。
pub fn decode_rows(rows: &[Value]) -> (Vec<Value>, usize, Vec<String>) {
    let mut decoded = Vec::new();
    let mut unsupported = 0;
    let mut kinds: Vec<String> = Vec::new();
    for row in rows {
        let kind = row["kind"].as_str().map(str::trim).unwrap_or_default();
        if row.is_object() && KNOWN_ROW_KINDS.contains(&kind) {
            decoded.push(row.clone());
            continue;
        }
        unsupported += 1;
        let label = if kind.is_empty() { "unknown" } else { kind };
        if !kinds.iter().any(|existing| existing == label) {
            kinds.push(label.to_owned());
        }
    }
    (decoded, unsupported, kinds)
}

fn invalid_contract(message: &str) -> ClientError {
    ClientError::new("invalid_contract", message)
}

fn require_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, ClientError> {
    value[key]
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| {
            invalid_contract("Conversation share API response does not match the client contract")
        })
}

fn require_int(value: &Value, key: &str) -> Result<i64, ClientError> {
    value[key].as_i64().ok_or_else(|| {
        invalid_contract("Conversation share API response does not match the client contract")
    })
}

/// 公开元数据 + 版本 + 完整性字段的形状校验（preview / continuation 共用）。
fn check_public_payload(data: &Value, continuation: bool) -> Result<i64, ClientError> {
    let version = require_int(data, "schema_version")?;
    let share = &data["share"];
    if !share["title"].is_string()
        || !share["access_mode"]
            .as_str()
            .is_some_and(|mode| ACCESS_MODES.contains(&mode))
    {
        return Err(invalid_contract(
            "Conversation share API response does not match the client contract",
        ));
    }
    require_int(share, "created_at")?;
    require_int(share, "expires_at")?;
    if continuation {
        require_str(data, "import_grant_id")?;
        require_int(data, "import_grant_expires_at")?;
        require_str(share, "share_id")?;
    }
    if !data["rows"].is_array() || !data["artifacts"].is_array() {
        return Err(invalid_contract(
            "Conversation share API response does not match the client contract",
        ));
    }
    let sha = |key: &str| {
        data["integrity"][key].as_str().is_some_and(|hash| {
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        })
    };
    if !sha("projection_sha256") || !sha("artifact_set_sha256") {
        return Err(invalid_contract(
            "Conversation share API response does not match the client contract",
        ));
    }
    for artifact in data["artifacts"].as_array().into_iter().flatten() {
        for key in [
            "artifact_id",
            "logical_artifact_key",
            "producer_product_turn_id",
            "display_name",
            "extension",
            "mime_type",
        ] {
            require_str(artifact, key)?;
        }
        require_int(artifact, "size_bytes")?;
        if !artifact["artifact_type"]
            .as_str()
            .is_some_and(|kind| ARTIFACT_TYPES.contains(&kind))
        {
            return Err(invalid_contract(
                "Conversation share API response does not match the client contract",
            ));
        }
        let url_key = if continuation { "download_url" } else { "url" };
        require_str(artifact, url_key)?;
    }
    Ok(version)
}

pub struct ShareClient {
    base_url: String,
    token: TokenProvider,
    http: reqwest::Client,
}

impl ShareClient {
    pub fn new(base_url: String, token: TokenProvider) -> anyhow::Result<Self> {
        Ok(Self {
            base_url,
            token,
            http: crate::support::http::client(Duration::from_secs(120))?,
        })
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    /// `requestData`：鉴权、错误归一化、`{code:0, msg, data}` 信封。
    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        auth_required: bool,
    ) -> Result<Value, ClientError> {
        let token = (self.token)()
            .await
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty());
        if auth_required && token.is_none() {
            return Err(ClientError::new(
                "authentication_required",
                "Conversation share authentication required",
            )
            .status(401));
        }
        let request_id = uuid_v4();
        let mut request = self
            .http
            .request(method.clone(), self.url(path))
            .timeout(DEFAULT_TIMEOUT)
            .header("x-request-id", &request_id);
        if let Some(token) = &token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        if let Some(body) = body {
            request = request
                .header("Content-Type", "application/json")
                .body(body.to_string());
        }
        let response = request
            .send()
            .await
            .map_err(|error| ClientError::new("network", error.to_string()))?;
        let status = response.status().as_u16();
        let response_request_id = normalize_request_id(
            response
                .headers()
                .get("x-request-id")
                .and_then(|value| value.to_str().ok()),
        );
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let text = response
            .text()
            .await
            .map_err(|error| ClientError::new("network", error.to_string()))?;
        if !(200..300).contains(&status) {
            if status == 401 && text.trim().is_empty() {
                return Err(ClientError::new(
                    "authentication_required",
                    "Conversation share authentication required",
                )
                .status(401)
                .request_id(response_request_id));
            }
            let parsed = if text.trim().is_empty() {
                None
            } else {
                match serde_json::from_str::<Value>(&text) {
                    Ok(value) => Some(value),
                    Err(_) if status >= 500 => {
                        return Err(ClientError::new(
                            "network",
                            format!("Conversation share API upstream failed with HTTP {status}"),
                        )
                        .status(status)
                        .request_id(response_request_id));
                    }
                    Err(_) => {
                        return Err(ClientError::new(
                            "invalid_contract",
                            "Conversation share API returned invalid JSON",
                        )
                        .status(status)
                        .request_id(response_request_id));
                    }
                }
            };
            if let Some(envelope) = parsed
                .as_ref()
                .filter(|value| value["code"].is_i64() && value["msg"].is_string())
            {
                let code = envelope["code"].as_i64().unwrap_or_default();
                let kind = error_kind(code);
                log::warn(format!(
                    "conversation share API request rejected path={path} method={method} status={status} kind={} code={code}",
                    kind.unwrap_or("unknown")
                ));
                let mut error = ClientError::new(
                    kind.unwrap_or("unknown"),
                    envelope["msg"].as_str().unwrap_or_default(),
                )
                .status(status)
                .request_id(response_request_id);
                if kind.is_some() {
                    error.code = Some(code);
                }
                if code == 3215 {
                    error.retry_after_ms = retry_after_ms(retry_after.as_deref());
                }
                return Err(error);
            }
            let kind = if status >= 500 {
                "network"
            } else if status == 429 {
                "rate_limited"
            } else {
                "unknown"
            };
            return Err(ClientError::new(
                kind,
                format!("Conversation share API failed with HTTP {status}"),
            )
            .status(status)
            .request_id(response_request_id));
        }
        let envelope: Value = serde_json::from_str(&text).map_err(|_| {
            ClientError::new(
                "invalid_contract",
                "Conversation share API response does not match the client contract",
            )
            .status(status)
            .request_id(response_request_id.clone())
        })?;
        if envelope["code"] != 0 || !envelope["msg"].is_string() || envelope.get("data").is_none() {
            return Err(ClientError::new(
                "invalid_contract",
                "Conversation share API response does not match the client contract",
            )
            .status(status)
            .request_id(response_request_id));
        }
        Ok(envelope["data"].clone())
    }

    /// `getCapabilities`：需要登录；未知的结果物类型与访问模式在 narrow 时丢弃。
    pub async fn capabilities(&self) -> Result<Value, ClientError> {
        let mut data = self
            .request(reqwest::Method::GET, "/shares/capabilities", None, true)
            .await?;
        for key in [
            "ttl_ms",
            "max_rows",
            "max_payload_bytes",
            "max_artifact_count",
            "max_artifact_bytes",
            "max_total_artifact_bytes",
            "schema_version",
        ] {
            require_int(&data, key)?;
        }
        let artifacts: Vec<Value> = data["allowed_artifacts"]
            .as_array()
            .ok_or_else(|| {
                invalid_contract(
                    "Conversation share API response does not match the client contract",
                )
            })?
            .iter()
            .filter(|entry| {
                entry["type"]
                    .as_str()
                    .is_some_and(|kind| ARTIFACT_TYPES.contains(&kind))
            })
            .cloned()
            .collect();
        let modes: Vec<Value> = data["access_modes"]
            .as_array()
            .ok_or_else(|| {
                invalid_contract(
                    "Conversation share API response does not match the client contract",
                )
            })?
            .iter()
            .filter(|mode| {
                mode.as_str()
                    .is_some_and(|mode| ACCESS_MODES.contains(&mode))
            })
            .cloned()
            .collect();
        data["allowed_artifacts"] = Value::Array(artifacts);
        data["access_modes"] = Value::Array(modes);
        Ok(data)
    }

    fn assert_supported_version(version: i64, endpoint: &str) -> Result<(), ClientError> {
        if version <= SCHEMA_VERSION {
            return Ok(());
        }
        log::warn(format!(
            "conversation share payload schema version is newer than this client endpoint={endpoint} version={version}"
        ));
        Err(ClientError::new(
            "unsupported_schema_version",
            "Conversation share payload requires a newer ZCode version",
        ))
    }

    /// `getPreview`：公开读取，不要求登录。
    pub async fn preview(&self, share_code: &str) -> Result<Value, ClientError> {
        let path = format!("/shares/{}/preview", encode_uri_component(share_code));
        let mut data = self
            .request(reqwest::Method::GET, &path, None, false)
            .await?;
        let version = check_public_payload(&data, false)?;
        Self::assert_supported_version(version, "preview")?;
        let (rows, unsupported, kinds) = decode_rows(
            data["rows"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default(),
        );
        if unsupported > 0 {
            log::info(format!(
                "conversation share dropped rows this client cannot render endpoint=preview kinds={kinds:?} droppedCount={unsupported}"
            ));
        }
        data["rows"] = Value::Array(rows);
        data["unsupportedRowCount"] = json!(unsupported);
        Ok(data)
    }

    /// `getContinuation`：对服务端原样发来的 rows / artifacts 复核完整性。
    pub async fn continuation(
        &self,
        share_code: &str,
        client_request_id: &str,
    ) -> Result<Value, ClientError> {
        if client_request_id.trim().is_empty() {
            return Err(invalid_contract("Invalid continuation request"));
        }
        let path = format!("/shares/{}/continuation", encode_uri_component(share_code));
        let body = json!({"schema_version": SCHEMA_VERSION, "client_request_id": client_request_id.trim()});
        let mut data = self
            .request(reqwest::Method::POST, &path, Some(body), false)
            .await?;
        let version = check_public_payload(&data, true)?;
        Self::assert_supported_version(version, "continuation")?;
        if !verify(&data["rows"], &data["artifacts"], &data["integrity"]) {
            return Err(invalid_contract(
                "Conversation share integrity check failed",
            ));
        }
        let raw_rows = data["rows"].clone();
        let (rows, unsupported, kinds) =
            decode_rows(raw_rows.as_array().map(Vec::as_slice).unwrap_or_default());
        if unsupported > 0 {
            log::info(format!(
                "conversation share dropped rows this client cannot render endpoint=continuation kinds={kinds:?} droppedCount={unsupported}"
            ));
        }
        data["rows"] = Value::Array(rows);
        data["rawRows"] = raw_rows;
        data["unsupportedRowCount"] = json!(unsupported);
        Ok(data)
    }

    /// 下载一个 artifact（导入用）；超时与 Content-Length 预检由调用方处理。
    pub async fn download(
        &self,
        url: &str,
        timeout: Duration,
    ) -> Result<reqwest::Response, reqwest::Error> {
        self.http.get(url).timeout(timeout).send().await
    }
}

/// JS `encodeURIComponent`。
pub fn encode_uri_component(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => (byte as char).to_string(),
            other => format!("%{other:02X}"),
        })
        .collect()
}
