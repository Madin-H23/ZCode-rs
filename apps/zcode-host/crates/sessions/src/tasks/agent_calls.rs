//! zcode-task legacy 成员对 CLI 的直接调用（替代 Node 的 IZCodeAgentService legacy 方法）：
//! V4 命令信封与 ACK 收口（zcodeV4HostCommand.ts）、CAS 重试，以及 `session/*` 请求。
//!
//! 进程选择与 Node 相同：写路径（`getClient`）走 `get_ready_client` 的 provider 门禁；只读路径
//! （`getReadOnlyClient`）用 `get_client`。Rust CLI 是当前协议，不做旧 schema 的兼容重试。
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};
use zcode_host_kit::RpcError;

use crate::agent::{AgentRuntime, RuntimePolicy, WorkspaceTarget};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

/// RFC 9562 uuid v7（与 TS `uuidv7` 相同的位布局）。
pub fn uuid_v7() -> String {
    let mut bytes = [0u8; 16];
    // 熵源不可用时无法生成命令 id，与连接 id 一样直接终止。
    getrandom::fill(&mut bytes).expect("operating system random source unavailable");
    let ts = now_ms();
    for (index, shift) in [40, 32, 24, 16, 8, 0].into_iter().enumerate() {
        bytes[index] = ((ts >> shift) & 0xff) as u8;
    }
    bytes[6] = 0x70 | (bytes[6] & 0x0f);
    bytes[8] = 0x80 | (bytes[8] & 0x3f);
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Host 进程内稳定的 V4 clientId（Host 重启即新的提交端；幂等表以 commandId 为键）。
pub fn host_client_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| format!("host-services-{}", uuid_v7()))
}

/// `createHostCommandEnvelope`：键序 commandId, clientId, sessionId, baseRevision?, type, payload, issuedAt。
pub fn envelope(
    command_type: &str,
    payload: Value,
    session_id: Option<&str>,
    command_id: Option<&str>,
    client_id: Option<&str>,
    base_revision: Option<i64>,
) -> Value {
    let mut map = Map::new();
    map.insert(
        "commandId".into(),
        json!(command_id.map(str::to_owned).unwrap_or_else(uuid_v7)),
    );
    map.insert(
        "clientId".into(),
        json!(client_id.unwrap_or(host_client_id())),
    );
    map.insert("sessionId".into(), json!(session_id));
    if let Some(revision) = base_revision {
        map.insert("baseRevision".into(), json!(revision));
    }
    map.insert("type".into(), json!(command_type));
    map.insert("payload".into(), payload);
    map.insert("issuedAt".into(), json!(now_ms()));
    Value::Object(map)
}

fn rejected(command_type: &str, ack: &Value, context: &str) -> RpcError {
    let mut message = format!(
        "v4 command {command_type} {}",
        ack["status"].as_str().unwrap_or_default()
    );
    if let Some(reason) = ack["reasonCode"].as_str().filter(|r| !r.is_empty()) {
        message.push_str(&format!(" ({reason})"));
    }
    if let Some(detail) = ack["message"].as_str().filter(|m| !m.is_empty()) {
        message.push_str(&format!(": {detail}"));
    }
    message.push_str(&format!(" — {context}"));
    RpcError::new("ZCodeV4CommandRejectedError", message)
        .with_code("ZCODE_V4_COMMAND_REJECTED")
        .with_data(json!({"ack": ack, "commandType": command_type}))
}

/// `assertV4CommandAckOk`：accepted / duplicate / noop 视为成功。
pub fn ack_ok(command_type: &str, ack: Value, context: &str) -> Result<Value, RpcError> {
    match ack["status"].as_str() {
        Some("accepted" | "duplicate" | "noop") => Ok(ack),
        _ => Err(rejected(command_type, &ack, context)),
    }
}

/// `sendConversationCommandV4`（host 发起，clientMode 恒为 desktop-continuous）。
pub async fn send_command(
    runtime: &AgentRuntime,
    target: &WorkspaceTarget,
    mut envelope: Value,
) -> Result<Value, RpcError> {
    let process = runtime.get_ready_client(target).await?;
    // 非本地 continuous 目标不带 ttft（同 Node）；host 构造的信封本就不含它。
    let non_local = target
        .workspace_identity
        .as_deref()
        .is_some_and(|id| !id.trim().is_empty())
        || target.remote_session_id.is_some();
    if non_local && let Some(object) = envelope.as_object_mut() {
        object.remove("ttft");
    }
    process.request_value("v4/command", &envelope).await
}

/// `sendHostCasCommandV4`：从 baseRevision 0 起，stale 时取 `revisionAtDecision` 重试，最多 4 次。
pub async fn send_cas(
    runtime: &AgentRuntime,
    target: &WorkspaceTarget,
    command_type: &str,
    payload: Value,
    session_id: &str,
    context: &str,
) -> Result<Value, RpcError> {
    let mut base_revision = 0;
    let mut last: Option<Value> = None;
    for _ in 0..4 {
        let ack = send_command(
            runtime,
            target,
            envelope(
                command_type,
                payload.clone(),
                Some(session_id),
                None,
                None,
                Some(base_revision),
            ),
        )
        .await?;
        if ack["status"] == "stale" {
            base_revision = ack["revisionAtDecision"].as_i64().unwrap_or(base_revision);
            last = Some(ack);
            continue;
        }
        return ack_ok(command_type, ack, context);
    }
    let ack = last.unwrap_or_else(
        || json!({"commandId": "", "status": "stale", "revisionAtDecision": base_revision}),
    );
    Err(rejected(
        command_type,
        &ack,
        &format!("{context}（连续 stale，放弃重试）"),
    ))
}

/// 写路径请求（`getClient`）。
pub async fn request(
    runtime: &AgentRuntime,
    target: &WorkspaceTarget,
    method: &str,
    params: Value,
) -> Result<Value, RpcError> {
    let process = runtime.get_ready_client(target).await?;
    process.request_value(method, &params).await
}

/// 只读请求（`getReadOnlyClient`，start-if-needed）。
pub async fn read(
    runtime: &AgentRuntime,
    target: &WorkspaceTarget,
    method: &str,
    params: Value,
) -> Result<Value, RpcError> {
    let process = runtime
        .get_client(target, RuntimePolicy::StartIfNeeded)
        .await?;
    process.request_value(method, &params).await
}

pub async fn read_session(
    runtime: &AgentRuntime,
    target: &WorkspaceTarget,
    session_id: &str,
) -> Result<Value, RpcError> {
    read(
        runtime,
        target,
        "session/read",
        json!({"sessionId": session_id}),
    )
    .await
}

/// `session/resume`（buildSessionResumeParams；模型 hint 不上线，同 Node）。
pub async fn resume_session(
    runtime: &AgentRuntime,
    target: &WorkspaceTarget,
    session_id: &str,
    thought_level: Option<&str>,
    mcp_servers: Option<&Value>,
) -> Result<Value, RpcError> {
    let mut params = Map::new();
    params.insert("sessionId".into(), json!(session_id));
    params.insert("workspace".into(), target.workspace_ref());
    if let Some(level) = thought_level {
        params.insert("thoughtLevel".into(), json!(level));
    }
    if let Some(servers) = mcp_servers {
        params.insert("mcpServers".into(), servers.clone());
    }
    params.extend(runtime.extensions().session_tool_flags(target).await);
    request(runtime, target, "session/resume", Value::Object(params)).await
}

/// 匹配 `isSessionMissingError`：`/\bSession (not found|is not active):/i`。
pub fn is_session_missing(error: &RpcError) -> bool {
    let lower = error.message.to_lowercase();
    ["session not found:", "session is not active:"]
        .iter()
        .any(|needle| {
            lower.match_indices(needle).any(|(index, _)| {
                index == 0
                    || !lower[..index]
                        .chars()
                        .next_back()
                        .is_some_and(|c| c.is_alphanumeric() || c == '_')
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_and_ack_follow_typescript() {
        let value = envelope("stop", json!({}), Some("s1"), Some("c1"), None, Some(3));
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "commandId",
                "clientId",
                "sessionId",
                "baseRevision",
                "type",
                "payload",
                "issuedAt"
            ]
        );
        assert!(
            value["clientId"]
                .as_str()
                .unwrap()
                .starts_with("host-services-")
        );
        assert_eq!(uuid_v7().as_bytes()[14], b'7');
        let error = ack_ok(
            "stop",
            json!({"status": "rejected", "reasonCode": "busy", "message": "no"}),
            "session=s1",
        )
        .unwrap_err();
        assert_eq!(
            error.message,
            "v4 command stop rejected (busy): no — session=s1"
        );
        assert_eq!(error.code_str(), Some("ZCODE_V4_COMMAND_REJECTED"));
        assert!(ack_ok("stop", json!({"status": "noop"}), "x").is_ok());
        assert!(is_session_missing(&RpcError::new(
            "E",
            "Session not found: abc"
        )));
        assert!(!is_session_missing(&RpcError::new(
            "E",
            "XSession not found: abc"
        )));
    }
}
