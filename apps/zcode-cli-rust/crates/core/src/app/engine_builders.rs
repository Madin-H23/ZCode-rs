//! Startup options of the engine.
use super::Engine;
use serde_json::Value;
use std::sync::Arc;

impl Engine {
    /// Config file `permission` section (mode fallback, allowed/disallowed tools)
    /// of the startup workspace, read once at startup like Node's app creation.
    pub fn with_permission_config(mut self, permission: &Value) -> Self {
        let project = &mut self.default_scope_mut().permissions;
        project.config = crate::domain::permission::Config::from_config(permission);
        project.config_mode = permission["mode"].as_str().map(str::to_owned);
        self
    }
    pub fn with_registry(
        mut self,
        registry: Option<Arc<dyn crate::contract::ModelRegistry>>,
    ) -> Self {
        self.registry = registry;
        if let Some(registry) = &self.registry {
            self.config = registry.default_selection();
        }
        self
    }
    /// Serves workspaces other than the startup one, opened through `source`
    /// (spec rust-cli-architecture, Workspace 作用域).
    pub fn with_workspaces(mut self, source: Arc<dyn crate::contract::WorkspaceSource>) -> Self {
        self.workspaces = Some(source);
        self
    }
    /// Node protocol sessions generate their title after the first input
    /// (`titleGeneration: {}` of `createRecord`); `-p` and embedders do not.
    pub fn with_title_generation(mut self, enabled: bool) -> Self {
        self.titles = enabled;
        self
    }
}
