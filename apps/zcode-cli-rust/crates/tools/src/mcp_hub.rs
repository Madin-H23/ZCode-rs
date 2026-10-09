use super::{mcp_config, mcp_connection::Connection};
use crate::contract::{ProcessCleanupFailure, ToolOutput};
use anyhow::{Context, Result, ensure};
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};
use tokio_util::sync::CancellationToken;

#[path = "mcp_hub_bindings.rs"]
mod bindings;
#[derive(Clone)]
struct Binding {
    name: String,
    /// The configured server name (`plugin:<plugin>:<key>` for plugin servers).
    server: String,
    original: String,
    key: String,
    safe: bool,
    /// Node MCP permission metadata: `readOnlyHint === true` / `destructiveHint === true`.
    read_only: bool,
    destructive: bool,
    definition: Value,
    connection: Arc<Connection>,
}
#[derive(Default)]
struct State {
    borrowed: BTreeSet<String>,
    overrides: BTreeMap<String, Value>,
    connections: BTreeMap<String, Arc<Connection>>,
    bindings: BTreeMap<String, Vec<Binding>>,
    /// Server statuses of the latest prepare and the workspace root it served.
    statuses: (PathBuf, BTreeMap<String, Value>),
}
/// The workspace view a hub call serves: its root, configuration and official
/// MCP identity (spec rust-cli-architecture, Workspace 作用域).
pub(super) struct Scope<'a> {
    pub cwd: &'a Path,
    pub config: &'a dyn crate::contract::ConfigSource,
    pub official: &'a Arc<super::official_auth::OfficialAuth>,
}
/// MCP connections of the process, shared by every workspace view.
pub(super) struct Hub {
    egress: Arc<zcode_cli_net::Egress>,
    state: RwLock<State>,
    gate: tokio::sync::Mutex<()>,
    stop: CancellationToken,
    /// Process telemetry of stdio servers (spec rust-m9-usage-logs §6).
    pub telemetry: Arc<super::mcp_telemetry::Tracker>,
}
impl Hub {
    pub fn inherit(&self, parent: &str, child: &str) {
        let mut state = self.state.write().unwrap();
        let bindings = state.bindings.get(parent).cloned().unwrap_or_default();
        state.bindings.insert(child.into(), bindings);
        state.borrowed.insert(child.into());
    }
    pub fn new(
        egress: Arc<zcode_cli_net::Egress>,
        sink: Option<super::mcp_telemetry::ProcessSink>,
    ) -> Self {
        Self {
            telemetry: Arc::new(super::mcp_telemetry::Tracker::new(sink)),
            egress,
            state: Default::default(),
            gate: Default::default(),
            stop: CancellationToken::new(),
        }
    }
    pub fn configure(&self, cwd: &Path, session: &str, servers: &Value) -> Result<()> {
        mcp_config::explicit(servers, (cwd, self.egress.home()))?;
        self.state
            .write()
            .unwrap()
            .overrides
            .insert(session.into(), servers.clone());
        Ok(())
    }
    /// `(read_only, destructive)` of a bound MCP tool.
    pub fn annotations(&self, session: &str, name: &str) -> Option<(bool, bool)> {
        self.state
            .read()
            .unwrap()
            .bindings
            .get(session)
            .and_then(|b| b.iter().find(|b| b.name == name))
            .map(|b| (b.read_only, b.destructive))
    }
    /// `(server, tool names)` bound for `session`; only connected servers have tools.
    pub fn inventory(&self, session: &str) -> Vec<(String, Vec<String>)> {
        let state = self.state.read().unwrap();
        let mut servers: Vec<(String, Vec<String>)> = vec![];
        for binding in state.bindings.get(session).into_iter().flatten() {
            match servers.iter_mut().find(|(s, _)| *s == binding.server) {
                Some((_, tools)) => tools.push(binding.name.clone()),
                None => servers.push((binding.server.clone(), vec![binding.name.clone()])),
            }
        }
        servers
    }
    pub async fn definitions(
        &self,
        scope: &Scope<'_>,
        session: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<Value>> {
        let overrides = self.state.read().unwrap().overrides.get(session).cloned();
        let borrowed = self.state.read().unwrap().borrowed.contains(session);
        if !borrowed {
            self.prepare(scope, session, overrides.as_ref(), false, cancel)
                .await?;
        }
        Ok(self
            .state
            .read()
            .unwrap()
            .bindings
            .get(session)
            .into_iter()
            .flatten()
            .map(|b| b.definition.clone())
            .collect())
    }
    /// `mcp/list` of one workspace. Connect mode converges the process-wide
    /// `mcp-status` lease on that workspace (Node `protocol-settings` lease,
    /// replace semantics); status mode only reports that workspace's latest
    /// prepare and nothing for another workspace.
    pub async fn list(
        &self,
        scope: &Scope<'_>,
        p: &Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        if p["mode"] == "status" {
            return Ok(json!({"statuses":self.statuses(scope.cwd)}));
        }
        ensure!(
            p.get("mode").is_none_or(|v| v == "connect"),
            "Invalid MCP list mode"
        );
        self.prepare(scope, "mcp-status", p.get("mcpServers"), true, cancel)
            .await?;
        Ok(json!({"statuses":self.statuses(scope.cwd)}))
    }
    fn statuses(&self, cwd: &Path) -> BTreeMap<String, Value> {
        let state = self.state.read().unwrap();
        if state.statuses.0 == cwd {
            state.statuses.1.clone()
        } else {
            BTreeMap::new()
        }
    }
    #[expect(clippy::await_holding_invalid_type, reason = "存量，U2 改为 actor")]
    async fn prepare(
        &self,
        scope: &Scope<'_>,
        session: &str,
        overrides: Option<&Value>,
        refresh: bool,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let _gate = tokio::select! {biased;_=cancel.cancelled()=>anyhow::bail!("Cancelled"),_=self.stop.cancelled()=>anyhow::bail!("MCP stopped"),gate=self.gate.lock()=>gate};
        let request_cancel = self.stop.child_token();
        let relay_cancel = request_cancel.clone();
        let caller = cancel.clone();
        let relay = tokio::spawn(async move {
            caller.cancelled().await;
            relay_cancel.cancel();
        });
        let result = self
            .prepare_inner(scope, session, overrides, refresh, &request_cancel)
            .await;
        relay.abort();
        result
    }
    async fn prepare_inner(
        &self,
        scope: &Scope<'_>,
        session: &str,
        overrides: Option<&Value>,
        refresh: bool,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let config = scope.config.load().await?;
        let configs = tokio::select! {biased;_=cancel.cancelled()=>anyhow::bail!("Cancelled"),result=mcp_config::configured((&self.egress,scope.cwd),&config.config,overrides,cancel)=>result?};
        let mut bindings = vec![];
        let mut statuses = BTreeMap::new();
        let mut names = BTreeSet::new();
        let enabled = configs.iter().filter(|c| c.enabled).count();
        let mut futures = stream::iter(configs)
            .map(|server| async move {
                let key = server.key(session);
                let previous = self.state.read().unwrap().connections.get(&key).cloned();
                let result = if !server.enabled {
                    Ok(None)
                } else if server.invalid {
                    Err(anyhow::anyhow!("config_invalid"))
                } else if let Some(previous) =
                    previous.filter(|p| !p.peer.is_transport_closed() && !refresh)
                {
                    Ok(Some(previous))
                } else {
                    // 与 Node 一致：MCP HTTP 与模型共用出口规则，stdio 子进程使用同一份工具环境。
                    let transport = if server.transport == "stdio" {
                        Ok(super::mcp_connection::Transport::Stdio(
                            self.egress.tool_env(),
                        ))
                    } else {
                        self.egress
                            .client(zcode_cli_net::Purpose::Mcp)
                            .await
                            .map(super::mcp_connection::Transport::Http)
                            .map_err(anyhow::Error::new)
                    };
                    match transport {
                        Ok(transport) => {
                            let telemetry = (&self.telemetry, key.as_str());
                            Connection::open(&server, transport, scope.official, cancel, telemetry)
                                .await
                                .map(|c| Some(Arc::new(c)))
                        }
                        Err(error) => Err(error),
                    }
                };
                (server, key, result)
            })
            .buffered(4);
        let mut cleanup_failure = None;
        while let Some((server, key, result)) = futures.next().await {
            match result {
                Ok(None) => {
                    statuses.insert(
                        server.name.clone(),
                        mcp_config::status(&server, "disabled", 0, None),
                    );
                }
                Ok(Some(connection)) => {
                    let discovered = bindings::bind(&server, &key, &connection, &mut names);
                    match discovered {
                        Ok(entries) => {
                            let mut status =
                                mcp_config::status(&server, "connected", entries.len(), None);
                            status["protocolEra"] = if connection.modern {
                                "modern"
                            } else {
                                "legacy"
                            }
                            .into();
                            statuses.insert(server.name, status);
                            let old = self
                                .state
                                .write()
                                .unwrap()
                                .connections
                                .insert(key, connection.clone());
                            if let Some(old) = old
                                && !Arc::ptr_eq(&old, &connection)
                                && let Err(error) = old.close().await
                            {
                                cleanup_failure = Some(error);
                            }
                            bindings.extend(entries);
                        }
                        Err(_) => {
                            if let Err(error) = connection.close().await {
                                cleanup_failure = Some(error);
                            }
                            statuses.insert(
                                server.name.clone(),
                                mcp_config::status(&server, "failed", 0, Some("tool_list_failed")),
                            );
                        }
                    }
                }
                Err(error) => {
                    if error.is::<ProcessCleanupFailure>() {
                        cleanup_failure = Some(error);
                        continue;
                    }
                    if let Some(status) = super::mcp_official::failed_status(&server, &error) {
                        statuses.insert(server.name, status);
                        continue;
                    }
                    let reason = error.to_string();
                    let kind = match reason.as_str() {
                        "config_invalid" => "config_invalid",
                        "not_authenticated" => "not_authenticated",
                        "connection_timeout" => "connection_timeout",
                        "process_start_failed" => "process_start_failed",
                        "tool_list_failed" => "tool_list_failed",
                        _ => "protocol_negotiation_failed",
                    };
                    statuses.insert(
                        server.name.clone(),
                        mcp_config::status(&server, "failed", 0, Some(kind)),
                    );
                }
            }
        }
        // Node session_startup：会话首次快照时的已配置、已连接与 stdio 进程数。
        let keys: BTreeSet<String> = bindings.iter().map(|b| b.key.clone()).collect();
        self.telemetry.bind(session, &keys);
        let connected = statuses.values().filter(|s| s["status"] == "connected");
        let processes = connected
            .clone()
            .filter(|s| s["transport"] == "stdio")
            .count();
        // Node：features.mcp 关闭时不建 tracker，不发任何进程遥测。
        if config.config["features"]["mcp"] != false {
            self.telemetry
                .session_startup(session, (enabled, connected.count(), processes));
        }
        {
            let mut state = self.state.write().unwrap();
            state.bindings.insert(session.into(), bindings);
            state.statuses = (scope.cwd.to_owned(), statuses);
        }
        self.prune().await?;
        if let Some(error) = cleanup_failure {
            return Err(error);
        }
        super::tools::check_cancel(cancel)
    }
    pub async fn call(
        &self,
        session: &str,
        name: &str,
        args: &Value,
        cancel: &CancellationToken,
    ) -> Result<ToolOutput> {
        let binding = self
            .state
            .read()
            .unwrap()
            .bindings
            .get(session)
            .and_then(|bs| bs.iter().find(|b| b.name == name))
            .cloned()
            .context("MCP tool unavailable in this session")?;
        binding
            .connection
            .call(&binding.original, args, cancel)
            .await
    }
    #[expect(clippy::await_holding_invalid_type, reason = "存量，U2 改为 actor")]
    pub async fn close_session(&self, session: &str, forget: bool) -> Result<()> {
        let _gate = self.gate.lock().await;
        {
            let mut state = self.state.write().unwrap();
            state.bindings.remove(session);
            state.borrowed.remove(session);
            self.telemetry.unbind(session);
            if forget {
                state.overrides.remove(session);
            }
        }
        self.prune().await
    }
    async fn prune(&self) -> Result<()> {
        let removed = {
            let mut state = self.state.write().unwrap();
            let used = state
                .bindings
                .values()
                .flatten()
                .map(|b| b.key.clone())
                .collect::<BTreeSet<_>>();
            let (removed, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut state.connections)
                .into_iter()
                .partition(|(key, _)| !used.contains(key));
            state.connections = kept.into_iter().collect();
            removed
        };
        for (_, connection) in removed {
            connection.close().await?;
        }
        Ok(())
    }
    /// Cancelled when the hub shuts down (the resource sampler stops with it).
    pub fn stopped(&self) -> CancellationToken {
        self.stop.clone()
    }
    #[expect(clippy::await_holding_invalid_type, reason = "存量，U2 改为 actor")]
    pub async fn shutdown(&self) -> Result<()> {
        self.stop.cancel();
        let _gate = self.gate.lock().await;
        {
            let mut state = self.state.write().unwrap();
            state.bindings.clear();
            state.overrides.clear();
        }
        self.prune().await
    }
}
