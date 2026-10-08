//! conversation-share 的错误契约（conversationShare.ts、conversationShareHttpClient.ts）。
//!
//! 两类错误经 RPC 透传键（`kind`、`status`、`code`、`retryAfterMs`、`details`）到 Renderer，UI 按
//! `kind` 分支：`ConversationShareClientError`（HTTP 层）与 `ConversationShareServiceError`
//! （业务层，`details` 只含脱敏后的安全字段）。
use regex::Regex;
use serde_json::{Map, Value, json};
use zcode_host_kit::RpcError;

const SAFE_DIAGNOSTIC_KEYS: [&str; 9] = [
    "rowKind",
    "rowId",
    "field",
    "artifactType",
    "extension",
    "phase",
    "expectedBytes",
    "actualBytes",
    "errno",
];
const MAX_FAILURE_ISSUES: usize = 5;

fn request_id_ok(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

/// `ConversationShareClientError`。
#[derive(Debug, Clone, PartialEq)]
pub struct ClientError {
    pub kind: &'static str,
    pub message: String,
    pub status: Option<u16>,
    pub code: Option<i64>,
    pub request_id: Option<String>,
    pub retry_after_ms: Option<i64>,
}

impl ClientError {
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status: None,
            code: None,
            request_id: None,
            retry_after_ms: None,
        }
    }

    pub fn status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    pub fn request_id(mut self, request_id: Option<String>) -> Self {
        self.request_id = request_id
            .filter(|id| request_id_ok(id.trim()))
            .map(|id| id.trim().to_owned());
        self
    }

    pub fn to_rpc(&self) -> RpcError {
        let mut error = RpcError::new("ConversationShareClientError", &self.message)
            .with_field("kind", self.kind);
        if let Some(status) = self.status {
            error = error.with_field("status", status);
        }
        if let Some(code) = self.code {
            error = error.with_code(code);
        }
        if let Some(request_id) = &self.request_id {
            error = error.with_field("details", json!({"requestId": request_id}));
        }
        if let Some(retry) = self.retry_after_ms {
            error = error.with_field("retryAfterMs", retry);
        }
        error
    }
}

/// `inferFailureReasonCode`：没有显式 reasonCode 时按文案归类。
pub fn infer_reason_code(message: &str) -> &'static str {
    let rules: [(&str, &str); 17] = [
        ("(?i)Running turns", "running_turn"),
        ("(?i)Streaming rows", "streaming_row"),
        ("(?i)Active tool calls", "active_tool_call"),
        ("(?i)Active subagents|subagent detail", "active_subagent"),
        (
            "(?i)attachments?|public input contains local write identity",
            "input_attachment",
        ),
        ("(?i)Inline tool images", "inline_tool_image"),
        (
            "(?i)Timeline|fork|checkpoint|summary references",
            "unsupported_timeline",
        ),
        ("(?i)Local and inline URLs", "unsafe_url"),
        (
            "(?i)missing its product turn|product turn identity",
            "missing_product_turn",
        ),
        (
            "(?i)Selected product turns|selected row|selection",
            "invalid_selection",
        ),
        (
            "(?i)not allowed by server capabilities",
            "artifact_type_not_allowed",
        ),
        ("(?i)extension is missing", "artifact_extension_missing"),
        ("(?i)outside the workspace", "artifact_outside_workspace"),
        (
            "(?i)changed after|file changed|size/mtime",
            "artifact_changed",
        ),
        (
            "(?i)artifact (?:ended|source|chunk|readable|cannot be read)",
            "artifact_read_failed",
        ),
        (
            "(?i)artifact (?:identifiers|manifest|acknowledgement|missing)",
            "artifact_manifest",
        ),
        (
            "(?i)payload|too many rows|too many artifacts",
            "payload_limit",
        ),
    ];
    rules
        .iter()
        .find(|(pattern, _)| Regex::new(pattern).is_ok_and(|regex| regex.is_match(message)))
        .map_or("invalid_conversation", |(_, code)| code)
}

fn unsafe_text(value: &str) -> bool {
    value.contains('/') || value.contains('\\') || value.contains("://")
}

fn safe_string(value: Option<&Value>) -> Option<Value> {
    let text = value?.as_str()?;
    (!text.is_empty() && text.len() <= 128 && !unsafe_text(text)).then(|| json!(text))
}

fn mime_ok(text: &str) -> bool {
    let Some((left, right)) = text.split_once('/') else {
        return false;
    };
    let part = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-'))
    };
    part(left) && part(right)
}

/// `sanitizeFailureIssue`：剥掉路径、URL、超长字段，保留定位信息。
pub fn sanitize_issue(issue: &Value) -> Value {
    let mut out = Map::new();
    out.insert("code".into(), issue["code"].clone());
    out.insert("scope".into(), issue["scope"].clone());
    for key in ["rowId", "turnOrdinal"] {
        if let Some(value) = issue.get(key).filter(|value| !value.is_null()) {
            out.insert(key.into(), value.clone());
        }
    }
    if let Some(id) = issue["productTurnId"].as_str().filter(|id| !id.is_empty()) {
        out.insert("productTurnId".into(), json!(id));
    }
    if let Some(name) = issue["artifactDisplayName"]
        .as_str()
        .map(str::trim)
        .filter(|name| !name.is_empty() && name.len() <= 128 && !unsafe_text(name))
    {
        out.insert("artifactDisplayName".into(), json!(name));
    }
    for key in ["artifactType", "extension"] {
        if let Some(value) = safe_string(issue.get(key)) {
            out.insert(key.into(), value);
        }
    }
    if let Some(mime) = issue["mimeType"]
        .as_str()
        .filter(|mime| mime.len() <= 128 && mime_ok(mime))
    {
        out.insert("mimeType".into(), json!(mime));
    }
    for key in ["actual", "limit", "retryAfterMs", "phase"] {
        if let Some(value) = issue.get(key).filter(|value| !value.is_null()) {
            out.insert(key.into(), value.clone());
        }
    }
    if let Some(formats) = issue["allowedFormats"].as_array() {
        let formats: Vec<Value> = formats
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty() && value.len() <= 128 && !unsafe_text(value))
            .map(|value| json!(value))
            .collect();
        if !formats.is_empty() {
            out.insert("allowedFormats".into(), Value::Array(formats));
        }
    }
    if let Some(artifacts) = issue["allowedArtifacts"].as_array() {
        let artifacts: Vec<Value> = artifacts
            .iter()
            .filter_map(|artifact| {
                let kind = artifact["type"].as_str()?.trim();
                let extensions: Vec<Value> = artifact["extensions"]
                    .as_array()?
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|value| value.trim().trim_start_matches('.'))
                    .filter(|value| !value.is_empty() && value.len() <= 32)
                    .map(|value| json!(value))
                    .collect();
                let mimes: Vec<Value> = artifact["mimeTypes"]
                    .as_array()
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(|value| value.trim().to_lowercase())
                            .filter(|value| mime_ok(value))
                            .map(Value::String)
                            .collect()
                    })
                    .unwrap_or_default();
                if kind.is_empty() || unsafe_text(kind) || extensions.is_empty() {
                    return None;
                }
                let mut entry = Map::new();
                entry.insert("type".into(), json!(kind));
                entry.insert("extensions".into(), Value::Array(extensions));
                entry.insert("mimeTypes".into(), Value::Array(mimes));
                if let Some(name) = artifact["displayName"]
                    .as_str()
                    .filter(|name| name.len() <= 64 && !unsafe_text(name))
                {
                    entry.insert("displayName".into(), json!(name.trim()));
                }
                Some(Value::Object(entry))
            })
            .collect();
        if !artifacts.is_empty() {
            out.insert("allowedArtifacts".into(), Value::Array(artifacts));
        }
    }
    if let Some(value) = issue.get("availability").filter(|value| !value.is_null()) {
        out.insert("availability".into(), value.clone());
    }
    Value::Object(out)
}

/// `sanitizeConversationShareIssues`。
pub fn sanitize_issues(issues: &[Value]) -> (Vec<Value>, usize, usize) {
    let sanitized: Vec<Value> = issues
        .iter()
        .take(MAX_FAILURE_ISSUES)
        .map(sanitize_issue)
        .collect();
    let omitted = issues.len().saturating_sub(sanitized.len());
    (sanitized, issues.len(), omitted)
}

/// `ConversationShareServiceError`。
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceError {
    pub kind: &'static str,
    pub message: String,
    pub status: Option<u16>,
    pub code: Option<i64>,
    pub request_id: Option<String>,
    pub reason_code: Option<&'static str>,
    pub diagnostics: Option<Map<String, Value>>,
    pub issues: Vec<Value>,
}

impl ServiceError {
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status: None,
            code: None,
            request_id: None,
            reason_code: None,
            diagnostics: None,
            issues: Vec::new(),
        }
    }

    pub fn issues(mut self, issues: Vec<Value>) -> Self {
        self.issues = issues;
        self
    }

    pub fn diagnostics(mut self, diagnostics: Map<String, Value>) -> Self {
        self.diagnostics = Some(diagnostics);
        self
    }

    pub fn to_rpc(&self) -> RpcError {
        let reason = self
            .reason_code
            .unwrap_or_else(|| infer_reason_code(&self.message));
        let mut details = Map::new();
        let request_id = self
            .request_id
            .as_deref()
            .map(str::trim)
            .filter(|id| request_id_ok(id));
        if let Some(id) = request_id {
            details.insert("requestId".into(), json!(id));
        }
        if let Some(status) = self.status {
            details.insert("status".into(), json!(status));
        }
        if let Some(code) = self.code {
            details.insert("code".into(), json!(code));
        }
        details.insert("reasonCode".into(), json!(reason));
        if let Some(diagnostics) = &self.diagnostics {
            let safe: Map<String, Value> = diagnostics
                .iter()
                .filter(|(key, value)| {
                    SAFE_DIAGNOSTIC_KEYS.contains(&key.as_str())
                        && value
                            .as_str()
                            .is_none_or(|text| !unsafe_text(text) && text.len() <= 128)
                })
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            if !safe.is_empty() {
                details.insert("diagnostics".into(), Value::Object(safe));
            }
        }
        let (issues, count, omitted) = sanitize_issues(&self.issues);
        if !issues.is_empty() {
            details.insert("issues".into(), Value::Array(issues));
        }
        if count > 0 {
            details.insert("issueCount".into(), json!(count));
            details.insert("omittedIssueCount".into(), json!(omitted));
        }
        let mut error = RpcError::new("ConversationShareServiceError", &self.message)
            .with_field("kind", self.kind)
            .with_field("details", Value::Object(details));
        if let Some(status) = self.status {
            error = error.with_field("status", status);
        }
        if let Some(code) = self.code {
            error = error.with_code(code);
        }
        error
    }
}

/// 两类错误的统一出口。
#[derive(Debug, Clone, PartialEq)]
pub enum ShareError {
    Client(ClientError),
    Service(ServiceError),
    /// 其他（文件系统、CLI）错误：原样作为普通 Error。
    Other(RpcError),
}

impl ShareError {
    pub fn to_rpc(&self) -> RpcError {
        match self {
            ShareError::Client(error) => error.to_rpc(),
            ShareError::Service(error) => error.to_rpc(),
            ShareError::Other(error) => error.clone(),
        }
    }
}

impl From<ClientError> for ShareError {
    fn from(error: ClientError) -> Self {
        ShareError::Client(error)
    }
}

impl From<ServiceError> for ShareError {
    fn from(error: ServiceError) -> Self {
        ShareError::Service(error)
    }
}

impl From<RpcError> for ShareError {
    fn from(error: RpcError) -> Self {
        ShareError::Other(error)
    }
}

impl From<std::io::Error> for ShareError {
    fn from(error: std::io::Error) -> Self {
        ShareError::Other(RpcError::new("Error", error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_errors_carry_safe_details_only() {
        let error = ServiceError::new(
            "invalid_contract",
            "Conversation artifact integrity check failed",
        )
        .issues(vec![json!({
            "code": "artifact_changed",
            "scope": "artifact",
            "artifactDisplayName": "/etc/passwd",
            "mimeType": "text/plain",
            "actual": 3,
        })])
        .to_rpc();
        let payload = error.to_json();
        assert_eq!(payload["kind"], "invalid_contract");
        assert_eq!(payload["details"]["reasonCode"], "invalid_conversation");
        assert_eq!(payload["details"]["issueCount"], 1);
        assert!(
            payload["details"]["issues"][0]
                .get("artifactDisplayName")
                .is_none()
        );
        assert_eq!(
            infer_reason_code("artifact cannot be read"),
            "artifact_read_failed"
        );
        let client = ClientError::new("authentication_required", "x")
            .status(401)
            .to_rpc()
            .to_json();
        assert_eq!(client["kind"], "authentication_required");
        assert_eq!(client["status"], 401);
    }
}
