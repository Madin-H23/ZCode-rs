//! Main ↔ Host 控制消息（docs/specs/rust-host-r0.md §4.1）。
//!
//! 传输是 Rust Host 的 stdin/stdout，每行一个 JSON 对象；消息对象与今天 `parentPort` 上的相同，
//! TS 侧 schema 在 `packages/shared/src/validation.ts`。R0 只解析用到的字段，其余消息原样识别为
//! [`MainToHost::Other`]，由调用方记 debug 日志后忽略。
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
    Other {
        kind: String,
    },
}

/// `HostInitMessage` 的子集；其余字段在实现对应服务的里程碑再读取。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitLocal {
    pub database_startup_id: Option<String>,
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
        _ => MainToHost::Other { kind },
    })
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
    pub disk: Vec<Value>,
}

impl DatabaseStartupState {
    pub fn ready(startup_id: String, attempt_id: String, now_ms: u64) -> Self {
        Self {
            schema_version: 1,
            startup_id,
            attempt_id,
            sequence: 0,
            started_at: now_ms,
            updated_at: now_ms,
            phase: "ready",
            disk: Vec::new(),
        }
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
                database_startup_id: Some("s1".into())
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
                kind: "broadcast".into()
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
    }
}
