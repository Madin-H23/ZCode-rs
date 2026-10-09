//! The process tool runtime behind every workspace view of the tool port
//! (spec rust-cli-architecture, Workspace 作用域): sessions, shells, MCP
//! connections, read state and the write gate are shared; a view only adds its
//! root, configuration and official MCP workspace.
use super::{tool_files::FileState, tool_shell::ShellTasks, tools::WorkspaceTools};
use serde_json::Value;
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::sync::Mutex;

/// The process tool runtime every workspace view uses.
pub(super) struct Shared {
    pub(super) reads: Mutex<HashMap<String, Arc<Mutex<FileState>>>>,
    // File writes from different sessions share one commit gate; reads remain concurrent.
    pub(super) writes: Arc<Mutex<()>>,
    pub(super) shell: ShellTasks,
    pub(super) mcp: super::mcp_hub::Hub,
    pub(super) web: super::web_fetch::WebFetcher,
    /// MCP process notifications until the engine takes them.
    pub(super) process_events:
        std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<(&'static str, Value)>>>,
    /// Owns the shell and MCP tasks (`<parent>/tools`): they are aborted when
    /// the tool runtime is dropped (spec rust-u2-task-scope §4.1).
    _tasks: zcode_async::TaskScope,
}
impl WorkspaceTools {
    /// `tasks` is the scope of the runtime instance the tools serve.
    pub fn new(
        cwd: PathBuf,
        artifacts: PathBuf,
        config: Arc<dyn crate::contract::ConfigSource>,
        egress: Arc<zcode_cli_net::Egress>,
        tasks: &zcode_async::Spawner,
    ) -> Self {
        let tasks = tasks.child("tools");
        let env = egress.tool_env();
        let (sink, events) = tokio::sync::mpsc::unbounded_channel();
        let mcp = super::mcp_hub::Hub::new(egress.clone(), Some(sink), &tasks.spawner());
        let official = super::official_auth::OfficialAuth::for_workspace(&egress, &cwd);
        Self {
            shared: Arc::new(Shared {
                shell: ShellTasks::new(env.clone(), tasks.spawner()),
                web: super::web_fetch::WebFetcher::new(egress.clone()),
                mcp,
                process_events: std::sync::Mutex::new(Some(events)),
                reads: Mutex::new(HashMap::new()),
                writes: Arc::new(Mutex::new(())),
                _tasks: tasks,
            }),
            env,
            egress,
            official,
            config,
            cwd,
            artifacts,
        }
    }
    /// A view of the same runtime for `workspace`, rooted at `cwd` with its
    /// own layered configuration.
    pub fn scoped(
        &self,
        workspace: &crate::contract::Workspace,
        cwd: PathBuf,
        config: Arc<dyn crate::contract::ConfigSource>,
    ) -> Self {
        let mut reference =
            serde_json::json!({"workspaceKey": workspace.key, "workspacePath": workspace.path});
        if let Some(identity) = workspace.identity() {
            reference["workspaceIdentity"] = identity.into();
        }
        Self {
            official: self
                .official
                .view(reference, cwd.to_string_lossy().into_owned()),
            shared: self.shared.clone(),
            env: self.env.clone(),
            egress: self.egress.clone(),
            artifacts: self.artifacts.clone(),
            config,
            cwd,
        }
    }
}
