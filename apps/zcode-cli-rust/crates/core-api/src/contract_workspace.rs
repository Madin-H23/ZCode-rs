//! Ports bound to one workspace: its layered configuration, prompt context and
//! the source that opens them for request workspaces (spec rust-cli-architecture,
//! Workspace 作用域).
use crate::contract::ToolPort;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use zcode_cli_domain::config::ConfigSnapshot;

/// Layered zcode configuration for one workspace. Implementations reload files on
/// every call (Node reloads at each entry point); consumers only read snapshots.
#[async_trait]
pub trait ConfigSource: Send + Sync {
    async fn load(&self) -> Result<Arc<ConfigSnapshot>>;
    /// Switches one project hook in `<cwd>/.zcode/config.json` if its
    /// declaration still has the reviewed digest. `Err` is a reason code.
    async fn set_workspace_hook_enabled(
        &self,
        _toggle: HookToggle,
    ) -> std::result::Result<(), &'static str> {
        Err("workspace_hooks_config_write_failed")
    }
}

/// A reviewed project hook declaration to switch (Node
/// `writeWorkspaceHookConfiguredToggle`).
pub struct HookToggle {
    pub path: String,
    pub event: zcode_cli_domain::hooks::HookEvent,
    pub relative_path: String,
    pub discovery_order: usize,
    pub matcher_index: usize,
    pub hook_index: usize,
    pub digest: String,
    pub resolved_timeout_ms: u64,
    pub resolved_max_output_bytes: u64,
    pub enabled: bool,
}

#[async_trait]
pub trait ContextPort: Send + Sync {
    fn desktop(&self) -> bool;
    async fn snapshot(
        &self,
        cancel: &CancellationToken,
    ) -> Result<zcode_cli_domain::prompt::PromptSnapshot>;
    async fn instructions(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Vec<zcode_cli_domain::prompt::InstructionSource>>;
}
pub use zcode_cli_domain::workspace_identity::Workspace;

/// The ports bound to one workspace (spec rust-cli-architecture, Workspace 作用域).
pub struct WorkspacePorts {
    pub context: Arc<dyn ContextPort>,
    pub tools: Arc<dyn ToolPort>,
    /// The workspace's layered configuration (permission, hooks, anomaly guard).
    pub config: Arc<dyn ConfigSource>,
}

/// Opens the ports of workspaces other than the startup one. Every tool port it
/// returns is a view of the startup tool runtime: shells, MCP connections and
/// read state stay keyed by session; only the root, configuration and the
/// official MCP workspace differ.
#[async_trait]
pub trait WorkspaceSource: Send + Sync {
    async fn open(&self, workspace: &Workspace) -> Result<WorkspacePorts>;
}
