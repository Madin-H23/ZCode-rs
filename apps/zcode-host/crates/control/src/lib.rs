//! Main ↔ Host 控制消息（docs/specs/rust-host-r0.md §4.1）。
//!
//! 传输是 Rust Host 的 stdin/stdout，每行一个 JSON 对象；消息对象与今天 `parentPort` 上的相同，
//! TS 侧 schema 在 `packages/shared/src/validation.ts`。R0 只解析用到的字段，其余消息原样识别为
//! [`MainToHost::Other`]，由调用方记 debug 日志后忽略。
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const INIT_LOCAL: &str = "init-local";
pub const ISSUE_ATTACHMENT_ENDPOINT: &str = "issue-attachment-endpoint";
pub const DISPOSE: &str = "dispose";

#[derive(Debug, Clone, PartialEq)]
pub enum MainToHost {
    InitLocal(InitLocal),
    IssueAttachmentEndpoint(IssueAttachmentEndpoint),
    /// 请求带 `requestId` 但载荷不合法：仍需回 `ok:false`，否则 Main 会一直等待。
    InvalidIssueAttachmentEndpoint {
        request_id: String,
        error: String,
    },
    Dispose,
    /// 本 crate 不解析的消息原样保留，由 [`ControlRouter`] 交给登记了该 `type` 的 service。
    Other {
        kind: String,
        message: Value,
    },
}

/// `HostInitMessage`（packages/desktop/src/main/desktopHostProcess.ts）。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitLocal {
    pub database_startup_id: Option<String>,
    pub host_id: Option<String>,
    pub device_mid: Option<String>,
    pub workspace_path: Option<String>,
    pub workspace_identity: Option<String>,
    #[serde(default)]
    pub agent_warmup_targets: Vec<WarmupTarget>,
    pub agent_spawn_fallback_cwd: Option<String>,
    /// Main 解析好的内置 Provider 配置路径；Host 不感知 Electron 安装布局。
    pub zcode_builtin_provider_config_file_path: Option<String>,
    /// Main 采集的登录 shell 环境补丁（PATH 等）；Host 显式传给子进程，不改写自身环境。
    #[serde(default)]
    pub runtime_process_env_patch: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WarmupTarget {
    pub workspace_path: String,
    pub workspace_identity: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMode {
    #[serde(rename = "desktop-continuous")]
    DesktopContinuous,
    #[serde(rename = "web-remote-replayable")]
    WebRemoteReplayable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum AttachmentScope {
    #[serde(rename = "local")]
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IssueAttachmentEndpoint {
    #[serde(rename = "type")]
    pub kind: String,
    pub request_id: String,
    pub attachment_id: String,
    pub client_mode: ClientMode,
    pub scope: AttachmentScope,
}

pub fn parse_main_to_host(line: &str) -> Result<MainToHost> {
    let value: Value = serde_json::from_str(line).context("control line is not JSON")?;
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .context("control message without type")?
        .to_owned();
    Ok(match kind.as_str() {
        INIT_LOCAL => MainToHost::InitLocal(serde_json::from_value(value)?),
        ISSUE_ATTACHMENT_ENDPOINT => {
            let request_id = value
                .get("requestId")
                .and_then(Value::as_str)
                .context("issue-attachment-endpoint without requestId")?
                .to_owned();
            match serde_json::from_value::<IssueAttachmentEndpoint>(value) {
                Ok(message) => MainToHost::IssueAttachmentEndpoint(message),
                Err(error) => MainToHost::InvalidIssueAttachmentEndpoint {
                    request_id,
                    error: error.to_string(),
                },
            }
        }
        DISPOSE => MainToHost::Dispose,
        _ => MainToHost::Other {
            kind,
            message: value,
        },
    })
}

/// Host → Main 控制消息的出口；实现只做入队，不能阻塞调用方。
pub trait ControlSink: Send + Sync {
    fn send(&self, message: HostToMain);
}

type ControlHandler = Arc<dyn Fn(Value) + Send + Sync>;

/// Main → Host 消息按 `type` 分发给 service；同一 `type` 只能有一个处理者。
#[derive(Default, Clone)]
pub struct ControlRouter {
    handlers: HashMap<String, ControlHandler>,
}

impl ControlRouter {
    pub fn on(&mut self, kind: &str, handler: impl Fn(Value) + Send + Sync + 'static) {
        self.handlers.insert(kind.to_owned(), Arc::new(handler));
    }

    /// 返回是否有处理者。
    pub fn dispatch(&self, kind: &str, message: Value) -> bool {
        match self.handlers.get(kind) {
            Some(handler) => {
                handler(message);
                true
            }
            None => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum HostToMain {
    #[serde(rename = "host-endpoint-ready", rename_all = "camelCase")]
    HostEndpointReady { ws_port: u16 },
    #[serde(rename = "database-startup-state")]
    DatabaseStartupState { state: DatabaseStartupState },
    #[serde(rename = "attachment-endpoint-issued", rename_all = "camelCase")]
    AttachmentEndpointIssued {
        request_id: String,
        #[serde(flatten)]
        result: AttachmentEndpointResult,
    },
    /// 结构化日志（`hostLogResponseSchema`），由 Main 的 hostLogRelay 实时落盘。
    #[serde(rename = "log")]
    Log {
        level: LogLevel,
        source: &'static str,
        message: String,
    },
    /// service 自行构造、已符合 `hostResponseMessageSchema` 的消息，原样写出。
    #[serde(untagged)]
    Raw(Value),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum AttachmentEndpointResult {
    Issued {
        ok: True,
        url: String,
        capability: String,
    },
    Failed {
        ok: False,
        error: String,
    },
}

/// JSON 字面量 `true` / `false`，让 `ok` 字段与 TS 的判别联合一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct True;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct False;

impl Serialize for True {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bool(true)
    }
}

impl Serialize for False {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bool(false)
    }
}

/// `databaseStartupStateSchema` 的 ready 子集（`packages/shared/src/database-startup.ts`）。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseStartupState {
    pub schema_version: u8,
    pub startup_id: String,
    pub attempt_id: String,
    pub sequence: u64,
    pub started_at: u64,
    pub updated_at: u64,
    pub phase: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database_phase: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_phase: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<&'static str>,
    pub disk: Vec<Value>,
}

impl DatabaseStartupState {
    pub fn new(
        startup_id: String,
        attempt_id: String,
        sequence: u64,
        started_at: u64,
        now_ms: u64,
        phase: &'static str,
    ) -> Self {
        Self {
            schema_version: 1,
            startup_id,
            attempt_id,
            sequence,
            started_at,
            updated_at: now_ms,
            phase,
            database_phase: None,
            failed_phase: None,
            error_code: None,
            disk: Vec::new(),
        }
    }

    pub fn ready(startup_id: String, attempt_id: String, now_ms: u64) -> Self {
        Self::new(startup_id, attempt_id, 0, now_ms, now_ms, "ready")
    }
}

pub fn encode_line(message: &HostToMain) -> String {
    let mut line = serde_json::to_string(message).unwrap_or_default();
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_init_local_and_ignores_other_fields() {
        let message = parse_main_to_host(
            r#"{"type":"init-local","databaseStartupId":"s1","runtimeProcessEnvPatch":{"PATH":"/bin"}}"#,
        )
        .unwrap();
        assert_eq!(
            message,
            MainToHost::InitLocal(InitLocal {
                database_startup_id: Some("s1".into()),
                runtime_process_env_patch: [("PATH".to_owned(), "/bin".to_owned())].into(),
                ..InitLocal::default()
            })
        );
    }

    #[test]
    fn invalid_issue_request_keeps_request_id() {
        let message = parse_main_to_host(
            r#"{"type":"issue-attachment-endpoint","requestId":"r1","attachmentId":"a","clientMode":"desktop-continuous","scope":{"kind":"remote"}}"#,
        )
        .unwrap();
        assert!(matches!(
            message,
            MainToHost::InvalidIssueAttachmentEndpoint { request_id, .. } if request_id == "r1"
        ));
    }

    #[test]
    fn unknown_messages_are_reported_by_type() {
        assert_eq!(
            parse_main_to_host(r#"{"type":"broadcast","payload":1}"#).unwrap(),
            MainToHost::Other {
                kind: "broadcast".into(),
                message: json!({"type":"broadcast","payload":1}),
            }
        );
    }

    #[test]
    fn encodes_host_messages_as_typescript_shapes() {
        let issued = HostToMain::AttachmentEndpointIssued {
            request_id: "r1".into(),
            result: AttachmentEndpointResult::Issued {
                ok: True,
                url: "ws://127.0.0.1:1/rpc".into(),
                capability: "ab".into(),
            },
        };
        assert_eq!(
            serde_json::to_value(&issued).unwrap(),
            json!({"type":"attachment-endpoint-issued","requestId":"r1","ok":true,"url":"ws://127.0.0.1:1/rpc","capability":"ab"})
        );
        let ready = HostToMain::HostEndpointReady { ws_port: 4000 };
        assert_eq!(
            serde_json::to_value(&ready).unwrap(),
            json!({"type":"host-endpoint-ready","wsPort":4000})
        );
        let state = HostToMain::DatabaseStartupState {
            state: DatabaseStartupState::ready("s1".into(), "a1".into(), 5),
        };
        assert_eq!(
            serde_json::to_value(&state).unwrap(),
            json!({"type":"database-startup-state","state":{"schemaVersion":1,"startupId":"s1","attemptId":"a1","sequence":0,"startedAt":5,"updatedAt":5,"phase":"ready","disk":[]}})
        );
        assert!(encode_line(&ready).ends_with('\n'));
        let log = HostToMain::Log {
            level: LogLevel::Warn,
            source: "zcode-host",
            message: "m".into(),
        };
        assert_eq!(
            serde_json::to_value(&log).unwrap(),
            json!({"type":"log","level":"warn","source":"zcode-host","message":"m"})
        );
    }
}
