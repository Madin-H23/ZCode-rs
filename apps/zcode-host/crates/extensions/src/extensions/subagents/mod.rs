//! `subagents` channel（packages/services/src/subagents/subagentsService.ts、subagentStorage.ts、
//! packages/shared/src/node/subagentMarkdownMigration.ts）与 Host 内部的 `prepareRuntimeState`。
//!
//! 状态文件 `{storage}/v2/agents-state.json` 的读改写在 Host 内经 `queue` 串行，跨进程持
//! `<file>.lock/`；启动 CLI 前（spawn 环境计算阶段）先完成旧 Provider 写法的迁移。
pub mod markdown;
pub mod selection;

use std::collections::{HashMap, HashSet};

use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};

use zcode_host_kit::{Reply, RpcError, Serial, ServiceBuilder, log};
use zcode_kit::js::string::trim as js_trim;

use super::plugin_roots::{OFFICIAL_MARKETPLACE, default_enabled};
use super::{Roots, bind, fsx, json as js, read_cli_config_lenient};
use crate::store::{LockOptions, atomic_write_text, with_file_lock};
use crate::support::collate::locale_compare;
use markdown::Parsed;

pub const CHANNEL: &str = "subagents";
const BUILT_IN_NAMES: [&str; 2] = ["general-purpose", "Explore"];

fn built_in_agents(overrides: &Map<String, Value>) -> Vec<Map<String, Value>> {
    let make = |name: &str, description: &str, color: &str, inject: bool, tools: Vec<&str>| {
        let mut agent = Map::new();
        agent.insert(
            "id".into(),
            json!(markdown::agent_state_id(name, "built-in", "built-in")),
        );
        agent.insert("name".into(), json!(name));
        agent.insert("description".into(), json!(description));
        agent.insert("color".into(), json!(color));
        agent.insert("injectAgentsMd".into(), json!(inject));
        if let Some(selection) = overrides.get(name) {
            agent.insert("modelSelection".into(), selection.clone());
            agent.insert("modelSelectionOverride".into(), selection.clone());
        }
        agent.insert("systemPrompt".into(), json!(""));
        agent.insert("tools".into(), json!(tools));
        agent.insert("path".into(), json!(format!("built-in:{name}")));
        agent.insert("scope".into(), json!("built-in"));
        agent.insert("source".into(), json!("built-in"));
        agent.insert("enabled".into(), json!(true));
        agent.insert("readOnly".into(), json!(true));
        agent
    };
    vec![
        make(
            "general-purpose",
            "General-purpose agent for researching complex questions, searching for code, and executing multi-step tasks.",
            "blue",
            true,
            vec!["*"],
        ),
        make(
            "Explore",
            "Read-only search agent for broad fan-out searches.",
            "cyan",
            false,
            vec![
                "Bash",
                "Glob",
                "Grep",
                "Read",
                "WebFetch",
                "WebSearch",
                "TodoWrite",
            ],
        ),
    ]
}

/// agents-state.json 的读视图（`readAgentStateFile` 之后的结构）。
#[derive(Debug, Clone)]
struct State {
    raw: Map<String, Value>,
    built_in: Map<String, Value>,
    plugin: Map<String, Value>,
    disabled: Vec<String>,
}

impl State {
    fn empty() -> Self {
        Self {
            raw: Map::new(),
            built_in: Map::new(),
            plugin: Map::new(),
            disabled: Vec::new(),
        }
    }

    fn from_text(text: Option<&str>) -> Self {
        let Some(Value::Object(raw)) = text.and_then(|text| js::parse(text).ok()) else {
            return Self::empty();
        };
        let built_in_raw = js::record(raw.get("builtInModelSelectionOverrides"));
        let mut built_in = Map::new();
        for name in ["general-purpose", "Explore"] {
            if let Some(selection) = selection::parse_selection(built_in_raw.get(name)) {
                built_in.insert(name.to_owned(), selection);
            }
        }
        let disabled = match raw.get("disabledAgentIds") {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .filter(|id| !js_trim(id).is_empty())
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        };
        Self {
            plugin: selection::parse_plugin_overrides(
                raw.get("pluginAgentModelSelectionOverrides"),
            ),
            raw,
            built_in,
            disabled,
        }
    }

    /// `{...parsed, builtIn…, plugin…, disabledAgentIds}` 或空状态的落盘形态（无结尾换行）。
    fn to_text(&self) -> String {
        let mut out = self.raw.clone();
        out.insert(
            "builtInModelSelectionOverrides".into(),
            Value::Object(self.built_in.clone()),
        );
        out.insert(
            "pluginAgentModelSelectionOverrides".into(),
            Value::Object(self.plugin.clone()),
        );
        out.insert("disabledAgentIds".into(), json!(self.disabled));
        js::pretty(&Value::Object(out))
    }
}

/// JS `Array.prototype.sort()` 的默认顺序（UTF-16 码元）。
fn sort_default(ids: &mut [String]) {
    ids.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
}

struct InstalledRecord {
    id: String,
    install_path: String,
    name: String,
    scope: Option<String>,
    project_path: Option<String>,
}

#[derive(Clone)]
pub struct SubagentsService {
    roots: Roots,
    /// 状态文件在本进程内的唯一写入方：迁移与读改写在这个 owner 任务里逐个执行
    /// （docs/specs/rust-u2-task-scope.md §6.3）。
    writes: Serial,
}

impl SubagentsService {
    pub fn new(roots: Roots) -> Self {
        // 用户设置与每次启动 CLI 前的导入；排队超过 16 个时提交方等待。
        let writes = Serial::start(&roots.tasks, "subagent-state-writes", 16);
        Self { roots, writes }
    }

    /// 在状态文件的 owner 任务里执行；提交方放弃等待不打断已经开始的读改写。
    async fn serialized<T, Edit, Edited>(&self, edit: Edit) -> Result<T, RpcError>
    where
        Edit: FnOnce(SubagentsService) -> Edited + Send + 'static,
        Edited: std::future::Future<Output = Result<T, RpcError>> + Send,
        T: Send + 'static,
    {
        let service = self.clone();
        self.writes
            .run(async move { edit(service).await })
            .await
            .unwrap_or_else(|stopped| Err(fsx::error(stopped.to_string())))
    }

    /// `resolveZCodeStorageRoot`：`storage.dir`（trim 后非空）或 `~/.zcode`，不追加 `cli`。
    async fn storage_root(&self) -> String {
        let config = read_cli_config_lenient(&self.roots).await;
        let storage = js::record(config.get("storage"));
        let dir = storage
            .get("dir")
            .and_then(Value::as_str)
            .map(|dir| js_trim(dir).to_owned())
            .filter(|dir| !dir.is_empty())
            .unwrap_or_else(|| "~/.zcode".to_owned());
        self.roots.resolve_config_path(&dir)
    }

    pub async fn user_root(&self) -> String {
        fsx::join(&self.storage_root().await, &["agents"])
    }

    async fn state_file(&self) -> String {
        fsx::join(&self.storage_root().await, &["v2", "agents-state.json"])
    }

    /// `migrateSubagentStateFile`：JSON 损坏或文件不存在时不动；其余 IO 错误上抛。
    async fn migrate_state_file(&self) -> Result<(), RpcError> {
        let path = self.state_file().await;
        let result = migrate_file(&path, |original| {
            let Ok(Value::Object(parsed)) = js::parse(original) else {
                return original.to_owned();
            };
            let next = selection::import_state_selections(&parsed);
            if js::stringify(&Value::Object(next.clone())) == js::stringify(&Value::Object(parsed))
            {
                original.to_owned()
            } else {
                js::pretty(&Value::Object(next))
            }
        })
        .await;
        match result {
            Ok(_) => Ok(()),
            Err(error) if error.code_str() == Some("ENOENT") => Ok(()),
            Err(error) => Err(error),
        }
    }

    async fn read_state(&self) -> State {
        if let Err(error) = self.migrate_state_file().await {
            log::warn(format!(
                "subagent state migration failed: {}",
                error.message
            ));
        }
        State::from_text(
            fsx::read_text_opt(&self.state_file().await)
                .await
                .as_deref(),
        )
    }

    /// 持锁读改写状态文件；只在状态文件的 owner 任务里调用（[`Self::serialized`]）。
    async fn update_state(&self, mutate: impl FnOnce(&mut State) + Send) -> Result<(), RpcError> {
        if let Err(error) = self.migrate_state_file().await {
            log::warn(format!(
                "subagent state migration failed: {}",
                error.message
            ));
        }
        let path = self.state_file().await;
        let file = std::path::Path::new(&path);
        let outcome = with_file_lock(file, &LockOptions::default(), async || {
            let text = tokio::fs::read_to_string(file).await.ok();
            let mut state = State::from_text(text.as_deref());
            mutate(&mut state);
            atomic_write_text(file, &state.to_text(), false).await
        })
        .await;
        outcome.map_err(|error| fsx::error(format!("{error:#}")))
    }

    /// Host 启动 CLI 前的一次性存储导入（`prepareRuntimeState`）；失败只记 warn。整段在 owner 任务里：
    /// 每次启动 CLI 都会调用，Markdown 与状态文件的迁移都只有这一个进程内写入方。
    pub async fn prepare_runtime_state(&self) {
        let result = self
            .serialized(|service| async move {
                let root = service.user_root().await;
                for failure in migrate_user_markdown(&root).await {
                    log::warn(format!(
                        "用户 Subagent Markdown 迁移失败，保留原文件 path={} error={}",
                        failure.0, failure.1
                    ));
                }
                service.migrate_state_file().await
            })
            .await;
        if let Err(error) = result {
            log::warn(format!("Subagent 状态文件迁移失败: {}", error.message));
        }
    }

    async fn file_agents(
        &self,
        workspace: &str,
        include_workspace: bool,
        diagnostics: &mut Vec<Value>,
    ) -> Vec<Map<String, Value>> {
        let mut roots = vec![("user", self.user_root().await)];
        if include_workspace {
            roots.push(("workspace", fsx::join(workspace, &[".zcode", "agents"])));
        }
        let mut agents = Vec::new();
        for (scope, root) in roots {
            for path in collect_markdown(&root).await {
                let Some(content) = fsx::read_text_opt(&path).await else {
                    diagnostics.push(json!({
                        "code": "agent_read_failed",
                        "message": format!("Failed to read agent Markdown: {path}"),
                        "path": path,
                    }));
                    continue;
                };
                match markdown::parse(&content, &path, scope) {
                    Parsed::Diagnostic(diagnostic) => diagnostics.push(diagnostic),
                    Parsed::Agent(mut agent) => {
                        if scope == "workspace" {
                            agent.insert("projectPath".into(), json!(workspace));
                        }
                        agents.push(agent);
                    }
                }
            }
        }
        agents
    }

    async fn plugin_records(&self) -> Vec<InstalledRecord> {
        let storage = self.storage_root().await;
        let cli_root = if fsx::basename(&storage) == "cli" {
            storage
        } else {
            fsx::join(&storage, &["cli"])
        };
        let plugin_root = fsx::join(&cli_root, &["plugins"]);
        let config = read_cli_config_lenient(&self.roots).await;
        let plugins = js::record(config.get("plugins"));
        let enabled: HashMap<String, bool> = js::boolean_record(plugins.get("enabledPlugins"))
            .into_iter()
            .collect();
        let suppressed = js::string_array(plugins.get("suppressedBuiltins"));
        let installed = read_installed_records(&plugin_root).await;
        let official_suffix = format!("@{OFFICIAL_MARKETPLACE}");
        let mut seen: HashSet<String> = installed.iter().map(|record| record.id.clone()).collect();
        let mut records: Vec<InstalledRecord> = installed
            .into_iter()
            .filter(|record| {
                enabled.get(&record.id) == Some(&true)
                    && !(record.id.ends_with(&official_suffix) && suppressed.contains(&record.id))
            })
            .collect();
        // 内置插件没有安装记录：缓存只补缺失身份，不能复活禁用 / 卸载的插件。
        for (name, versions) in scan_official_cache(&plugin_root).await {
            let id = format!("{name}{official_suffix}");
            if seen.contains(&id) || suppressed.contains(&id) {
                continue;
            }
            if !enabled
                .get(&id)
                .copied()
                .unwrap_or_else(|| default_enabled(&id))
            {
                continue;
            }
            for version in versions {
                let manifest = read_manifest(&version).await;
                if manifest
                    .as_ref()
                    .and_then(|m| m.get("name"))
                    .and_then(Value::as_str)
                    != Some(name.as_str())
                {
                    continue;
                }
                records.push(InstalledRecord {
                    id: id.clone(),
                    install_path: version,
                    name: name.clone(),
                    scope: Some("user".into()),
                    project_path: None,
                });
                seen.insert(id.clone());
                break;
            }
        }
        records
    }

    async fn plugin_agents(
        &self,
        state: &State,
        reserved: &[String],
        diagnostics: &mut Vec<Value>,
    ) -> (Vec<Map<String, Value>>, Vec<Map<String, Value>>) {
        let mut parsed: Vec<(Map<String, Value>, String, String)> = Vec::new();
        for record in self.plugin_records().await {
            let workspace_scope = record.scope.as_deref() == Some("workspace");
            for path in plugin_agent_paths(&record.install_path).await {
                let Some(content) = fsx::read_text_opt(&path).await else {
                    diagnostics.push(json!({
                        "code": "agent_read_failed",
                        "message": format!("Failed to read plugin agent Markdown: {path}"),
                        "path": path,
                    }));
                    continue;
                };
                match markdown::parse(
                    &content,
                    &path,
                    if workspace_scope { "workspace" } else { "user" },
                ) {
                    Parsed::Diagnostic(diagnostic) => diagnostics.push(diagnostic),
                    Parsed::Agent(mut agent) => {
                        let bare = agent["name"].as_str().unwrap_or("").to_owned();
                        let id = markdown::plugin_agent_state_id(&record.id, &bare);
                        agent.insert("id".into(), json!(id));
                        agent.insert("name".into(), json!(format!("{}:{bare}", record.name)));
                        agent.insert("readOnly".into(), json!(true));
                        agent.insert("source".into(), json!("plugin"));
                        agent.insert("pluginId".into(), json!(record.id));
                        agent.insert("pluginName".into(), json!(record.name));
                        if workspace_scope && let Some(project) = &record.project_path {
                            agent.insert("projectPath".into(), json!(project));
                        }
                        // 插件文件只提供默认值；覆盖替换整份选择。
                        if let Some(default) = agent.get("modelSelection").cloned() {
                            agent.insert("defaultModelSelection".into(), default);
                        }
                        if let Some(selection) = state.plugin.get(&id) {
                            agent.insert("modelSelection".into(), selection.clone());
                            agent.insert("modelSelectionOverride".into(), selection.clone());
                        }
                        parsed.push((agent, bare, record.name.clone()));
                    }
                }
            }
        }
        let reserved: HashSet<&str> = BUILT_IN_NAMES
            .iter()
            .copied()
            .chain(reserved.iter().map(String::as_str))
            .collect();
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for (_, bare, _) in &parsed {
            *counts.entry(bare.as_str()).or_default() += 1;
        }
        let mut profiles = Vec::new();
        let mut runtime = Vec::new();
        for (agent, bare, plugin_name) in &parsed {
            profiles.push(agent.clone());
            runtime.push(agent.clone());
            if counts.get(bare.as_str()) == Some(&1) && !reserved.contains(bare.as_str()) {
                let mut alias = agent.clone();
                let id = agent["id"].as_str().unwrap_or("");
                alias.insert(
                    "id".into(),
                    json!(markdown::plugin_agent_state_id(
                        &format!("{id}:alias"),
                        bare
                    )),
                );
                alias.insert("name".into(), json!(bare));
                runtime.push(alias);
            } else if reserved.contains(bare.as_str()) {
                diagnostics.push(json!({
                    "code": "agent_ambiguous_name",
                    "message": format!("Plugin agent bare name conflicts with an existing profile; use {plugin_name}:{bare}"),
                    "path": agent["path"],
                }));
            } else if counts.get(bare.as_str()).copied().unwrap_or(0) > 1 {
                diagnostics.push(json!({
                    "code": "agent_ambiguous_name",
                    "message": format!("Plugin agent bare name is ambiguous; use {plugin_name}:{bare}"),
                    "path": agent["path"],
                }));
            }
        }
        let by_name = |list: &mut Vec<Map<String, Value>>| {
            list.sort_by(|left, right| {
                locale_compare(
                    left["name"].as_str().unwrap_or(""),
                    right["name"].as_str().unwrap_or(""),
                )
            });
        };
        by_name(&mut profiles);
        by_name(&mut runtime);
        (profiles, runtime)
    }

    pub async fn list(&self, params: Value) -> Result<Value, RpcError> {
        let workspace = params
            .get("workspacePath")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let mode = params
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("allRuntimeScopes")
            .to_owned();
        let mut diagnostics = Vec::new();
        for (path, _) in migrate_user_markdown(&self.user_root().await).await {
            diagnostics.push(json!({
                "code": "agent_read_failed",
                "message": "Subagent Markdown migration failed; original file preserved",
                "path": path,
            }));
        }
        let state = self.read_state().await;
        let built_ins = built_in_agents(&state.built_in);
        let files = self
            .file_agents(&workspace, mode == "allRuntimeScopes", &mut diagnostics)
            .await;
        let sorted = |scope: &str| {
            let mut list: Vec<Map<String, Value>> = files
                .iter()
                .filter(|agent| agent["scope"] == scope)
                .cloned()
                .collect();
            list.sort_by(|left, right| {
                locale_compare(
                    left["name"].as_str().unwrap_or(""),
                    right["name"].as_str().unwrap_or(""),
                )
            });
            list
        };
        let user = sorted("user");
        let workspace_agents = sorted("workspace");
        let name_of = |agent: &Map<String, Value>| agent["name"].as_str().unwrap_or("").to_owned();
        let reserved: Vec<String> = built_ins
            .iter()
            .chain(&user)
            .chain(&workspace_agents)
            .map(name_of)
            .collect();
        let (profiles, runtime) = self
            .plugin_agents(&state, &reserved, &mut diagnostics)
            .await;
        let discovered: Vec<Map<String, Value>> = if mode == "settingsUserOnly" {
            built_ins.into_iter().chain(user).collect()
        } else {
            // 运行时同名后者覆盖前者，位置保持首次出现处（Map.set 语义）。
            let mut ordered: Vec<(String, Map<String, Value>)> = Vec::new();
            for agent in built_ins
                .into_iter()
                .chain(user)
                .chain(workspace_agents)
                .chain(runtime)
            {
                let name = name_of(&agent);
                match ordered.iter_mut().find(|(existing, _)| *existing == name) {
                    Some(slot) => slot.1 = agent,
                    None => ordered.push((name, agent)),
                }
            }
            ordered.into_iter().map(|(_, agent)| agent).collect()
        };
        let disabled: HashSet<&str> = state.disabled.iter().map(String::as_str).collect();
        let attach = |agents: Vec<Map<String, Value>>| -> Vec<Value> {
            agents
                .into_iter()
                .map(|mut agent| {
                    let enabled = agent["scope"] != "user"
                        || !disabled.contains(agent["id"].as_str().unwrap_or(""));
                    agent.insert("enabled".into(), json!(enabled));
                    Value::Object(agent)
                })
                .collect()
        };
        let agents = attach(discovered);
        let user_agents: Vec<Value> = agents
            .iter()
            .filter(|agent| agent["source"] == "user")
            .cloned()
            .collect();
        Ok(json!({
            "agents": agents,
            "userAgents": user_agents,
            "pluginAgents": attach(profiles),
            "capability": { "userScopeAvailable": true },
            "diagnostics": diagnostics,
        }))
    }

    pub async fn set_enabled(&self, params: Value) -> Result<(), RpcError> {
        let id = params
            .get("agentId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let enabled = params
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        self.serialized(move |service| async move {
            service
                .update_state(move |state| {
                    let mut set: Vec<String> = state.disabled.clone();
                    set.dedup();
                    let mut unique: Vec<String> = Vec::new();
                    for item in set {
                        if !unique.contains(&item) {
                            unique.push(item);
                        }
                    }
                    if enabled {
                        unique.retain(|item| *item != id);
                    } else if !unique.contains(&id) {
                        unique.push(id);
                    }
                    sort_default(&mut unique);
                    state.disabled = unique;
                })
                .await
        })
        .await
    }

    pub async fn set_built_in_override(&self, params: Value) -> Result<(), RpcError> {
        let name = params
            .get("agentName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let selection =
            selection::normalize_selection(params.get("modelSelection")).map_err(fsx::error)?;
        self.serialized(move |service| async move {
            service
                .update_state(move |state| match selection {
                    Some(selection) => {
                        state.built_in.insert(name, selection);
                    }
                    None => {
                        js::delete(&mut state.built_in, &name);
                    }
                })
                .await
        })
        .await
    }

    pub async fn set_plugin_override(&self, params: Value) -> Result<(), RpcError> {
        let id = params
            .get("agentId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if !id.starts_with("plugin:") || js_trim(&id) != id {
            return Err(fsx::error("无效插件 Subagent 身份"));
        }
        let selection =
            selection::normalize_selection(params.get("modelSelection")).map_err(fsx::error)?;
        self.serialized(move |service| async move {
            service
                .update_state(move |state| match selection {
                    Some(selection) => {
                        state.plugin.insert(id, selection);
                    }
                    None => {
                        js::delete(&mut state.plugin, &id);
                    }
                })
                .await
        })
        .await
    }

    pub async fn primary_user_directory(&self) -> Result<Value, RpcError> {
        let path = self.user_root().await;
        fsx::mkdirp(&path).await?;
        Ok(json!({ "path": path }))
    }

    async fn agent_dir(
        &self,
        params: &Value,
    ) -> Result<(String, String, Option<String>), RpcError> {
        let scope = params
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("user")
            .to_owned();
        let workspace = params
            .get("workspacePath")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if scope == "workspace" {
            let trimmed = workspace.as_deref().map(js_trim).unwrap_or("");
            if trimmed.is_empty() {
                return Err(fsx::error(
                    "Workspace path is required for workspace subagents",
                ));
            }
            return Ok((fsx::join(trimmed, &[".zcode", "agents"]), scope, workspace));
        }
        Ok((self.user_root().await, scope, workspace))
    }

    fn validated_config(params: &Value) -> Result<Map<String, Value>, RpcError> {
        let config = params
            .get("config")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let field = |key: &str| {
            config
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned()
        };
        let name = js_trim(&field("name")).to_owned();
        let length = name.encode_utf16().count();
        if !(3..=50).contains(&length) {
            return Err(fsx::error("Name must be between 3 and 50 characters"));
        }
        if !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(fsx::error(
                "Name can only contain letters, numbers, and hyphens",
            ));
        }
        if js_trim(&field("description")).is_empty() {
            return Err(fsx::error("Description is required"));
        }
        if js_trim(&field("systemPrompt")).is_empty() {
            return Err(fsx::error("System prompt is required"));
        }
        if BUILT_IN_NAMES.contains(&name.as_str()) {
            return Err(fsx::error(format!(
                "Agent name \"{name}\" is reserved by a built-in agent"
            )));
        }
        let mut normalized = config.clone();
        normalized.insert("name".into(), json!(name));
        normalized.insert("description".into(), json!(js_trim(&field("description"))));
        normalized.insert(
            "systemPrompt".into(),
            json!(js_trim(&field("systemPrompt"))),
        );
        match selection::normalize_selection(config.get("modelSelection")).map_err(fsx::error)? {
            Some(selection) => {
                normalized.insert("modelSelection".into(), selection);
            }
            None => {
                js::delete(&mut normalized, "modelSelection");
            }
        }
        Ok(normalized)
    }

    fn saved_agent(
        content: &str,
        path: &str,
        scope: &str,
        workspace: Option<&str>,
    ) -> Result<Value, RpcError> {
        match markdown::parse(content, path, scope) {
            Parsed::Agent(mut agent) => {
                if scope == "workspace"
                    && let Some(workspace) = workspace
                {
                    agent.insert("projectPath".into(), json!(workspace));
                }
                Ok(Value::Object(agent))
            }
            Parsed::Diagnostic(diagnostic) => Err(fsx::error(
                diagnostic["message"]
                    .as_str()
                    .unwrap_or("Failed to parse saved agent")
                    .to_owned(),
            )),
        }
    }

    pub async fn create_agent(&self, params: Value) -> Result<Value, RpcError> {
        let config = Self::validated_config(&params)?;
        let (dir, scope, workspace) = self.agent_dir(&params).await?;
        let file_name = format!(
            "{}.md",
            config["name"].as_str().unwrap_or("").to_lowercase()
        );
        let path = fsx::join(&dir, &[&file_name]);
        fsx::mkdirp(&dir).await?;
        if fsx::exists(&path).await {
            return Err(fsx::error(format!(
                "Agent file \"{file_name}\" already exists"
            )));
        }
        let content = markdown::serialize(&config);
        // 先验证即将写入的 Markdown 可解析，避免 serializer 回归时把坏 profile 落盘。
        let agent = Self::saved_agent(&content, &path, &scope, workspace.as_deref())?;
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = options
            .open(&path)
            .await
            .map_err(|error| fsx::fs_error(&error, "open", &path))?;
        tokio::io::AsyncWriteExt::write_all(&mut file, content.as_bytes())
            .await
            .map_err(|error| fsx::fs_error(&error, "write", &path))?;
        Ok(json!({ "agent": agent }))
    }

    pub async fn update_agent(&self, params: Value) -> Result<Value, RpcError> {
        let config = Self::validated_config(&params)?;
        let (dir, scope, workspace) = self.agent_dir(&params).await?;
        let file_name = format!(
            "{}.md",
            config["name"].as_str().unwrap_or("").to_lowercase()
        );
        let path = fsx::join(&dir, &[&file_name]);
        fsx::mkdirp(&dir).await?;
        let old = params
            .get("oldFilePath")
            .and_then(Value::as_str)
            .filter(|old| !old.is_empty())
            .map(str::to_owned);
        if old.as_deref().is_some_and(|old| old != path) && fsx::exists(&path).await {
            return Err(fsx::error(format!(
                "Agent file \"{file_name}\" already exists"
            )));
        }
        let content = markdown::serialize(&config);
        let agent = Self::saved_agent(&content, &path, &scope, workspace.as_deref())?;
        tokio::fs::write(&path, &content)
            .await
            .map_err(|error| fsx::fs_error(&error, "open", &path))?;
        // 禁用状态按 agent id 存储；改名会生成新 id，必须迁移，否则禁用的 agent 改名后重新启用。
        let previous = params
            .get("agentId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let next = agent["id"].as_str().unwrap_or("").to_owned();
        if previous != next && self.read_state().await.disabled.contains(&previous) {
            self.update_state(move |state| {
                let mut ids: Vec<String> = Vec::new();
                for id in state
                    .disabled
                    .iter()
                    .filter(|id| **id != previous)
                    .cloned()
                    .chain(std::iter::once(next))
                {
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
                sort_default(&mut ids);
                state.disabled = ids;
            })
            .await?;
        }
        if let Some(old) = old.filter(|old| *old != path) {
            let _ = tokio::fs::remove_file(&old).await;
        }
        Ok(json!({ "agent": agent }))
    }

    pub async fn delete_agent(&self, params: Value) -> Result<(), RpcError> {
        let path = params
            .get("filePath")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let id = params
            .get("agentId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(fsx::fs_error(&error, "rm", &path)),
        }
        self.update_state(move |state| {
            let mut ids: Vec<String> = Vec::new();
            for item in state.disabled.iter().filter(|item| **item != id) {
                if !ids.contains(item) {
                    ids.push(item.clone());
                }
            }
            sort_default(&mut ids);
            state.disabled = ids;
        })
        .await
    }
}

/// 递归收集 `.md` / `.markdown`（不跟随根目录软链），按 localeCompare 排序。
fn collect_markdown(root: &str) -> BoxFuture<'_, Vec<String>> {
    Box::pin(async move {
        if !fsx::exists(root).await {
            return Vec::new();
        }
        if !tokio::fs::symlink_metadata(root)
            .await
            .is_ok_and(|meta| meta.is_dir())
        {
            return Vec::new();
        }
        let Ok(entries) = fsx::read_dir(root).await else {
            return Vec::new();
        };
        let mut result = Vec::new();
        for entry in entries {
            let path = fsx::join(root, &[&entry.name]);
            if entry.is_dir {
                result.extend(collect_markdown(&path).await);
                continue;
            }
            let lower = entry.name.to_lowercase();
            if entry.is_file && (lower.ends_with(".md") || lower.ends_with(".markdown")) {
                result.push(path);
            }
        }
        result.sort_by(|left, right| locale_compare(left, right));
        result
    })
}

async fn read_manifest(root: &str) -> Option<Map<String, Value>> {
    for segments in [
        [".zcode-plugin", "plugin.json"],
        [".claude-plugin", "plugin.json"],
        [".codex-plugin", "plugin.json"],
    ] {
        if let Some(manifest) = fsx::read_text_opt(&fsx::join(root, &segments))
            .await
            .and_then(|raw| js::parse_record(&raw))
        {
            return Some(manifest);
        }
    }
    None
}

async fn plugin_agent_paths(root: &str) -> Vec<String> {
    let manifest = read_manifest(root).await;
    let base = fsx::resolve(root, &[]);
    let mut roots: Vec<String> = Vec::new();
    let mut add = |relative: &str| {
        let resolved = fsx::resolve(&base, &[relative.strip_prefix("./").unwrap_or(relative)]);
        let relative_to_root = fsx::relative(&base, &resolved);
        if relative_to_root.starts_with("..") || fsx::is_absolute(&relative_to_root) {
            return;
        }
        if !roots.contains(&resolved) {
            roots.push(resolved);
        }
    };
    add("agents");
    match manifest
        .as_ref()
        .and_then(|manifest| manifest.get("agents"))
    {
        Some(Value::String(path)) => add(path),
        Some(Value::Array(items)) => {
            for item in items.iter().filter_map(Value::as_str) {
                add(item);
            }
        }
        _ => {}
    }
    let mut paths = Vec::new();
    for root in roots {
        paths.extend(collect_markdown(&root).await);
    }
    paths.sort_by(|left, right| locale_compare(left, right));
    paths
}

async fn read_installed_records(plugin_root: &str) -> Vec<InstalledRecord> {
    let Some(parsed) = fsx::read_text_opt(&fsx::join(plugin_root, &["installed_plugins.json"]))
        .await
        .and_then(|raw| js::parse(&raw).ok())
    else {
        return Vec::new();
    };
    let Some(items) = parsed.get("plugins").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let item = item.as_object()?;
            let text = |key: &str| item.get(key).and_then(Value::as_str).map(str::to_owned);
            Some(InstalledRecord {
                id: text("id")?,
                install_path: text("installPath")?,
                name: text("name")?,
                scope: text("scope"),
                project_path: text("projectPath"),
            })
        })
        .collect()
}

/// `scanOfficialPluginCacheRoots`（shared/node）：版本目录跳过备份 / seed 锁 / 临时目录，按数字感知降序。
async fn scan_official_cache(plugin_root: &str) -> Vec<(String, Vec<String>)> {
    let cache = fsx::join(plugin_root, &["cache", OFFICIAL_MARKETPLACE]);
    let Ok(plugins) = fsx::read_dir(&cache).await else {
        return Vec::new();
    };
    let mut roots = Vec::new();
    for plugin in plugins.into_iter().filter(|entry| entry.is_dir) {
        let dir = fsx::join(&cache, &[&plugin.name]);
        let Ok(versions) = fsx::read_dir(&dir).await else {
            continue;
        };
        let mut names: Vec<String> = versions
            .into_iter()
            .filter(|entry| {
                entry.is_dir
                    && !entry.name.contains(".backup")
                    && !entry.name.contains(".seed-lock")
                    && !entry.name.contains(".tmp-")
            })
            .map(|entry| entry.name)
            .collect();
        names.sort_by(|left, right| numeric_compare(right, left));
        if !names.is_empty() {
            roots.push((
                plugin.name.clone(),
                names.iter().map(|name| fsx::join(&dir, &[name])).collect(),
            ));
        }
    }
    roots.sort_by(|left, right| locale_compare(&left.0, &right.0));
    roots
}

/// `localeCompare(…, { numeric: true, sensitivity: "base" })` 的近似：数字段按数值比较，其余不分大小写。
fn numeric_compare(left: &str, right: &str) -> std::cmp::Ordering {
    let chunks = |text: &str| {
        let mut parts: Vec<(bool, String)> = Vec::new();
        for c in text.chars() {
            let digit = c.is_ascii_digit();
            match parts.last_mut() {
                Some((is_digit, part)) if *is_digit == digit => part.push(c),
                _ => parts.push((digit, c.to_string())),
            }
        }
        parts
    };
    let (a, b) = (chunks(left), chunks(right));
    for ((a_digit, a_part), (b_digit, b_part)) in a.iter().zip(&b) {
        let ordering = if *a_digit && *b_digit {
            let trim = |s: &str| s.trim_start_matches('0').to_owned();
            let (x, y) = (trim(a_part), trim(b_part));
            x.len().cmp(&y.len()).then_with(|| x.cmp(&y))
        } else {
            locale_compare(&a_part.to_lowercase(), &b_part.to_lowercase())
        };
        if ordering != std::cmp::Ordering::Equal {
            return ordering;
        }
    }
    a.len().cmp(&b.len())
}

/// `migrateFile`：持锁、写前比较原文与文件身份（外部编辑器不持应用锁），不能覆盖并发修改。
async fn migrate_file(
    path: &str,
    transform: impl FnOnce(&str) -> String + Send,
) -> Result<bool, RpcError> {
    let meta = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|error| fsx::fs_error(&error, "lstat", path))?;
    if !meta.is_file() {
        return Ok(false);
    }
    let file = std::path::Path::new(path);
    let result = with_file_lock(file, &LockOptions::default(), async || {
        Ok(migrate_locked(path, transform).await)
    })
    .await;
    match result {
        Ok(inner) => inner,
        Err(error) => Err(fsx::error(format!("{error:#}"))),
    }
}

async fn migrate_locked(
    path: &str,
    transform: impl FnOnce(&str) -> String,
) -> Result<bool, RpcError> {
    let before = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|error| fsx::fs_error(&error, "lstat", path))?;
    let mode = file_mode(&before);
    if !before.is_file() || mode & 0o222 == 0 {
        return Ok(false);
    }
    let original = tokio::fs::read_to_string(path)
        .await
        .map_err(|error| fsx::fs_error(&error, "open", path))?;
    let next = transform(&original);
    if next == original {
        return Ok(false);
    }
    let directory = fsx::dirname(path);
    let physical = fsx::realpath(&directory)
        .await
        .map_err(|error| fsx::fs_error(&error, "realpath", &directory))?;
    let temp = fsx::join(
        &directory,
        &[&format!(
            ".{}.{}.tmp",
            fsx::basename(path),
            fsx::random_uuid()
        )],
    );
    let outcome: Result<bool, RpcError> = async {
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(mode & 0o777);
        let mut handle = options
            .open(&temp)
            .await
            .map_err(|error| fsx::fs_error(&error, "open", &temp))?;
        tokio::io::AsyncWriteExt::write_all(&mut handle, next.as_bytes())
            .await
            .map_err(|error| fsx::fs_error(&error, "write", &temp))?;
        drop(handle);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&temp, std::fs::Permissions::from_mode(mode & 0o777))
                .await
                .map_err(|error| fsx::fs_error(&error, "chmod", &temp))?;
        }
        let current = tokio::fs::symlink_metadata(path)
            .await
            .map_err(|error| fsx::fs_error(&error, "lstat", path))?;
        let unchanged = current.is_file()
            && same_identity(&current, &before)
            && fsx::realpath(&directory).await.ok().as_deref() == Some(physical.as_str())
            && tokio::fs::read_to_string(path).await.ok().as_deref() == Some(original.as_str());
        if !unchanged {
            return Err(fsx::error("Subagent file changed during migration"));
        }
        tokio::fs::rename(&temp, path)
            .await
            .map_err(|error| fsx::fs_error(&error, "rename", &temp))?;
        Ok(true)
    }
    .await;
    let _ = tokio::fs::remove_file(&temp).await;
    outcome
}

#[cfg(unix)]
fn file_mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode()
}

#[cfg(not(unix))]
fn file_mode(meta: &std::fs::Metadata) -> u32 {
    if meta.permissions().readonly() {
        0o444
    } else {
        0o666
    }
}

#[cfg(unix)]
fn same_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.ino() == right.ino() && left.dev() == right.dev()
}

#[cfg(not(unix))]
fn same_identity(_left: &std::fs::Metadata, _right: &std::fs::Metadata) -> bool {
    true
}

/// `migrateUserSubagentMarkdown`：只处理用户 agents 根下的普通文件，不跟随软链。返回失败列表。
fn migrate_user_markdown(root: &str) -> BoxFuture<'_, Vec<(String, String)>> {
    Box::pin(async move {
        let mut failures = Vec::new();
        match tokio::fs::symlink_metadata(root).await {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return failures,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return failures,
            Err(error) => {
                failures.push((root.to_owned(), fsx::fs_message(&error, "lstat", root)));
                return failures;
            }
        }
        let entries = match fsx::read_dir(root).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return failures,
            Err(error) => {
                failures.push((root.to_owned(), fsx::fs_message(&error, "scandir", root)));
                return failures;
            }
        };
        for entry in entries {
            let path = fsx::join(root, &[&entry.name]);
            if entry.is_dir {
                failures.extend(migrate_user_markdown(&path).await);
                continue;
            }
            let lower = entry.name.to_lowercase();
            if entry.is_file
                && (lower.ends_with(".md") || lower.ends_with(".markdown"))
                && let Err(error) = migrate_file(&path, selection::migrate_markdown_provider).await
            {
                failures.push((path, error.message));
            }
        }
        failures
    })
}

pub fn service(subagents: SubagentsService) -> ServiceBuilder {
    let builder = ServiceBuilder::new(CHANNEL);
    let builder = bind(builder, "list", &subagents, |s, p| async move {
        s.list(p).await.map(Reply::Value)
    });
    let builder = bind(builder, "setEnabled", &subagents, |s, p| async move {
        s.set_enabled(p).await.map(|()| Reply::Undefined)
    });
    let builder = bind(
        builder,
        "setBuiltInModelOverride",
        &subagents,
        |s, p| async move { s.set_built_in_override(p).await.map(|()| Reply::Undefined) },
    );
    let builder = bind(
        builder,
        "setPluginAgentModelOverride",
        &subagents,
        |s, p| async move { s.set_plugin_override(p).await.map(|()| Reply::Undefined) },
    );
    let builder = bind(
        builder,
        "getPrimaryUserAgentsDirectory",
        &subagents,
        |s, _| async move { s.primary_user_directory().await.map(Reply::Value) },
    );
    let builder = bind(builder, "createAgent", &subagents, |s, p| async move {
        s.create_agent(p).await.map(Reply::Value)
    });
    let builder = bind(builder, "updateAgent", &subagents, |s, p| async move {
        s.update_agent(p).await.map(Reply::Value)
    });
    bind(builder, "deleteAgent", &subagents, |s, p| async move {
        s.delete_agent(p).await.map(|()| Reply::Undefined)
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_list_toggle_and_migrate() {
        let home = tempfile::tempdir().unwrap();
        let service = SubagentsService::new(Roots::new(
            &zcode_host_base::testing::spawner(),
            home.path(),
            home.path(),
        ));
        let created = service
            .create_agent(json!({"config":{"name":"Reviewer","description":"d","systemPrompt":"p"},"provider":"glm"}))
            .await
            .unwrap();
        assert_eq!(created["agent"]["id"], "user:user:reviewer");
        assert!(home.path().join(".zcode/agents/reviewer.md").exists());
        service
            .set_enabled(json!({"agentId":"user:user:reviewer","enabled":false}))
            .await
            .unwrap();
        let listed = service
            .list(json!({"workspacePath": home.path().join("ws").display().to_string()}))
            .await
            .unwrap();
        let names: Vec<&str> = listed["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["general-purpose", "Explore", "Reviewer"]);
        assert_eq!(listed["userAgents"][0]["enabled"], false, "{listed}");
        let state =
            std::fs::read_to_string(home.path().join(".zcode/v2/agents-state.json")).unwrap();
        assert_eq!(
            state,
            "{\n  \"builtInModelSelectionOverrides\": {},\n  \"pluginAgentModelSelectionOverrides\": {},\n  \"disabledAgentIds\": [\n    \"user:user:reviewer\"\n  ]\n}"
        );
        // 旧 Provider 写法在启动前迁移。
        std::fs::write(
            home.path().join(".zcode/agents/legacy.md"),
            "---\nname: legacy\ndescription: d\nmodel: builtin:zai/glm-4.6\n---\nx\n",
        )
        .unwrap();
        std::fs::write(
            home.path().join(".zcode/v2/agents-state.json"),
            r#"{"builtInModelOverrides":{"Explore":"builtin:bigmodel/glm-5"}}"#,
        )
        .unwrap();
        service.prepare_runtime_state().await;
        assert!(
            std::fs::read_to_string(home.path().join(".zcode/agents/legacy.md"))
                .unwrap()
                .contains("model: zai-api/GLM-4.6")
        );
        let migrated: Value = serde_json::from_str(
            &std::fs::read_to_string(home.path().join(".zcode/v2/agents-state.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            migrated["builtInModelSelectionOverrides"]["Explore"]["providerId"],
            "bigmodel-api"
        );
        assert!(
            service
                .set_plugin_override(json!({"agentId":"user:x"}))
                .await
                .is_err()
        );
        assert!(
            service
                .create_agent(
                    json!({"config":{"name":"Explore","description":"d","systemPrompt":"p"}})
                )
                .await
                .is_err()
        );
    }
}
