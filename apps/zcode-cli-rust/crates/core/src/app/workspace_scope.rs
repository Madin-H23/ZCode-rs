//! Workspace scopes (spec rust-cli-architecture, Workspace 作用域): the engine
//! serves every workspace a request or session names. Each open workspace is
//! one scope that owns its ports, project permissions and topic state; the
//! startup workspace is the default for requests that name none.
use super::Engine;
use crate::contract::{ConfigSource, ContextPort, ToolPort, Workspace, WorkspacePorts};
use crate::domain::{
    model_anomaly::Guard,
    permission::{self as policy, Ruleset},
    topic_log::TopicLog,
};
use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

/// Idle non-default scopes kept for reuse; older ones are dropped.
const IDLE_SCOPES: usize = 8;

/// Everything the engine keeps for one workspace.
pub(super) struct Scope {
    pub workspace: Workspace,
    pub context: Arc<dyn ContextPort>,
    pub tools: Arc<dyn ToolPort>,
    /// Layered configuration: project hook discovery and toggles. `None` for
    /// embedders without one (project hooks then stay off).
    pub config: Option<Arc<dyn ConfigSource>>,
    pub permissions: ProjectPermissions,
    pub anomaly_guard: Guard,
    /// Epoch of the scope's `sessions-index` and `workspace-config` topics; a
    /// reopened scope starts a new one so an old base can only get a snapshot.
    pub epoch: String,
    pub index: BTreeMap<String, Value>,
    pub index_seq: u64,
    pub index_log: TopicLog,
    pub config_seq: u64,
    /// Last use, for dropping the oldest idle scopes.
    pub used: u64,
}

/// Project permission inputs of one workspace (Node `PermissionService`
/// config lists and the project's persisted rules and mode preference).
#[derive(Default)]
pub(super) struct ProjectPermissions {
    pub config: policy::Config,
    /// Config file `permission.mode`, the last fallback for new sessions.
    pub config_mode: Option<String>,
    pub project_rules: Arc<Ruleset>,
    /// Project preference written by `switchCollaborationMode`.
    pub project_mode: Option<String>,
}

impl ProjectPermissions {
    /// Project rules and mode preference from the workspace settings plus the
    /// config file `permission` section.
    pub fn new(settings: &BTreeMap<(String, String), Value>, permission: &Value) -> Self {
        let setting = |key: &str| settings.get(&("permission".to_owned(), key.to_owned()));
        Self {
            config: policy::Config::from_config(permission),
            config_mode: permission["mode"].as_str().map(str::to_owned),
            project_rules: Arc::new(
                setting("ruleset")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default(),
            ),
            project_mode: setting("mode")
                .and_then(|v| v["mode"].as_str())
                .map(str::to_owned),
        }
    }
}

/// The ports a scope opens with; `config` is `None` for embedders without one.
pub(super) struct ScopePorts {
    pub context: Arc<dyn ContextPort>,
    pub tools: Arc<dyn ToolPort>,
    pub config: Option<Arc<dyn ConfigSource>>,
}

impl Scope {
    pub fn new(
        workspace: Workspace,
        ScopePorts {
            context,
            tools,
            config,
        }: ScopePorts,
        permissions: ProjectPermissions,
        index: BTreeMap<String, Value>,
        epoch: String,
    ) -> Self {
        Self {
            workspace,
            context,
            tools,
            config,
            permissions,
            anomaly_guard: Guard::default(),
            epoch,
            index,
            index_seq: 0,
            index_log: TopicLog::index(),
            config_seq: 0,
            used: 0,
        }
    }

    /// The startup workspace's scope: its sessions-index seed and project
    /// settings (the config file part comes from the engine builders).
    pub async fn startup(
        workspace: &Workspace,
        store: &dyn crate::contract::SessionStore,
        ports: ScopePorts,
        epoch: String,
    ) -> Result<Self> {
        let index = store.load_index(&workspace.key).await?;
        let settings = store.project_settings(&workspace.key).await?;
        let permissions = ProjectPermissions::new(&settings, &Value::Null);
        Ok(Self::new(
            workspace.clone(),
            ports,
            permissions,
            index,
            epoch,
        ))
    }
}

impl ScopePorts {
    /// The startup ports; the config source arrives with workspace trust.
    pub fn startup(context: Arc<dyn ContextPort>, tools: Arc<dyn ToolPort>) -> Self {
        Self {
            context,
            tools,
            config: None,
        }
    }
}

impl Engine {
    /// The startup workspace's scope.
    pub(super) fn default_scope(&self) -> &Scope {
        &self.scopes[&self.default_workspace]
    }

    pub(super) fn default_scope_mut(&mut self) -> &mut Scope {
        self.scopes.get_mut(&self.default_workspace).unwrap()
    }

    /// An open scope with the same key keeps its path (a key names one workspace).
    fn known(&self, workspace: Workspace) -> Workspace {
        self.scopes
            .get(&workspace.key)
            .map_or(workspace, |scope| scope.workspace.clone())
    }

    /// The workspace a request's `workspace` field names; the default one
    /// when it names none.
    pub(super) fn request_workspace(&self, p: &Value) -> Result<Workspace> {
        Ok(match Workspace::from_ref(p.get("workspace"))? {
            Some(workspace) => self.known(workspace),
            None => self.default_scope().workspace.clone(),
        })
    }

    /// Only the workspace a request names, if any.
    pub(super) fn named_workspace(&self, p: &Value) -> Result<Option<Workspace>> {
        Ok(Workspace::from_ref(p.get("workspace"))?.map(|w| self.known(w)))
    }

    /// A V4 workspace id (`createSession.workspaceId`, topic ids).
    pub(super) fn workspace_of_id(&self, id: &str) -> Result<Workspace> {
        match self.scopes.get(id) {
            Some(scope) => Ok(scope.workspace.clone()),
            None => Workspace::from_id(id),
        }
    }

    /// The workspace a session belongs to.
    pub(super) fn session_workspace(session: &crate::domain::session::Session) -> Workspace {
        Workspace {
            key: session.workspace.clone(),
            path: session
                .workspace_path
                .clone()
                .unwrap_or_else(|| session.workspace.clone()),
        }
    }

    /// Opens `workspace`'s scope when it is not open yet and marks it used:
    /// one config snapshot, the project settings and the sessions-index seed.
    pub(super) async fn open_scope(&mut self, workspace: &Workspace) -> Result<()> {
        self.scope_clock += 1;
        if let Some(scope) = self.scopes.get_mut(&workspace.key) {
            scope.used = self.scope_clock;
            return Ok(());
        }
        // 未接入 WorkspaceSource 的嵌入方（测试替身）只服务启动 workspace。
        let source = self
            .workspaces
            .clone()
            .ok_or_else(|| anyhow!("Workspace identity mismatch"))?;
        let WorkspacePorts {
            context,
            tools,
            config,
        } = source.open(workspace).await?;
        let snapshot = config.load().await?;
        let settings = self.store.project_settings(&workspace.key).await?;
        let index = self.store.load_index(&workspace.key).await?;
        let permissions = ProjectPermissions::new(&settings, &snapshot.config["permission"]);
        let mut scope = Scope::new(
            workspace.clone(),
            ScopePorts {
                context,
                tools,
                config: Some(config),
            },
            permissions,
            index,
            self.clock.id(),
        );
        scope.anomaly_guard = Guard::from_config(&snapshot.config["modelAnomalyGuard"]);
        scope.used = self.scope_clock;
        tracing::info!(
            target: "zcode::runtime",
            event = "workspace.scope_opened",
            remote = workspace.identity().is_some(),
            "Workspace scope opened"
        );
        self.scopes.insert(workspace.key.clone(), scope);
        Ok(())
    }

    /// The scope of workspace `key`.
    pub(super) fn scope(&self, key: &str) -> Result<&Scope> {
        self.scopes.get(key).context("Workspace scope unavailable")
    }

    pub(super) fn scope_mut(&mut self, key: &str) -> Result<&mut Scope> {
        self.scopes
            .get_mut(key)
            .context("Workspace scope unavailable")
    }

    /// The scope of a resident session (open for as long as the session is).
    pub(super) fn session_scope(&self, id: &str) -> Result<&Scope> {
        let session = self.sessions.get(id).context("Session unavailable")?;
        self.scope(&session.workspace)
    }

    /// The tool port of a resident session's workspace.
    pub(super) fn session_tools(&self, id: &str) -> Result<Arc<dyn ToolPort>> {
        Ok(self.session_scope(id)?.tools.clone())
    }

    /// Drops the oldest idle scopes beyond [`IDLE_SCOPES`]. A scope is idle
    /// without resident sessions and open index or config topics; the default
    /// scope always stays.
    pub(super) fn trim_scopes(&mut self) {
        let busy: std::collections::BTreeSet<&str> = self
            .sessions
            .values()
            .map(|s| s.workspace.as_str())
            .chain(self.interest.keys().filter_map(|topic| {
                topic
                    .strip_prefix("sessions-index/")
                    .or_else(|| topic.strip_prefix("workspace-config/"))
            }))
            .collect();
        let mut idle: Vec<(u64, String)> = self
            .scopes
            .iter()
            .filter(|(key, _)| **key != self.default_workspace && !busy.contains(key.as_str()))
            .map(|(key, scope)| (scope.used, key.clone()))
            .collect();
        if idle.len() <= IDLE_SCOPES {
            return;
        }
        idle.sort();
        for (_, key) in idle.into_iter().rev().skip(IDLE_SCOPES) {
            self.scopes.remove(&key);
        }
    }
}
