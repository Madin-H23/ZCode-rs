//! 插件管理、MCP 状态、技能 / 插件引用目录与已保存工作流（docs/specs/rust-host-agent.md §2.2）。
use std::sync::Arc;

use serde_json::{Map, Value};
use zcode_host_kit::{RpcError, log};

use super::api::{
    AgentApi, MCP_STATUS_MODE_UNSUPPORTED_CODE, PLUGIN_CANCEL_TIMEOUT, PLUGIN_MANAGEMENT_TIMEOUT,
    copy_defined, copy_truthy, unrecognized_top_level_keys,
};
use super::runtime::{AgentProcess, WorkspaceTarget};

/// 一个插件管理成员：CLI 方法、无条件字段、`!== undefined` 字段、真值字段、超时。
pub struct PluginMethod {
    pub member: &'static str,
    pub method: &'static str,
    pub required: &'static [&'static str],
    pub defined: &'static [&'static str],
    pub truthy: &'static [&'static str],
    /// false：`plugins/list` 用默认超时并在超时后重试一次。
    pub long_timeout: bool,
    /// false：不带 workspace（cancelOperation）。
    pub workspace: bool,
}

const fn method(
    member: &'static str,
    method: &'static str,
    required: &'static [&'static str],
    defined: &'static [&'static str],
    truthy: &'static [&'static str],
) -> PluginMethod {
    PluginMethod {
        member,
        method,
        required,
        defined,
        truthy,
        long_timeout: true,
        workspace: true,
    }
}

/// plugin-management lane 上的成员（zcodeAgentService.ts 逐字段对应）。
pub const PLUGIN_METHODS: &[PluginMethod] = &[
    PluginMethod {
        long_timeout: false,
        ..method("listPlugins", "plugins/list", &[], &[], &["configScope"])
    },
    method(
        "getPluginsOverview",
        "plugins/overview",
        &[],
        &[],
        &["configScope"],
    ),
    method(
        "resolveSuggestedPluginReference",
        "plugins/resolveSuggestedReference",
        &["stableId", "operationId", "clientMode", "deliveryKind"],
        &[],
        &[],
    ),
    method(
        "addPluginMarketplace",
        "plugins/marketplace/add",
        &["source"],
        &["dryRun"],
        &["operationId"],
    ),
    method(
        "removePluginMarketplace",
        "plugins/marketplace/remove",
        &["marketplace"],
        &[],
        &[],
    ),
    method(
        "updatePluginMarketplace",
        "plugins/marketplace/update",
        &[],
        &[],
        &["marketplace", "operationId"],
    ),
    method(
        "installPlugin",
        "plugins/install",
        &["pluginName", "marketplace"],
        &["dryRun"],
        &["scope", "operationId"],
    ),
    PluginMethod {
        workspace: false,
        ..method(
            "cancelPluginOperation",
            "plugins/cancelOperation",
            &["operationId"],
            &[],
            &[],
        )
    },
    method(
        "uninstallPlugin",
        "plugins/uninstall",
        &[],
        &["removeCache"],
        &["pluginId", "pluginName", "marketplace"],
    ),
    method(
        "updatePlugin",
        "plugins/update",
        &[],
        &[],
        &["pluginId", "marketplace"],
    ),
    method(
        "restoreBuiltinPlugin",
        "plugins/restoreBuiltin",
        &["pluginId"],
        &[],
        &[],
    ),
    method(
        "configurePlugin",
        "plugins/configure",
        &["pluginId", "options"],
        &["dryRun"],
        &["scope"],
    ),
    method(
        "resetPluginConfig",
        "plugins/resetConfig",
        &["pluginId"],
        &[],
        &["scope"],
    ),
    method(
        "validatePlugin",
        "plugins/validate",
        &[],
        &[],
        &["pluginName", "marketplace", "source"],
    ),
    method(
        "describePlugin",
        "plugins/describe",
        &["marketplace", "pluginName"],
        &[],
        &[],
    ),
    method(
        "setPluginEnabled",
        "plugins/setEnabled",
        &["pluginId", "enabled"],
        &[],
        &["operationId", "scope"],
    ),
];

fn is_timeout(error: &RpcError, method: &str) -> bool {
    error.message == format!("ZCode agent request timed out: {method}")
}

impl AgentApi {
    pub async fn plugin_call(
        &self,
        spec: &PluginMethod,
        params: &Value,
    ) -> Result<Value, RpcError> {
        let mut request = Map::new();
        if spec.workspace {
            let target = WorkspaceTarget::from_value(params)?;
            request.insert("workspace".into(), target.workspace_ref());
        }
        for key in spec.required {
            // 原样转发（undefined 不发），由 CLI 的 strict schema 判定。
            copy_defined(&mut request, params, &[key]);
        }
        copy_defined(&mut request, params, spec.defined);
        copy_truthy(&mut request, params, spec.truthy);
        // configurePlugin：clearOptionKeys 只在非空时下发。
        if spec.member == "configurePlugin"
            && params["clearOptionKeys"]
                .as_array()
                .is_some_and(|keys| !keys.is_empty())
        {
            request.insert("clearOptionKeys".into(), params["clearOptionKeys"].clone());
        }
        let request = Value::Object(request);
        if spec.member == "cancelPluginOperation" {
            return self
                .plugin_client()
                .await?
                .request_with_timeout(spec.method, &request, PLUGIN_CANCEL_TIMEOUT)
                .await;
        }
        if spec.long_timeout {
            // 官方 marketplace 首次接入需要 clone 仓库，默认超时会杀掉健康的 agent。
            return self
                .plugin_client()
                .await?
                .request_with_timeout(spec.method, &request, PLUGIN_MANAGEMENT_TIMEOUT)
                .await;
        }
        match self
            .plugin_client()
            .await?
            .request_value(spec.method, &request)
            .await
        {
            Err(error) if is_timeout(&error, spec.method) => {
                // 超时已回收不回包的旧进程；幂等的列表请求在新进程上重试一次。
                log::warn(format!(
                    "{} timed out; retrying on a fresh agent",
                    spec.method
                ));
                self.plugin_client()
                    .await?
                    .request_value(spec.method, &request)
                    .await
            }
            other => other,
        }
    }

    pub async fn list_mcp_server_statuses(&self, params: &Value) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        let request = |omit_mode: bool, omit_servers: bool| {
            let mut request = Map::new();
            request.insert("workspace".into(), target.workspace_ref());
            if !omit_servers {
                copy_defined(&mut request, params, &["mcpServers"]);
            }
            if !omit_mode {
                copy_defined(&mut request, params, &["mode"]);
            }
            Value::Object(request)
        };
        let error = match self
            .mcp_client()
            .await?
            .request_value("mcp/list", &request(false, false))
            .await
        {
            Ok(result) => return Ok(result),
            Err(error) => error,
        };
        let keys = unrecognized_top_level_keys(&error);
        let has = |key: &str| keys.iter().any(|known| known == key);
        if params["mode"] == "status" && has("mode") {
            // 旧 Agent 不认 status-only：省略 mode 重试会退回 connect 并改变连接集合；
            // 稳定错误码让设置页停止不可能成功的轮询。
            return Err(RpcError::new(
                "ZCodeAgentMcpStatusModeUnsupportedError",
                "The connected ZCode Agent does not support MCP status-only refresh",
            )
            .with_code(MCP_STATUS_MODE_UNSUPPORTED_CODE));
        }
        let defined = |key: &str| params.get(key).is_some_and(|value| !value.is_null());
        if (defined("mode") && has("mode")) || (defined("mcpServers") && has("mcpServers")) {
            log::warn(format!("mcp/list compat retry without {keys:?}"));
            return self
                .mcp_client()
                .await?
                .request_value("mcp/list", &request(has("mode"), has("mcpServers")))
                .await;
        }
        if !is_timeout(&error, "mcp/list") {
            return Err(error);
        }
        log::warn("mcp/list timed out; retrying on a fresh agent");
        self.mcp_client()
            .await?
            .request_value("mcp/list", &request(false, false))
            .await
    }

    /// Plugin 引用目录走持有 session 的 chat 进程（插件管理进程没有 session）；
    /// 新入口 `-32601` 时退回旧方法。
    pub async fn plugin_reference_catalog(&self, params: &Value) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        let process = self.read_client(&target).await?;
        let mut request = Map::new();
        request.insert("workspace".into(), target.workspace_ref());
        copy_truthy(&mut request, params, &["sessionId"]);
        let request = Value::Object(request);
        match process
            .request_value("plugins/referenceCatalogWithCategory", &request)
            .await
        {
            Err(error) if error.code_i64() == Some(-32601) => {
                process
                    .request_value("plugins/referenceCatalog", &request)
                    .await
            }
            other => other,
        }
    }

    pub async fn skill_reference_catalog(&self, params: &Value) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        let process = self.read_client(&target).await?;
        let mut request = Map::new();
        request.insert("workspace".into(), target.workspace_ref());
        copy_truthy(&mut request, params, &["sessionId"]);
        process
            .request_value("skills/referenceCatalog", &Value::Object(request))
            .await
    }

    /// 已保存工作流的载体：带 workspacePath 就用它；只给 `scope:"global"` 时复用任一本地 chat
    /// 进程（不为此拉起），没有则用 plugin-management lane。
    async fn workflow_carrier(
        &self,
        params: &Value,
    ) -> Result<(Arc<AgentProcess>, Value), RpcError> {
        if params["workspacePath"].is_string() {
            let target = WorkspaceTarget::from_value(params)?;
            return Ok((self.read_client(&target).await?, target.workspace_ref()));
        }
        for process in self.chat.live_processes().await {
            if process.target.is_local() {
                let workspace = process.target.workspace_ref();
                return Ok((process, workspace));
            }
        }
        let target = self.plugin_target().await?;
        Ok((self.plugin_client().await?, target.workspace_ref()))
    }

    pub async fn workflow_call(
        &self,
        method: &str,
        params: &Value,
        fields: &[&str],
    ) -> Result<Value, RpcError> {
        let (process, workspace) = self.workflow_carrier(params).await?;
        let mut request = Map::new();
        request.insert("workspace".into(), workspace);
        copy_defined(&mut request, params, fields);
        // scope 只在有定义时下推：不给 scope 的项目档保持原有线上形状。
        copy_defined(&mut request, params, &["scope"]);
        process.request_value(method, &Value::Object(request)).await
    }

    /// 全局档搬回项目档：workspace 必带，既是载体又是目标项目。
    pub async fn move_saved_workflow(&self, params: &Value) -> Result<Value, RpcError> {
        let target = WorkspaceTarget::from_value(params)?;
        let process = self.read_client(&target).await?;
        let mut request = Map::new();
        request.insert("workspace".into(), target.workspace_ref());
        copy_defined(&mut request, params, &["name"]);
        process
            .request_value("workflows/move", &Value::Object(request))
            .await
    }

    pub fn plugin_method(member: &str) -> Option<&'static PluginMethod> {
        PLUGIN_METHODS.iter().find(|spec| spec.member == member)
    }
}

/// 只读的 V4 workflow run 查询（plans / workflowRunEvents 同族：无状态、超时重发安全）。
pub const WORKFLOW_RUN_QUERIES: &[(&str, &str, &[&str])] = &[
    (
        "conversationWorkflowRunEventsV4",
        "v4/conversation/workflowRunEvents",
        &["sessionId", "runId", "afterSequence", "limit"],
    ),
    (
        "conversationWorkflowRunArtifactsV4",
        "v4/conversation/workflowRunArtifacts",
        &["sessionId", "runId"],
    ),
    (
        "conversationWorkflowRunArtifactDataV4",
        "v4/conversation/workflowRunArtifactData",
        &["sessionId", "runId", "artifactId", "afterSequence", "limit"],
    ),
    (
        "conversationWorkflowRunArtifactReadV4",
        "v4/conversation/workflowRunArtifactRead",
        &[
            "sessionId",
            "runId",
            "artifactId",
            "version",
            "offset",
            "limit",
        ],
    ),
    (
        "conversationWorkflowRunWorkspaceV4",
        "v4/conversation/workflowRunWorkspace",
        &["sessionId", "runId"],
    ),
    (
        "conversationWorkflowRunNodeResultV4",
        "v4/conversation/workflowRunNodeResult",
        &["sessionId", "runId", "siteId", "ordinal", "maxBytes"],
    ),
];

/// 已保存工作流成员：CLI 方法与透传字段。
pub const WORKFLOW_METHODS: &[(&str, &str, &[&str])] = &[
    ("listSavedWorkflows", "workflows/list", &[]),
    ("getSavedWorkflow", "workflows/get", &["name"]),
    (
        "updateSavedWorkflowMeta",
        "workflows/updateMeta",
        &["name", "meta"],
    ),
    ("deleteSavedWorkflow", "workflows/delete", &["name"]),
    (
        "listSavedWorkflowRuns",
        "workflows/runs",
        &["name", "limit"],
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_method_table_covers_the_management_channel() {
        let members: Vec<&str> = PLUGIN_METHODS.iter().map(|spec| spec.member).collect();
        for member in [
            "listPlugins",
            "getPluginsOverview",
            "addPluginMarketplace",
            "removePluginMarketplace",
            "updatePluginMarketplace",
            "installPlugin",
            "cancelPluginOperation",
            "uninstallPlugin",
            "updatePlugin",
            "restoreBuiltinPlugin",
            "configurePlugin",
            "resetPluginConfig",
            "validatePlugin",
            "describePlugin",
            "setPluginEnabled",
            "resolveSuggestedPluginReference",
        ] {
            assert!(members.contains(&member), "{member}");
        }
    }
}
