//! 跨区域收尾（docs/specs/rust-host-integration.md）：动态工作流门禁的唯一写入方、Host 派发新建会话的
//! `session-create-telemetry`。其余收尾项落在各自的模块里（闲时凭据 `off_peak::credentials`、
//! settings-sync 手动导入、反向请求错误 `data`、RPC 遥测在 host-core）。
use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::account::coding_plan::CodingPlanSubscription;
use crate::agent::extensions::DynamicWorkflowGate;
use crate::automation::dispatch::is_remote_workspace_identity;
use crate::main_bridge::MainBridge;

/// 动态工作流门禁：`getDynamicWorkflowClientConfig().enabled === true`（本地覆盖
/// `ZCODE_DYNAMIC_WORKFLOW_MODE` 优先于远端配置，读取失败按关闭）。runtime 只调用一次并缓存。
pub fn dynamic_workflow_gate(plan: CodingPlanSubscription) -> DynamicWorkflowGate {
    Arc::new(move || {
        let plan = plan.clone();
        Box::pin(async move { Ok(plan.dynamic_workflow_config(false).await["enabled"] == true) })
    })
}

/// `AutomationSessionCreateTelemetry.eventExtraDetail.create_source`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateSource {
    /// automation 派发新建会话（定时与手动运行）。
    AutomationScheduled,
    /// 闲时任务 init 分支新建会话。
    AutomationIdle,
}

impl CreateSource {
    fn as_str(self) -> &'static str {
        match self {
            CreateSource::AutomationScheduled => "automation_scheduled",
            CreateSource::AutomationIdle => "automation_idle",
        }
    }
}

/// 一次 Host 派发新建会话的遥测输入（`reportHostSessionCreate`）。
pub struct SessionCreate<'a> {
    pub session_id: &'a str,
    pub message_id: &'a str,
    pub source: CreateSource,
    pub workspace_identity: Option<&'a str>,
}

/// `parseRemoteWorkspaceIdentity(identity)?.kind`。
fn remote_kind(identity: &str) -> &'static str {
    if !is_remote_workspace_identity(identity) {
        return "";
    }
    match identity.split(':').nth(1) {
        Some("ssh") => "ssh",
        Some("wsl") => "wsl",
        Some("docker") => "docker",
        _ => "",
    }
}

/// `session-create-telemetry` 消息（`hostSessionCreateTelemetryResponseSchema`）。
pub fn session_create_message(vars: &BTreeMap<String, String>, input: &SessionCreate<'_>) -> Value {
    let identity = input
        .workspace_identity
        .map(str::trim)
        .filter(|identity| !identity.is_empty());
    // `resolveWorkspaceTelemetryDetail`：有 identity 即视为远端，种类按 identity 解析。
    let (workspace_kind, remote_kind) = match identity {
        Some(identity) => ("remote", remote_kind(identity)),
        None => ("local", ""),
    };
    json!({
        "type": "session-create-telemetry",
        "event": {
            "elementName": "session_create",
            "eventRegion": "app",
            "eventType": "result",
            "talkId": input.session_id,
            "messageId": input.message_id,
            "context": {
                "clientTimezone": crate::account::net::client_timezone(vars)
                    .unwrap_or_else(|| "UTC".to_owned()),
                "clientLanguage": crate::account::net::client_language(vars),
                "screenResolution": "",
            },
            "eventExtraDetail": {
                "create_source": input.source.as_str(),
                "client_kind": "desktop",
                "workspace_kind": workspace_kind,
                "remote_kind": remote_kind,
            },
        },
    })
}

/// 只在实际新建并完成首发后调用；发送失败只丢弃本次上报，不影响派发结果。
pub fn report_session_create(
    bridge: &MainBridge,
    vars: &BTreeMap<String, String>,
    input: SessionCreate<'_>,
) {
    if input.session_id.is_empty() || input.message_id.is_empty() {
        return;
    }
    bridge.post(session_create_message(vars, &input));
}

#[cfg(test)]
mod tests;
