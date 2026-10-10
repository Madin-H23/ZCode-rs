//! 账号 provider 第三层 overlay（packages/provider/src/account-provider-service.ts、
//! account-provider-resolution.ts 与 services/src/model-provider/accountProviderConnectionResolver.ts）。
//!
//! `AccountProviderSource` 是账号快照的唯一所有者：刷新 single-flight，刷新期间到达的原因并入
//! 下一轮；本轮查询期间身份或设置变化时丢弃结果；首次失败发布 fail-closed，之后失败保留上一份。
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use sha2::Digest;
use tokio::sync::oneshot;
use zcode_async::Spawner;
use zcode_host_kit::{EventHub, Overflow, log};

use super::auth::{AccountRequestAuth, AccountSettings, CodingPlanKeys, read_account_settings};
use super::availability::{Availability, AvailabilityProvider, AvailabilityResolver};
use crate::providers::{AccountLayer, ProviderRuntime};
use crate::settings::SettingStore;

const ACCOUNT_SETTING_KEYS: [&str; 3] = [
    "providerFamilyDomain",
    "providerFamilyConnectionSelections",
    "zcodeEndpointOrigin",
];

/// 一个账号 provider 的连接结果（`AccountProviderConnectionResult`）。
#[derive(Debug, Clone, PartialEq)]
struct Connection {
    provider_id: String,
    status: &'static str,
    unavailable_reason: Option<&'static str>,
    models: Option<Vec<String>>,
    current: Option<bool>,
    connection_key: Option<String>,
    effective_at: Option<f64>,
    reset_previous: bool,
}

impl Connection {
    fn new(provider_id: &str, status: &'static str) -> Self {
        Self {
            provider_id: provider_id.to_owned(),
            status,
            unavailable_reason: None,
            models: None,
            current: None,
            connection_key: None,
            effective_at: None,
            reset_previous: false,
        }
    }
}

/// `resolveAccountUnavailableReason`。
fn account_reason(reason: &str) -> &'static str {
    match reason {
        "coding_plan_not_authenticated" => "not-authenticated",
        "coding_plan_not_connected" => "not-connected",
        "coding_plan_auth_failed" => "credential-failed",
        _ => "not-entitled",
    }
}

fn is_credential_refresh_reason(reason: &str) -> bool {
    reason.contains("oauth-callback")
        || reason.rsplit(':').next() == Some("oauth-login-entitlement")
}

/// 内置层里 `access.type = zhipu-account` 的 provider：(id, access)。
fn configured_account_providers(builtin: &Value) -> Vec<(String, Value)> {
    let mut seen = std::collections::HashSet::new();
    builtin["providerConfigRules"]["providerRules"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|rule| {
            let id = rule["providerId"].as_str()?;
            if id.is_empty() || id == "builtin:zapi" || !seen.insert(id.to_owned()) {
                return None;
            }
            let access = &rule["config"]["access"];
            (access["type"] == "zhipu-account").then(|| (id.to_owned(), access.clone()))
        })
        .collect()
}

fn sha256_hex(text: &str) -> String {
    sha2::Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `createFailClosedAccountProviderConfigSnapshot`。
fn fail_closed(builtin_revision: &str, builtin: &Value) -> AccountLayer {
    let providers: Map<String, Value> = configured_account_providers(builtin)
        .into_iter()
        .map(|(id, _)| {
            (
                id,
                json!({"access": {"type": "zhipu-account", "entitled": false}}),
            )
        })
        .collect();
    layer(builtin_revision, providers, None)
}

fn layer(
    based_on: &str,
    providers: Map<String, Value>,
    states: Option<Map<String, Value>>,
) -> AccountLayer {
    let revision = format!(
        "account:{}",
        json!([
            based_on,
            providers,
            states.clone().map(Value::Object).unwrap_or(Value::Null)
        ])
    );
    AccountLayer {
        revision,
        based_on_builtin_revision: based_on.to_owned(),
        providers,
        states: states.unwrap_or_default(),
    }
}

struct Waiter {
    generation: u64,
    sender: oneshot::Sender<Result<Arc<AccountLayer>, String>>,
}

#[derive(Default)]
struct SourceState {
    snapshot: Option<Arc<AccountLayer>>,
    pending_reasons: Vec<String>,
    requested_generation: u64,
    in_flight: bool,
    waiters: Vec<Waiter>,
    previous_scopes: HashMap<String, String>,
}

#[derive(Clone)]
pub struct AccountProviderSource {
    inner: Arc<Inner>,
}

struct Inner {
    /// 账号的作用域：刷新与变化订阅在这里。
    tasks: Spawner,
    runtime: ProviderRuntime,
    settings: SettingStore,
    auth: AccountRequestAuth,
    keys: CodingPlanKeys,
    availability: AvailabilityResolver,
    state: Mutex<SourceState>,
    /// 账号快照变化（不含首份），参数是合并后的原因；CLI 推送订阅它。
    changed: EventHub<String>,
}

impl AccountProviderSource {
    pub fn new(
        spawner: &Spawner,
        runtime: ProviderRuntime,
        settings: SettingStore,
        auth: AccountRequestAuth,
        keys: CodingPlanKeys,
        availability: AvailabilityResolver,
    ) -> Self {
        runtime.attach_account_layer();
        Self {
            inner: Arc::new(Inner {
                tasks: spawner.clone(),
                runtime,
                settings,
                auth,
                keys,
                availability,
                state: Mutex::new(SourceState::default()),
                // 订阅者是 CLI 热同步（按原因逐次推送，原因进日志），一次变化一条；落后 256 条说明它卡住，
                // 结束它的订阅（docs/specs/rust-u2-task-scope.md §6.2）。
                changed: EventHub::new("account-providers-changed", 256, Overflow::CloseSubscriber),
            }),
        }
    }

    pub fn changes(&self) -> EventHub<String> {
        self.inner.changed.clone()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, SourceState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 启动：首轮刷新，并订阅内置配置与账号相关设置的变化。
    pub fn start(&self) {
        let source = self.clone();
        let mut builtin = Box::pin(self.inner.runtime.builtin_changes().subscribe());
        let tasks = &self.inner.tasks;
        drop(tasks.spawn_resident("account-builtin-changes", async move {
            while builtin.next().await.is_some() {
                let _ = source.refresh("config:zcodeBuiltin:file-changed").await;
            }
        }));
        let source = self.clone();
        let mut settings = Box::pin(self.inner.settings.on_updated().subscribe());
        drop(tasks.spawn_resident("account-setting-changes", async move {
            while let Some(keys) = settings.next().await {
                let keys: Vec<&String> = keys
                    .iter()
                    .filter(|key| ACCOUNT_SETTING_KEYS.contains(&key.as_str()))
                    .collect();
                if !keys.is_empty() {
                    let reason = format!(
                        "settings:{}",
                        keys.iter()
                            .map(|k| k.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    );
                    let _ = source.refresh(&reason).await;
                }
            }
        }));
        let source = self.clone();
        drop(tasks.spawn("account-first-read", async move {
            let _ = source.read().await;
        }));
    }

    /// 当前快照；还没有时触发首轮刷新并等待。
    pub async fn read(&self) -> Result<Arc<AccountLayer>, String> {
        if let Some(snapshot) = self.state().snapshot.clone() {
            return Ok(snapshot);
        }
        self.refresh("start").await
    }

    /// 返回覆盖本次请求之后状态的一轮结果（refresh 契约）。
    pub async fn refresh(&self, reason: &str) -> Result<Arc<AccountLayer>, String> {
        let (receiver, start) = {
            let mut state = self.state();
            state.requested_generation += 1;
            let generation = state.requested_generation;
            if !state.pending_reasons.iter().any(|r| r == reason) {
                state.pending_reasons.push(reason.to_owned());
            }
            let (sender, receiver) = oneshot::channel();
            state.waiters.push(Waiter { generation, sender });
            let start = !state.in_flight;
            if start {
                state.in_flight = true;
            }
            (receiver, start)
        };
        if start {
            let source = self.clone();
            drop(
                self.inner
                    .tasks
                    .spawn("account-provider-refresh", async move {
                        source.run_loop().await
                    }),
            );
        }
        receiver
            .await
            .unwrap_or_else(|_| Err("AccountProviderService 已 dispose".to_owned()))
    }

    fn settle(&self, generation: u64, result: Result<Arc<AccountLayer>, String>) {
        let mut state = self.state();
        let waiters = std::mem::take(&mut state.waiters);
        for waiter in waiters {
            if waiter.generation <= generation {
                let _ = waiter.sender.send(result.clone());
            } else {
                state.waiters.push(waiter);
            }
        }
    }

    async fn run_loop(&self) {
        loop {
            let (generation, reasons) = {
                let mut state = self.state();
                if state.pending_reasons.is_empty() {
                    state.in_flight = false;
                    return;
                }
                (
                    state.requested_generation,
                    std::mem::take(&mut state.pending_reasons),
                )
            };
            let latest = self.state().snapshot.clone();
            let (builtin_revision, builtin) = self.inner.runtime.builtin_snapshot().await;
            match self.resolve(&builtin, latest.as_deref(), &reasons).await {
                Ok((providers, states)) => {
                    let (current_revision, _) = self.inner.runtime.builtin_snapshot().await;
                    let superseded = !self.state().pending_reasons.is_empty()
                        || current_revision != builtin_revision;
                    if superseded {
                        let mut state = self.state();
                        if !state
                            .pending_reasons
                            .iter()
                            .any(|r| r == "superseded-resolution")
                        {
                            state
                                .pending_reasons
                                .push("superseded-resolution".to_owned());
                        }
                        continue;
                    }
                    let next = layer(&builtin_revision, providers, Some(states));
                    if latest
                        .as_ref()
                        .is_some_and(|latest| latest.revision == next.revision)
                    {
                        self.settle(generation, Ok(latest.clone().expect("checked")));
                        continue;
                    }
                    let next = Arc::new(next);
                    self.state().snapshot = Some(next.clone());
                    self.inner.runtime.set_account_layer((*next).clone()).await;
                    self.settle(generation, Ok(next));
                    if latest.is_some() {
                        self.inner.changed.emit(reasons.join(","));
                    }
                }
                Err(error) => {
                    log::warn(format!(
                        "[account-provider-runtime] account provider source refresh failed reasons={reasons:?}: {error}"
                    ));
                    if latest.is_none() {
                        // 首次账号查询失败只让账号 provider fail-closed，不阻断其他 provider。
                        let next = Arc::new(fail_closed(&builtin_revision, &builtin));
                        self.state().snapshot = Some(next.clone());
                        self.inner.runtime.set_account_layer((*next).clone()).await;
                        self.settle(generation, Ok(next));
                        continue;
                    }
                    self.settle(generation, Err(error));
                    // 失败期间新到的原因仍在 pending：继续下一轮回应它们。
                }
            }
        }
    }

    /// 一轮解析：连接结果 → overlay providers 与 states（`createAccountProviderConfigResolver`）。
    async fn resolve(
        &self,
        builtin: &Value,
        latest: Option<&AccountLayer>,
        reasons: &[String],
    ) -> Result<(Map<String, Value>, Map<String, Value>), String> {
        let configured = configured_account_providers(builtin);
        let connections = self.connections(&configured, reasons).await?;
        let by_id: HashMap<&str, &Connection> = connections
            .iter()
            .map(|connection| (connection.provider_id.as_str(), connection))
            .collect();
        let mut providers = Map::new();
        for (id, access) in &configured {
            let fallback = Connection::new(id, "unknown");
            let connection = by_id.get(id.as_str()).copied().unwrap_or(&fallback);
            let entitlement =
                |entitled: bool| json!({"access": {"type": "zhipu-account", "entitled": entitled}});
            let config = match connection.status {
                "available" | "pending" => {
                    let entitled = connection.status == "available";
                    if access["mode"] == "start-plan" {
                        let models: Vec<String> = connection
                            .models
                            .clone()
                            .unwrap_or_default()
                            .into_iter()
                            .map(|model| model.trim().to_owned())
                            .filter(|model| !model.is_empty())
                            .collect();
                        json!({"access": {"type": "zhipu-account", "entitled": entitled}, "builtinModelIds": models})
                    } else {
                        entitlement(entitled)
                    }
                }
                "unavailable" => entitlement(false),
                _ => match latest
                    .filter(|_| !connection.reset_previous)
                    .and_then(|latest| latest.providers.get(id))
                {
                    Some(previous) => previous.clone(),
                    None => entitlement(false),
                },
            };
            providers.insert(id.clone(), config);
        }
        let mut states = Map::new();
        for connection in &connections {
            let previous = if connection.reset_previous {
                None
            } else {
                latest.and_then(|latest| latest.states.get(&connection.provider_id))
            };
            let entitled = providers[&connection.provider_id]["access"]["entitled"] == true;
            let mut state = Map::new();
            if connection.status == "unknown"
                && let Some(Value::Object(previous)) = previous
            {
                state = previous.clone();
            }
            let availability = match (connection.status, previous) {
                ("unknown", Some(previous)) => previous["availability"].clone(),
                (status, _) => json!(status),
            };
            state.insert("availability".into(), availability);
            state.insert("entitled".into(), json!(entitled));
            let reason = match (connection.status, previous) {
                ("unknown", Some(previous)) => previous.get("unavailableReason").cloned(),
                ("unavailable", _) => connection.unavailable_reason.map(|r| json!(r)),
                _ => None,
            };
            match reason {
                Some(reason) => {
                    state.insert("unavailableReason".into(), reason);
                }
                None => {
                    state.shift_remove("unavailableReason");
                }
            }
            if let Some(current) = connection.current {
                state.insert("current".into(), json!(current));
            }
            match &connection.connection_key {
                Some(key) => {
                    state.insert("connectionKey".into(), json!(key));
                }
                None => {
                    state.shift_remove("connectionKey");
                }
            }
            if let Some(effective_at) = connection.effective_at {
                state.insert("effectiveAt".into(), json!(effective_at));
            }
            states.insert(connection.provider_id.clone(), Value::Object(state));
        }
        Ok((providers, states))
    }

    /// `createAccountProviderConnectionResolver`。
    async fn connections(
        &self,
        configured: &[(String, Value)],
        reasons: &[String],
    ) -> Result<Vec<Connection>, String> {
        let settings = read_account_settings(&self.inner.settings).await;
        let force_refresh = reasons
            .iter()
            .any(|reason| is_credential_refresh_reason(reason));
        let mut identities: HashMap<String, Option<String>> = HashMap::new();
        let mut availability: HashMap<String, Availability> = HashMap::new();
        for family in ["zai", "bigmodel"] {
            let family_providers: Vec<(&String, String)> = configured
                .iter()
                .filter(|(_, access)| {
                    access["accountType"] == family
                        && access["mode"]
                            .as_str()
                            .is_some_and(|mode| mode != "off-peak")
                })
                .map(|(id, access)| (id, access["mode"].as_str().unwrap_or("").to_owned()))
                .collect();
            if family_providers.is_empty() {
                continue;
            }
            let queryable: Vec<&(&String, String)> = family_providers
                .iter()
                .filter(|(id, kind)| {
                    if settings.unresolved_families.iter().any(|f| f == family)
                        && kind != "start-plan"
                    {
                        availability.insert((*id).clone(), Availability::Unknown);
                        return false;
                    }
                    true
                })
                .collect();
            if queryable.is_empty() {
                continue;
            }
            let identity = self.identity(&mut identities, family).await;
            let Some(identity) = identity else {
                for (id, _) in &queryable {
                    availability.insert(
                        (*id).clone(),
                        Availability::Unavailable("coding_plan_not_connected"),
                    );
                }
                continue;
            };
            let mut providers = Vec::new();
            for (id, kind) in &queryable {
                let api_key = if kind != "team-coding-plan" {
                    self.inner
                        .keys
                        .load(id, family, &identity, force_refresh)
                        .await
                } else {
                    None
                };
                providers.push(AvailabilityProvider {
                    provider_id: (*id).clone(),
                    family: family.to_owned(),
                    plan_kind: kind.clone(),
                    api_key,
                });
            }
            let resolved = self
                .inner
                .availability
                .family(family, &providers, &settings.selections)
                .await;
            for (id, _) in &queryable {
                availability.insert(
                    (*id).clone(),
                    resolved.get(*id).cloned().unwrap_or(Availability::Unknown),
                );
            }
        }

        let mut connections = Vec::new();
        let mut scopes = HashMap::new();
        let previous_scopes = self.state().previous_scopes.clone();
        for (id, access) in configured {
            let (Some(family), Some(mode)) =
                (access["accountType"].as_str(), access["mode"].as_str())
            else {
                connections.push(Connection::new(id, "unavailable"));
                continue;
            };
            let selection = settings.selection(family);
            let identity = self.identity(&mut identities, family).await;
            let team_scope =
                if mode == "team-coding-plan" && selection["kind"] == "team-coding-plan" {
                    json!([
                        selection["organizationId"],
                        selection["projectId"],
                        selection["productId"]
                    ])
                } else {
                    Value::Null
                };
            let scope = json!([identity, team_scope]).to_string();
            let reset_previous = previous_scopes
                .get(id)
                .is_some_and(|previous| *previous != scope);
            scopes.insert(id.clone(), scope);
            if mode == "off-peak" {
                let selected = selection["kind"].as_str();
                let matching = settings.domain.as_deref() == Some(family)
                    && matches!(
                        selected,
                        Some("individual-coding-plan" | "team-coding-plan")
                    )
                    && configured.iter().any(|(candidate, candidate_access)| {
                        candidate_access["accountType"] == family
                            && candidate_access["mode"].as_str() == selected
                            && matches!(
                                availability.get(candidate),
                                Some(Availability::Available { .. })
                            )
                    });
                connections.push(Connection::new(
                    id,
                    if matching { "available" } else { "unavailable" },
                ));
                continue;
            }
            let result = availability
                .get(id)
                .cloned()
                .unwrap_or(Availability::Unknown);
            let mut connection = match &result {
                Availability::Available { models } => {
                    let mut connection = Connection::new(id, "available");
                    connection.models = models.clone();
                    connection
                }
                Availability::Pending {
                    effective_at,
                    models,
                } => {
                    let mut connection = Connection::new(id, "pending");
                    connection.models = Some(models.clone());
                    connection.effective_at = Some(*effective_at);
                    connection
                }
                Availability::Unavailable(reason) => {
                    let mut connection = Connection::new(id, "unavailable");
                    connection.unavailable_reason = Some(account_reason(reason));
                    connection
                }
                Availability::Unknown => Connection::new(id, "unknown"),
            };
            connection.current = Some(
                settings.domain.as_deref() == Some(family)
                    && if mode == "start-plan" {
                        identity.is_some()
                    } else {
                        selection["kind"].as_str() == Some(mode)
                    },
            );
            let key_selection = if mode == "start-plan" {
                json!({"kind": "start-plan"})
            } else if selection.is_null() {
                Value::Null
            } else {
                selection.clone()
            };
            connection.connection_key = Some(sha256_hex(
                &json!([identity, family, key_selection]).to_string(),
            ));
            connection.reset_previous = reset_previous;
            connections.push(connection);
        }
        // 查询可能跨越切账号 / 换套餐：身份或设置变了就丢弃本轮结果，不推进 previousScopes。
        for (family, captured) in &identities {
            let current = self
                .inner
                .auth
                .account_identity(family)
                .await
                .map(|id| id.trim().to_owned());
            if current != *captured {
                return Err("账号查询期间连接或身份发生变化，丢弃过期结果".to_owned());
            }
        }
        let latest_settings: AccountSettings = read_account_settings(&self.inner.settings).await;
        if latest_settings != settings {
            return Err("账号查询期间连接或身份发生变化，丢弃过期结果".to_owned());
        }
        self.state().previous_scopes = scopes;
        Ok(connections)
    }

    async fn identity(
        &self,
        cache: &mut HashMap<String, Option<String>>,
        family: &str,
    ) -> Option<String> {
        if let Some(identity) = cache.get(family) {
            return identity.clone();
        }
        let identity = self
            .inner
            .auth
            .account_identity(family)
            .await
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty());
        cache.insert(family.to_owned(), identity.clone());
        identity
    }
}
