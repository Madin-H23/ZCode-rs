//! `mcp-sync` channel（packages/services/src/mcp-sync/mcpSyncService.ts）：用户与 workspace 目录里的
//! MCP 配置读写、远端同步候选，以及经 workspace CLI 的 `mcp/list` 状态查询。
use std::collections::HashMap;

use serde_json::{Map, Value, json};
use zcode_host_kit::{Reply, RpcError, ServiceBuilder, log};

use super::cli_config::Update;
use super::skill_sync::truthy;
use super::{Roots, bind, check_write_access, fsx, json as js};
use crate::agent::{AgentRuntime, RuntimePolicy, WorkspaceTarget};
use crate::support::collate::locale_compare;

pub const CHANNEL: &str = "mcp-sync";
pub const STATUS_MODE_UNSUPPORTED_CODE: &str = "ZCODE_AGENT_MCP_STATUS_MODE_UNSUPPORTED";
const ENABLED_KEY: &str = "enabled";
const LEGACY_ENABLE_KEY: &str = "enable";

#[derive(Clone, Copy, PartialEq, Eq)]
struct Descriptor {
    source: &'static str,
    user_segments: &'static [&'static str],
    workspace_segments: &'static [&'static str],
    file_name: &'static str,
    nested: bool,
}

const ZCODE: Descriptor = Descriptor {
    source: "zcode",
    user_segments: &[".zcode", "cli"],
    workspace_segments: &[".zcode"],
    file_name: "config.json",
    nested: true,
};
const AGENTS: Descriptor = Descriptor {
    source: "agents",
    user_segments: &[".agents"],
    workspace_segments: &[".agents"],
    file_name: "mcp.json",
    nested: false,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    User,
    Workspace,
}

impl Scope {
    fn as_str(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Workspace => "workspace",
        }
    }
}

#[derive(Debug, Clone)]
struct Record {
    name: String,
    config: Value,
    enabled: bool,
    source: &'static str,
    path: String,
}

/// `readJsonObject`：不存在为 `None`；读不了、JSON 损坏或不是对象一律报错（不能当成空对象覆盖）。
fn parse_object(raw: Option<&str>, path: &str) -> Result<Option<Map<String, Value>>, RpcError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    match js::parse(raw) {
        Ok(Value::Object(map)) => Ok(Some(map)),
        Ok(_) => Err(fsx::error(format!("MCP 配置文件 {path} 必须是 JSON 对象"))),
        Err(message) => Err(fsx::error(format!(
            "无法解析 MCP 配置文件 {path}: {message}"
        ))),
    }
}

async fn read_object(path: &str) -> Result<Option<Map<String, Value>>, RpcError> {
    match fsx::read_text(path).await {
        Ok(raw) => parse_object(Some(&raw), path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(fsx::error(format!(
            "无法读取 MCP 配置文件 {path}: {}",
            fsx::fs_message(&error, "open", path)
        ))),
    }
}

fn server_map(parsed: &Map<String, Value>, nested: bool) -> Map<String, Value> {
    if nested {
        let mcp = js::record(parsed.get("mcp"));
        return js::record(mcp.get("servers"));
    }
    js::record(parsed.get("mcpServers"))
}

fn with_server_map(
    current: &Map<String, Value>,
    nested: bool,
    servers: Map<String, Value>,
) -> Map<String, Value> {
    let mut next = current.clone();
    if !nested {
        next.insert("mcpServers".into(), Value::Object(servers));
        return next;
    }
    let mut mcp = js::record(current.get("mcp"));
    mcp.insert("servers".into(), Value::Object(servers));
    next.insert("mcp".into(), Value::Object(mcp));
    next
}

fn server_enabled(config: &Value) -> bool {
    config.get(ENABLED_KEY) != Some(&Value::Bool(false))
}

/// `setServerEnabled`：启用是默认值，不落盘；同时清掉旧的 `enable`。
fn set_server_enabled(config: &Value, enabled: bool) -> Value {
    let mut rest = config.as_object().cloned().unwrap_or_default();
    js::delete(&mut rest, LEGACY_ENABLE_KEY);
    js::delete(&mut rest, ENABLED_KEY);
    if !enabled {
        rest.insert(ENABLED_KEY.into(), json!(false));
    }
    Value::Object(rest)
}

/// `migrateLegacyEnableFlag`：两个字段冲突时以停用为准。
fn migrate_legacy(servers: &Map<String, Value>) -> (Map<String, Value>, bool) {
    let mut changed = false;
    let mut migrated = Map::new();
    for (name, config) in servers {
        let Some(object) = config
            .as_object()
            .filter(|object| object.contains_key(LEGACY_ENABLE_KEY))
        else {
            migrated.insert(name.clone(), config.clone());
            continue;
        };
        changed = true;
        let disabled = object.get(LEGACY_ENABLE_KEY) == Some(&Value::Bool(false))
            || object.get(ENABLED_KEY) == Some(&Value::Bool(false));
        migrated.insert(name.clone(), set_server_enabled(config, !disabled));
    }
    (migrated, changed)
}

/// `removeLegacyMcpEnabledOverride`：清掉旧版按目录写在 `mcp.<目录>` 下的覆盖。
fn remove_legacy_override(
    config: &Map<String, Value>,
    directory: &str,
    name: &str,
) -> Option<Map<String, Value>> {
    let mut mcp = config.get("mcp")?.as_object()?.clone();
    let mut path_config = mcp.get(directory)?.as_object()?.clone();
    js::delete(&mut path_config, name)?;
    if path_config.is_empty() {
        js::delete(&mut mcp, directory);
    } else {
        mcp.insert(directory.to_owned(), Value::Object(path_config));
    }
    let mut next = config.clone();
    next.insert("mcp".into(), Value::Object(mcp));
    Some(next)
}

#[derive(Clone)]
pub struct McpSyncService {
    roots: Roots,
    runtime: Option<AgentRuntime>,
}

impl McpSyncService {
    pub fn new(roots: Roots, runtime: Option<AgentRuntime>) -> Self {
        Self { roots, runtime }
    }

    fn config_path(
        &self,
        descriptor: Descriptor,
        scope: Scope,
        workspace: Option<&str>,
    ) -> Result<String, RpcError> {
        let (base, segments) = match scope {
            Scope::User => (self.roots.home.clone(), descriptor.user_segments),
            Scope::Workspace => (
                workspace
                    .filter(|path| !path.is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        fsx::error(format!(
                            "Missing workspace path for {} workspace MCP config",
                            descriptor.source
                        ))
                    })?,
                descriptor.workspace_segments,
            ),
        };
        let mut parts: Vec<&str> = segments.to_vec();
        parts.push(descriptor.file_name);
        Ok(fsx::join(&base, &parts))
    }

    fn location(
        &self,
        descriptor: Descriptor,
        scope: Scope,
        workspace: Option<&str>,
    ) -> Result<Value, RpcError> {
        let path = self.config_path(descriptor, scope, workspace)?;
        let mut location = Map::new();
        location.insert("source".into(), json!(descriptor.source));
        location.insert(
            "scope".into(),
            json!(if scope == Scope::Workspace {
                "project"
            } else {
                "user"
            }),
        );
        location.insert("directoryPath".into(), json!(fsx::dirname(&path)));
        if let Some(workspace) = workspace.filter(|path| !path.is_empty()) {
            location.insert("projectPath".into(), json!(workspace));
        }
        Ok(Value::Object(location))
    }

    /// 读 server map；有旧 `enable` 字段时持锁就地折叠成 `enabled` 并回写（失败只记 warn）。
    async fn servers_with_migration(
        &self,
        path: &str,
        parsed: &Map<String, Value>,
        nested: bool,
    ) -> Map<String, Value> {
        let (servers, changed) = migrate_legacy(&server_map(parsed, nested));
        if !changed {
            return servers;
        }
        let owned = path.to_owned();
        let result = self
            .roots
            .writes
            .update(path, move |raw| {
                let Some(current) = parse_object(raw.as_deref(), &owned)? else {
                    return Ok(Update::Skip(()));
                };
                let (fresh, changed) = migrate_legacy(&server_map(&current, nested));
                if !changed {
                    return Ok(Update::Skip(()));
                }
                let next = with_server_map(&current, nested, fresh);
                Ok(Update::Write(js::pretty_line(&Value::Object(next)), ()))
            })
            .await;
        if let Err(error) = result {
            log::warn(format!(
                "[mcp-sync] legacy enable migration failed: {path} {}",
                error.message
            ));
        }
        servers
    }

    async fn directory_servers(
        &self,
        descriptor: Descriptor,
        scope: Scope,
        workspace: Option<&str>,
    ) -> Result<Vec<Value>, RpcError> {
        let path = self.config_path(descriptor, scope, workspace)?;
        let Some(parsed) = read_object(&path).await? else {
            return Ok(Vec::new());
        };
        let servers = self
            .servers_with_migration(&path, &parsed, descriptor.nested)
            .await;
        let location = self.location(descriptor, scope, workspace)?;
        Ok(servers
            .into_iter()
            .map(|(name, config)| {
                let mut record = Map::new();
                record.insert("source".into(), json!("zcodeagentmcp"));
                record.insert("scope".into(), json!(scope.as_str()));
                record.insert("name".into(), json!(name));
                let enabled = server_enabled(&config);
                record.insert("config".into(), config);
                record.insert("enabled".into(), json!(enabled));
                if scope == Scope::Workspace
                    && let Some(workspace) = workspace
                {
                    record.insert("projectPath".into(), json!(workspace));
                }
                record.insert("location".into(), location.clone());
                record.insert("file".into(), json!({ "format": "json", "filePath": path }));
                Value::Object(record)
            })
            .collect())
    }

    async fn preferred_servers(
        &self,
        scope: Scope,
        workspace: Option<&str>,
    ) -> Result<Vec<Value>, RpcError> {
        let zcode = self.directory_servers(ZCODE, scope, workspace).await?;
        if !zcode.is_empty() {
            return Ok(zcode);
        }
        self.directory_servers(AGENTS, scope, workspace).await
    }

    pub async fn load(&self, params: Value) -> Result<Value, RpcError> {
        let workspace = params
            .get("workspacePath")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty());
        let mut servers = Vec::new();
        if let Some(workspace) = workspace {
            servers.extend(
                self.preferred_servers(Scope::Workspace, Some(workspace))
                    .await?,
            );
        }
        servers.extend(self.preferred_servers(Scope::User, workspace).await?);
        Ok(json!({ "servers": servers }))
    }

    async fn user_records(&self, descriptor: Descriptor) -> Result<Vec<Record>, RpcError> {
        let path = self.config_path(descriptor, Scope::User, None)?;
        let Some(parsed) = read_object(&path).await? else {
            return Ok(Vec::new());
        };
        let servers = self
            .servers_with_migration(&path, &parsed, descriptor.nested)
            .await;
        Ok(servers
            .into_iter()
            .map(|(name, config)| Record {
                enabled: server_enabled(&config),
                name,
                config,
                source: descriptor.source,
                path: path.clone(),
            })
            .collect())
    }

    async fn effective_records(&self) -> Result<Vec<Record>, RpcError> {
        let mut records = self.user_records(ZCODE).await?;
        if records.is_empty() {
            records = self.user_records(AGENTS).await?;
        }
        records.sort_by(|left, right| locale_compare(&left.name, &right.name));
        Ok(records)
    }

    async fn effective_by_name(&self) -> Result<HashMap<String, Record>, RpcError> {
        let mut result = HashMap::new();
        for record in self.effective_records().await? {
            result.entry(name_key(&record.name)).or_insert(record);
        }
        Ok(result)
    }

    fn candidate(record: &Record) -> Value {
        json!({
            "id": fsx::sha256_hex(format!("{}:{}:{}", record.source, record.path, record.name).as_bytes()),
            "name": record.name,
            "config": record.config,
            "enabled": record.enabled,
            "source": record.source,
            "path": record.path,
        })
    }

    pub async fn list_local_candidates(&self) -> Result<Value, RpcError> {
        let candidates: Vec<Value> = self
            .effective_records()
            .await?
            .iter()
            .map(Self::candidate)
            .collect();
        Ok(json!({ "candidates": candidates, "localHomeDir": self.roots.home }))
    }

    pub async fn list_remote_statuses(&self, params: Value) -> Result<Value, RpcError> {
        let existing = self.effective_by_name().await?;
        let statuses: Vec<Value> = js::string_array(params.get("names"))
            .into_iter()
            .map(|name| match existing.get(&name_key(&name)) {
                Some(record) => json!({ "name": name, "exists": true, "path": record.path }),
                None => json!({ "name": name, "exists": false }),
            })
            .collect();
        Ok(json!({ "remoteHomeDir": self.roots.home, "statuses": statuses }))
    }

    pub async fn export(&self, params: Value) -> Result<Value, RpcError> {
        let candidates: Vec<Value> = self
            .effective_records()
            .await?
            .iter()
            .map(Self::candidate)
            .collect();
        let mut servers = Vec::new();
        for id in js::string_array(params.get("serverIds")) {
            let candidate = candidates
                .iter()
                .find(|candidate| candidate["id"] == id.as_str())
                .ok_or_else(|| fsx::error(format!("mcp sync candidate not found: {id}")))?;
            servers.push(candidate.clone());
        }
        Ok(json!({ "localHomeDir": self.roots.home, "servers": servers }))
    }

    pub async fn save(&self, payload: Value) -> Result<(), RpcError> {
        let action = payload.get("action").and_then(Value::as_str).unwrap_or("");
        let name = payload
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let project = payload
            .get("projectPath")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty());
        let scope = if project.is_some() {
            Scope::Workspace
        } else {
            Scope::User
        };
        if action == "set-enabled" {
            let enabled = payload
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or_else(|| fsx::error("Missing enabled value for MCP set-enabled action"))?;
            let location = match payload.get("location").filter(|value| !value.is_null()) {
                Some(location) => location.clone(),
                None => self.location(ZCODE, scope, project)?,
            };
            let source = location.get("source").and_then(Value::as_str).unwrap_or("");
            let descriptor = [ZCODE, AGENTS]
                .into_iter()
                .find(|descriptor| descriptor.source == source)
                .ok_or_else(|| {
                    fsx::error(format!(
                        "Unsupported MCP settings directory source: {source}"
                    ))
                })?;
            self.write_server_enabled(descriptor, &location, &name, enabled)
                .await?;
            return self.cleanup_legacy_override(&location, &name).await;
        }
        if action == "upsert" && payload.get("config").is_none_or(|config| !truthy(config)) {
            // 与 TS 相同：先读现有配置（可能触发旧字段迁移），再校验 config。
            let path = self.config_path(ZCODE, scope, project)?;
            if let Some(parsed) = read_object(&path).await? {
                self.servers_with_migration(&path, &parsed, true).await;
            }
            return Err(fsx::error("Missing MCP config for upsert action"));
        }
        let path = self.config_path(ZCODE, scope, project)?;
        if let Some(parsed) = read_object(&path).await? {
            self.servers_with_migration(&path, &parsed, true).await;
        }
        let config = payload.get("config").cloned();
        let upsert = action == "upsert";
        let owned = path.clone();
        self.roots
            .writes
            .update(&path, move |raw| {
                let current = parse_object(raw.as_deref(), &owned)?.unwrap_or_default();
                let mut servers = server_map(&current, true);
                if upsert {
                    servers.insert(name.clone(), config.clone().unwrap_or(Value::Null));
                } else {
                    js::delete(&mut servers, &name);
                }
                let next = with_server_map(&current, true, servers);
                Ok(Update::Write(js::pretty_line(&Value::Object(next)), ()))
            })
            .await
    }

    async fn write_server_enabled(
        &self,
        descriptor: Descriptor,
        location: &Value,
        name: &str,
        enabled: bool,
    ) -> Result<(), RpcError> {
        let scope = if location.get("scope").and_then(Value::as_str) == Some("project") {
            Scope::Workspace
        } else {
            Scope::User
        };
        let workspace = match scope {
            Scope::Workspace => location.get("projectPath").and_then(Value::as_str),
            Scope::User => None,
        };
        let path = self.config_path(descriptor, scope, workspace)?;
        let directory = location
            .get("directoryPath")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let (owned, name) = (path.clone(), name.to_owned());
        self.roots
            .writes
            .update(&path, move |raw| {
                let current = parse_object(raw.as_deref(), &owned)?.unwrap_or_default();
                let mut servers = server_map(&current, descriptor.nested);
                let Some(server) = servers
                    .get(&name)
                    .filter(|server| server.is_object())
                    .cloned()
                else {
                    return Ok(Update::Skip(()));
                };
                // 停用状态写在 server 配置对象内，不把目录路径写到 mcp 顶层。
                servers.insert(name.clone(), set_server_enabled(&server, enabled));
                let mut next = with_server_map(&current, descriptor.nested, servers);
                if let Some(cleaned) = remove_legacy_override(&next, &directory, &name) {
                    next = cleaned;
                }
                Ok(Update::Write(js::pretty_line(&Value::Object(next)), ()))
            })
            .await
    }

    async fn cleanup_legacy_override(&self, location: &Value, name: &str) -> Result<(), RpcError> {
        let path = self.config_path(ZCODE, Scope::User, None)?;
        let directory = location
            .get("directoryPath")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let (owned, name) = (path.clone(), name.to_owned());
        self.roots
            .writes
            .update(&path, move |raw| {
                let current = parse_object(raw.as_deref(), &owned)?.unwrap_or_default();
                Ok(match remove_legacy_override(&current, &directory, &name) {
                    Some(next) => Update::Write(js::pretty_line(&Value::Object(next)), ()),
                    None => Update::Skip(()),
                })
            })
            .await
    }

    pub async fn import(&self, params: Value) -> Result<Value, RpcError> {
        if params.get("overwrite").is_some_and(truthy) {
            return Err(fsx::error("mcp sync overwrite is not supported"));
        }
        let target = self.config_path(ZCODE, Scope::User, None)?;
        // 先算已有记录（可能触发旧字段迁移，自己持锁），再持锁合并写入。
        let mut existing = self.effective_by_name().await?;
        let paths = RewritePaths {
            local_home: params
                .get("localHomeDir")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            local_workspace: params
                .get("localWorkspacePath")
                .and_then(Value::as_str)
                .map(str::to_owned),
            remote_home: self.roots.home.clone(),
            remote_workspace: params
                .get("remoteWorkspacePath")
                .and_then(Value::as_str)
                .map(str::to_owned),
        };
        let servers = params
            .get("servers")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let path = target.clone();
        self.roots
            .writes
            .update(&path, move |raw| {
                let current = parse_object(raw.as_deref(), &target)?.unwrap_or_default();
                let mut target_servers = server_map(&current, true);
                let mut results = Vec::new();
                let mut changed = false;
                for server in &servers {
                    let name = server
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    let key = name_key(&name);
                    if target_servers.get(&name).is_some_and(truthy) {
                        results.push(json!({ "name": name, "status": "skipped", "path": target }));
                        continue;
                    }
                    if let Some(record) = existing.get(&key) {
                        results.push(
                            json!({ "name": name, "status": "skipped", "path": record.path }),
                        );
                        continue;
                    }
                    let enabled = server.get("enabled").is_some_and(truthy);
                    let config = server.get("config").cloned().unwrap_or(Value::Null);
                    let rewritten = rewrite_filesystem_config(
                        &name,
                        set_server_enabled(&config, enabled),
                        &paths,
                    );
                    target_servers.insert(name.clone(), rewritten.clone());
                    existing.insert(
                        key,
                        Record {
                            name: name.clone(),
                            config: rewritten,
                            enabled,
                            source: "zcode",
                            path: target.clone(),
                        },
                    );
                    results.push(json!({ "name": name, "status": "synced", "path": target }));
                    changed = true;
                }
                let result = json!({ "results": results });
                Ok(if changed {
                    let next = with_server_map(&current, true, target_servers);
                    Update::Write(js::pretty_line(&Value::Object(next)), result)
                } else {
                    Update::Skip(result)
                })
            })
            .await
    }

    pub async fn list_workspace_statuses(&self, params: Value) -> Result<Value, RpcError> {
        let runtime = self.runtime.clone().ok_or_else(|| {
            fsx::error("MCP server status listing is unavailable in this runtime")
        })?;
        let target = WorkspaceTarget::from_value(&params)?;
        let servers = params
            .get("mcpServers")
            .filter(|value| !value.is_null())
            .cloned();
        let mode = params
            .get("mode")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let request = |omit_servers: bool, omit_mode: bool| {
            let mut body = Map::new();
            body.insert("workspace".into(), target.workspace_ref());
            if let Some(servers) = servers.as_ref().filter(|_| !omit_servers) {
                body.insert("mcpServers".into(), servers.clone());
            }
            if let Some(mode) = mode.as_ref().filter(|_| !omit_mode) {
                body.insert("mode".into(), json!(mode));
            }
            Value::Object(body)
        };
        // Rust CLI 校验请求 workspace 与进程一致：mcp/list 发给该 workspace 自己的 CLI（见 spec §5）。
        let send = |body: Value| {
            let runtime = runtime.clone();
            let target = target.clone();
            async move {
                let process = runtime
                    .get_client(&target, RuntimePolicy::StartIfNeeded)
                    .await?;
                process.request_value("mcp/list", &body).await
            }
        };
        match send(request(false, false)).await {
            Ok(result) => Ok(result),
            Err(error) => {
                let keys = unrecognized_keys(&error);
                let has = |key: &str| keys.iter().any(|item| item == key);
                if mode.as_deref() == Some("status") && has("mode") {
                    let mut unsupported = RpcError::new(
                        "ZCodeAgentMcpStatusModeUnsupportedError",
                        "The connected ZCode Agent does not support MCP status-only refresh",
                    )
                    .with_code(STATUS_MODE_UNSUPPORTED_CODE);
                    unsupported.data = None;
                    return Err(unsupported);
                }
                if (mode.is_some() && has("mode")) || (servers.is_some() && has("mcpServers")) {
                    return send(request(has("mcpServers"), has("mode"))).await;
                }
                if !error.message.starts_with("ZCode agent request timed out") {
                    return Err(error);
                }
                log::warn(format!(
                    "MCP 状态列表请求超时，重启无响应 agent 后重试一次 workspaceKey={}",
                    target.key()
                ));
                send(request(false, false)).await
            }
        }
    }

    pub fn user_config_directory(&self) -> String {
        fsx::dirname(&self.roots.cli_config_path())
    }
}

fn name_key(name: &str) -> String {
    zcode_kit::js::string::trim(name).to_lowercase()
}

/// `getUnrecognizedTopLevelKeys`：-32602 且 data.message 是 zod issues JSON。
fn unrecognized_keys(error: &RpcError) -> Vec<String> {
    if error.code_i64() != Some(-32602)
        || (error.message != "Invalid params" && !error.message.starts_with("Invalid params — "))
    {
        return Vec::new();
    }
    let Some(serialized) = error
        .data
        .as_deref()
        .and_then(|data| data.get("message"))
        .and_then(Value::as_str)
    else {
        return Vec::new();
    };
    let Ok(Value::Array(issues)) = serde_json::from_str::<Value>(serialized) else {
        return Vec::new();
    };
    let mut keys = Vec::new();
    for issue in issues {
        let top_level = issue
            .get("path")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty);
        if issue.get("code") != Some(&json!("unrecognized_keys")) || !top_level {
            continue;
        }
        for key in issue
            .get("keys")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(key) = key.as_str()
                && !keys.iter().any(|item: &String| item == key)
            {
                keys.push(key.to_owned());
            }
        }
    }
    keys
}

struct RewritePaths {
    local_home: String,
    local_workspace: Option<String>,
    remote_home: String,
    remote_workspace: Option<String>,
}

/// `rewriteFilesystemMcpConfig`：filesystem MCP 的路径参数从本机改写到远端。
fn rewrite_filesystem_config(name: &str, config: Value, paths: &RewritePaths) -> Value {
    if !is_stdio(&config) || !is_filesystem_server(name, &config) {
        return config;
    }
    let Some(args) = config.get("args").and_then(Value::as_array).cloned() else {
        return config;
    };
    let mut next = config.as_object().cloned().unwrap_or_default();
    next.insert(
        "args".into(),
        Value::Array(
            args.into_iter()
                .map(|arg| match arg {
                    Value::String(text) => Value::String(rewrite_path_arg(&text, paths)),
                    other => other,
                })
                .collect(),
        ),
    );
    Value::Object(next)
}

fn is_stdio(config: &Value) -> bool {
    let kind = config
        .get("type")
        .and_then(Value::as_str)
        .map(|kind| zcode_kit::js::string::trim(kind).to_lowercase())
        .unwrap_or_default();
    if kind.is_empty()
        && config
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| !zcode_kit::js::string::trim(command).is_empty())
    {
        return true;
    }
    kind == "stdio"
}

fn is_filesystem_server(name: &str, config: &Value) -> bool {
    let normalized = name_key(name);
    if matches!(normalized.as_str(), "filesystem" | "file-system" | "fs") {
        return true;
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(command) = config.get("command").and_then(Value::as_str) {
        parts.push(command.to_owned());
    }
    for arg in config
        .get("args")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(arg) = arg.as_str() {
            parts.push(arg.to_owned());
        }
    }
    let haystack = parts.join(" ").to_lowercase();
    haystack.contains("@modelcontextprotocol/server-filesystem")
        || haystack.contains("mcp-server-filesystem")
}

fn rewrite_path_arg(arg: &str, paths: &RewritePaths) -> String {
    if let Some(relative) = relative_within(paths.local_workspace.as_deref(), arg)
        && let Some(remote) = paths
            .remote_workspace
            .as_deref()
            .filter(|path| !zcode_kit::js::string::trim(path).is_empty())
    {
        return join_remote(remote, &relative);
    }
    if let Some(relative) = relative_within(Some(&paths.local_home), arg) {
        return join_remote(&paths.remote_home, &relative);
    }
    arg.to_owned()
}

fn comparable(raw: &str) -> Option<(String, bool)> {
    let trimmed = zcode_kit::js::string::trim(raw);
    if trimmed.is_empty() {
        return None;
    }
    let bytes = trimmed.as_bytes();
    let windows = (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/'))
        || trimmed.starts_with("\\\\");
    if !windows && !trimmed.starts_with('/') {
        return None;
    }
    let mut value = trimmed.replace('\\', "/").trim_end_matches('/').to_owned();
    if value.is_empty() {
        value = "/".to_owned();
    }
    Some((value, windows))
}

fn relative_within(base: Option<&str>, candidate: &str) -> Option<String> {
    let base = base.filter(|base| !zcode_kit::js::string::trim(base).is_empty())?;
    let (base_value, insensitive) = comparable(base)?;
    let (candidate_value, _) = comparable(candidate)?;
    let fold = |value: &str| {
        if insensitive {
            value.to_lowercase()
        } else {
            value.to_owned()
        }
    };
    let (base_cmp, candidate_cmp) = (fold(&base_value), fold(&candidate_value));
    if candidate_cmp == base_cmp {
        return Some(String::new());
    }
    let prefix = if base_cmp.ends_with('/') {
        base_cmp
    } else {
        format!("{base_cmp}/")
    };
    if !candidate_cmp.starts_with(&prefix) {
        return None;
    }
    Some(candidate_value[prefix.len()..].to_owned())
}

/// posix / win32 `path.join` 的词法规范化。
fn normalize_join(base: &str, segments: &[&str], separator: char) -> String {
    let joined = std::iter::once(base)
        .chain(segments.iter().copied())
        .collect::<Vec<_>>()
        .join("/");
    let unified = joined.replace('\\', "/");
    let (root, rest) = if separator == '\\' && unified.len() >= 2 && unified.as_bytes()[1] == b':' {
        let drive = &unified[..2];
        match unified[2..].strip_prefix('/') {
            Some(rest) => (format!("{drive}/"), rest.to_owned()),
            None => (drive.to_owned(), unified[2..].to_owned()),
        }
    } else if separator == '\\' && unified.starts_with("//") {
        ("//".to_owned(), unified[2..].to_owned())
    } else if let Some(rest) = unified.strip_prefix('/') {
        ("/".to_owned(), rest.to_owned())
    } else {
        (String::new(), unified)
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if root.is_empty() {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let body = format!("{root}{}", parts.join("/"));
    let body = if body.is_empty() {
        ".".to_owned()
    } else {
        body
    };
    if separator == '\\' {
        body.replace('/', "\\")
    } else {
        body
    }
}

/// `joinRemotePath`：按远端路径形态选择 posix / win32 拼接。
fn join_remote(base: &str, relative: &str) -> String {
    if relative.is_empty() {
        return base.to_owned();
    }
    let segments: Vec<&str> = relative
        .split(['/', '\\'])
        .filter(|segment| !segment.is_empty())
        .collect();
    let normalized = zcode_kit::js::string::trim(base);
    let bytes = normalized.as_bytes();
    let windows = (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/'))
        || normalized.starts_with("\\\\");
    if !normalized.starts_with('/') && windows {
        return normalize_join(normalized, &segments, '\\');
    }
    normalize_join(&normalized.replace('\\', "/"), &segments, '/')
}

pub fn service(mcp: McpSyncService) -> ServiceBuilder {
    let builder = ServiceBuilder::new(CHANNEL);
    let builder = bind(
        builder,
        "loadMcpFromUserDirectory",
        &mcp,
        |s, p| async move { s.load(p).await.map(Reply::Value) },
    );
    let builder = bind(
        builder,
        "listWorkspaceMcpServerStatuses",
        &mcp,
        |s, p| async move { s.list_workspace_statuses(p).await.map(Reply::Value) },
    );
    let builder = bind(builder, "saveMcpToUserDirectory", &mcp, |s, p| async move {
        s.save(p).await.map(|()| Reply::Undefined)
    });
    let builder = bind(
        builder,
        "listLocalUserMcpCandidates",
        &mcp,
        |s, _| async move { s.list_local_candidates().await.map(Reply::Value) },
    );
    let builder = bind(
        builder,
        "listRemoteUserMcpStatuses",
        &mcp,
        |s, p| async move { s.list_remote_statuses(p).await.map(Reply::Value) },
    );
    let builder = bind(builder, "exportMcpServers", &mcp, |s, p| async move {
        s.export(p).await.map(Reply::Value)
    });
    let builder = bind(
        builder,
        "checkRemoteUserMcpWriteAccess",
        &mcp,
        |s, _| async move {
            Ok(Reply::Value(
                check_write_access(&s.user_config_directory()).await,
            ))
        },
    );
    bind(builder, "importMcpServers", &mcp, |s, p| async move {
        s.import(p).await.map(Reply::Value)
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn rewrites_filesystem_paths_for_remote() {
        let paths = RewritePaths {
            local_home: "/Users/me".into(),
            local_workspace: Some("/Users/me/repo".into()),
            remote_home: "/home/me".into(),
            remote_workspace: Some("/srv/repo".into()),
        };
        let config = json!({"command":"npx","args":["-y","@modelcontextprotocol/server-filesystem","/Users/me/repo/src","/Users/me/docs","/etc"]});
        let rewritten = rewrite_filesystem_config("files", config, &paths);
        assert_eq!(
            rewritten["args"],
            json!([
                "-y",
                "@modelcontextprotocol/server-filesystem",
                "/srv/repo/src",
                "/home/me/docs",
                "/etc"
            ])
        );
        assert_eq!(join_remote("C:\\Users\\me", "a/b"), "C:\\Users\\me\\a\\b");
    }

    #[tokio::test]
    async fn save_load_and_legacy_migration() {
        let home = tempfile::tempdir().unwrap();
        let config_dir = home.path().join(".zcode/cli");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.json"),
            r#"{"other":1,"mcp":{"servers":{"a":{"command":"x","enable":false,"enabled":true}}}}"#,
        )
        .unwrap();
        let service = McpSyncService::new(
            Roots::new(
                &zcode_host_base::testing::spawner(),
                home.path(),
                home.path(),
            ),
            None,
        );
        let loaded = service.load(json!({})).await.unwrap();
        assert_eq!(loaded["servers"][0]["enabled"], false);
        let text = std::fs::read_to_string(config_dir.join("config.json")).unwrap();
        assert_eq!(
            text,
            "{\n  \"other\": 1,\n  \"mcp\": {\n    \"servers\": {\n      \"a\": {\n        \"command\": \"x\",\n        \"enabled\": false\n      }\n    }\n  }\n}\n"
        );
        service
            .save(json!({"action":"set-enabled","name":"a","enabled":true}))
            .await
            .unwrap();
        service
            .save(json!({"action":"upsert","name":"b","config":{"url":"http://127.0.0.1:1"}}))
            .await
            .unwrap();
        let candidates = service.list_local_candidates().await.unwrap();
        assert_eq!(candidates["candidates"].as_array().unwrap().len(), 2);
        assert_eq!(candidates["candidates"][0]["enabled"], true);
        let imported = service
            .import(json!({"servers":[{"name":"b","config":{},"enabled":true},{"name":"c","config":{"command":"y"},"enabled":false}],"localHomeDir":"/x"}))
            .await
            .unwrap();
        assert_eq!(imported["results"][0]["status"], "skipped");
        assert_eq!(imported["results"][1]["status"], "synced");
        assert!(
            service
                .save(json!({"action":"upsert","name":"z"}))
                .await
                .is_err()
        );
        std::fs::write(config_dir.join("config.json"), "{bad").unwrap();
        assert!(service.load(json!({})).await.is_err());
    }
}
