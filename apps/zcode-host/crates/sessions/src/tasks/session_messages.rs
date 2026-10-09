//! 远端 workspace 的会话消息请求（远端 `zcode-task.onDynamicWorkspaceEvent` 的
//! `workspace_session_message_send_requested`）在 Host 内应答（docs/specs/rust-u4-app-host.md §2.5）。
//!
//! 窗口级 Host 时这条请求经 Main 的 taskRealtimeBus 绕回同一个 Host：Host 的 zcode-task 适配器不支持
//! `deliverSessionMessage`，回 `failed`；结果回到发起方 Host 后，`sendSessionMessageDeliveryResult`
//! 同样不支持，只记一条 warn。App 级 Host 只有它自己，所以直接在 Host 内得出同一个结果、记同一条 warn，
//! 不再发 `session-message-send-requested` / `session-route-announce`，也不处理
//! `session-message-deliver` / `session-message-delivery-result`。
//!
//! ```text
//! 远端 workspace 事件 ─► answer_send_request ─► 结果 {status: failed, error: 不支持 deliverSessionMessage}
//!                                          └─► 交回发起方：不支持 sendSessionMessageDeliveryResult ─► warn
//! ```
use serde_json::{Value, json};
use zcode_host_kit::log;

use super::lifecycle_members::unsupported;

/// 应答一条会话消息请求，返回 `SessionMessageDeliveryResult`（Node 同形）。
pub fn answer_send_request(request: &Value) -> Value {
    let result = json!({
        "error": unsupported("deliverSessionMessage").message,
        "messageId": request["messageId"],
        "requestId": request["requestId"],
        "sessionId": request["fromSessionId"],
        "status": "failed",
    });
    log::debug(format!("session message delivery result: {result}"));
    // 与经 Main 绕回时发起方收到结果后的处理相同：适配器无法转交结果，记 warn。
    log::warn(format!(
        "failed to forward session message delivery result: {}",
        unsupported("sendSessionMessageDeliveryResult").message
    ));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_message_requests_fail_inside_the_host_with_the_adapter_error() {
        let result = answer_send_request(&json!({
            "messageId": "m", "requestId": "q", "fromSessionId": "s", "toSessionId": "o",
            "content": "x", "createdAt": 1,
        }));
        assert_eq!(
            result,
            json!({
                "error": "ZCode task service adapter does not support IZCodeTaskService.deliverSessionMessage yet.",
                "messageId": "m",
                "requestId": "q",
                "sessionId": "s",
                "status": "failed",
            })
        );
        // 错误码与 zcode-task 的同名成员一致（ZCODE_AGENT_UNSUPPORTED_LEGACY_TASK_METHOD）。
        assert_eq!(
            unsupported("deliverSessionMessage")
                .code
                .as_deref()
                .and_then(Value::as_str),
            Some("ZCODE_AGENT_UNSUPPORTED_LEGACY_TASK_METHOD")
        );
    }
}
