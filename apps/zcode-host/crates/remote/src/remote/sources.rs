//! window-controller 的远端 source（`windowHostControllerService.ts` 的 remote 分支与
//! `windowHostControllerProjection.ts`），规则见 rust-host-remote.md §2.7：
//!
//! ```text
//! 查询 scope ─► registry 按 (path, identity) 找 session ─► 远端 source（在线：读远端三个分区）
//!                                                    └─ identity 是远端 identity 但无 session：跳过
//! 远端 workspace 事件 ─► 强制重读；远端 sessions-index（existing-only）─► 实时摘要叠加
//! 断线 ─► disconnect：行改为 offline；重连 ─► replace：首个快照成功时一帧内移除旧行、加入新行
//! 释放 / bind ─► remove：task.removed + workspace.removed
//! ```
//!
//! 投影变化经 [`Controller::publish_external`] 推送，与本地行共用同一个 `{logEpoch, seq}`。
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, Weak};

use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio::task::AbortHandle;
use zcode_host_kit::{RpcError, RpcValue, log};
use zcode_host_rpc::RpcClient;

use super::RemoteHandle;
use super::registry::{ConnState, RemoteRegistry, RemoteScope};
use crate::agent::wire::{Assembled, Assembler};
use crate::tasks::Partitions;
use crate::tasks::controller::{
    self, Controller, ExternalSources, ExternalUpdate, matches_kind, merge_partitions,
};

fn fault(message: &str) -> RpcError {
    RpcError::new("Error", message)
}

fn workspace_key(scope: &RemoteScope) -> String {
    super::registry::workspace_key(&scope.workspace_path, Some(&scope.workspace_identity))
}

fn source_key(scope: &RemoteScope) -> String {
    format!(
        "remote\0{}\0{}",
        scope.remote_session_id,
        workspace_key(scope)
    )
}

fn task_key(scope: &RemoteScope, task_id: &str) -> String {
    format!(
        "{}\0{}\0{task_id}",
        scope.remote_session_id,
        workspace_key(scope)
    )
}

fn address_for(scope: &RemoteScope, task_id: &str) -> Value {
    json!({
        "remoteSessionId": scope.remote_session_id,
        "workspacePath": scope.workspace_path,
        "workspaceIdentity": scope.workspace_identity,
        "taskId": task_id,
    })
}

fn workspace_target(scope: &RemoteScope) -> Value {
    json!({"workspacePath": scope.workspace_path, "workspaceIdentity": scope.workspace_identity})
}

fn removed_workspace(scope: &RemoteScope) -> Value {
    json!({
        "op": "workspace.removed",
        "remoteSessionId": scope.remote_session_id,
        "workspacePath": scope.workspace_path,
        "workspaceIdentity": scope.workspace_identity,
    })
}

/// normalizeTaskMeta：身份字段以 source 为准。
fn normalize_meta(meta: &Value, scope: &RemoteScope) -> Value {
    let mut meta = meta.as_object().cloned().unwrap_or_default();
    meta.insert("workspacePath".into(), json!(scope.workspace_path));
    meta.insert("workspaceIdentity".into(), json!(scope.workspace_identity));
    Value::Object(meta)
}

struct Source {
    scope: RemoteScope,
    online: bool,
    connection_state: &'static str,
    memberships: Option<Vec<(Value, Value)>>,
    overlays: HashMap<String, Value>,
    rows: BTreeMap<String, Value>,
    /// 已读过快照的远端连接（句柄地址）；换了连接要重读。
    loaded_for: Option<usize>,
    events: Option<AbortHandle>,
    observer: Option<Observer>,
}

/// 远端 sessions-index 观察任务与它当前的订阅（停止时反向退订）。
struct Observer {
    task: AbortHandle,
    subscription: Arc<Mutex<Option<String>>>,
    client: RpcClient,
}

impl Source {
    fn new(scope: RemoteScope) -> Self {
        Self {
            scope,
            online: false,
            connection_state: "connecting",
            memberships: None,
            overlays: HashMap::new(),
            rows: BTreeMap::new(),
            loaded_for: None,
            events: None,
            observer: None,
        }
    }

    fn stop(&mut self) {
        if let Some(task) = self.events.take() {
            task.abort();
        }
        if let Some(observer) = self.observer.take() {
            observer.task.abort();
            let subscription = observer
                .subscription
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(subscription_id) = subscription {
                let mut params = workspace_target(&self.scope);
                params["subscriptionId"] = json!(subscription_id);
                params["runtimePolicy"] = json!("existing-only");
                let call = observer.client.call(
                    "zcode-agent",
                    "unsubscribeSessionsIndexV4",
                    vec![RpcValue::Json(params)],
                );
                tokio::spawn(async move {
                    let _ = call.await;
                });
            }
        }
    }

    fn fact(&self) -> Value {
        json!({
            "remoteSessionId": self.scope.remote_session_id,
            "workspacePath": self.scope.workspace_path,
            "workspaceIdentity": self.scope.workspace_identity,
            "sourceAvailability": if self.online { "online" } else { "offline" },
            "connectionState": self.connection_state,
        })
    }

    /// buildSourceRows：tasks-index 行叠加远端 sessions-index 摘要（规则同本地）。
    fn build_rows(&self) -> BTreeMap<String, Value> {
        let mut rows = BTreeMap::new();
        for (base_meta, membership) in self.memberships.iter().flatten() {
            let Some(task_id) = base_meta["taskId"].as_str() else {
                continue;
            };
            let summary = self.overlays.get(task_id);
            let mut meta = base_meta.clone();
            if let (Some(summary), Some(object)) = (summary, meta.as_object_mut()) {
                let custom = summary["titleSource"] == "custom";
                if summary["title"].is_string() && (base_meta["titleOverridden"] != true || custom)
                {
                    object.insert("title".into(), summary["title"].clone());
                }
                if custom {
                    object.insert("titleOverridden".into(), json!(true));
                }
                if summary["lastActivityAt"].is_number() {
                    object.insert("updatedAt".into(), summary["lastActivityAt"].clone());
                }
                match controller::status_from_phase(&summary["phase"]) {
                    Some(status) => {
                        object.insert("status".into(), json!(status));
                    }
                    None => {
                        object.shift_remove("status");
                    }
                }
                match controller::pending_interaction(summary) {
                    Some(pending) => {
                        object.insert("pendingInteraction".into(), pending);
                    }
                    None => {
                        object.shift_remove("pendingInteraction");
                    }
                }
            }
            let live = match summary {
                Some(summary) => controller::live_status(summary),
                None => match meta["status"].as_str() {
                    Some("completed") => "completed",
                    Some("error") => "error",
                    _ => "idle",
                },
            };
            let mut row = Map::new();
            row.insert("address".into(), address_for(&self.scope, task_id));
            row.insert("meta".into(), meta);
            row.insert("membership".into(), membership.clone());
            row.insert(
                "sourceAvailability".into(),
                json!(if self.online { "online" } else { "offline" }),
            );
            row.insert("liveStatus".into(), json!(live));
            if let Some(summary) = summary {
                row.insert("activity".into(), controller::activity(summary));
            }
            rows.insert(task_key(&self.scope, task_id), Value::Object(row));
        }
        rows
    }
}

#[derive(Default)]
struct State {
    sources: BTreeMap<String, Source>,
    /// 新 source key → 仍在借用离线行的上一代 scope。
    pending_replacement: HashMap<String, RemoteScope>,
}

impl State {
    fn rows(&self) -> BTreeMap<String, Value> {
        self.sources
            .values()
            .flat_map(|source| {
                source
                    .rows
                    .iter()
                    .map(|(key, row)| (key.clone(), row.clone()))
            })
            .collect()
    }

    fn facts(&self) -> Vec<Value> {
        self.sources.values().map(Source::fact).collect()
    }
}

/// 远端解析结果。
enum Resolution {
    /// 不是远端：交给本地 source。
    Local,
    /// 远端 identity 但没有 logical session（或解析歧义）：跳过，绝不落回本地。
    Skip,
    Remote(RemoteScope, bool),
}

pub struct RemoteSources {
    registry: RemoteRegistry<RemoteHandle>,
    controller: Mutex<Weak<Controller>>,
    state: Mutex<State>,
    refresh_lock: tokio::sync::Mutex<()>,
    me: Weak<RemoteSources>,
}

fn diff(previous: &BTreeMap<String, Value>, next: &BTreeMap<String, Value>) -> Vec<Value> {
    let mut deltas = Vec::new();
    for (key, row) in previous {
        if !next.contains_key(key) {
            deltas.push(json!({"op": "task.removed", "address": row["address"]}));
        }
    }
    for (key, row) in next {
        if previous.get(key) != Some(row) {
            deltas.push(json!({"op": "task.upserted", "task": row}));
        }
    }
    deltas
}

impl RemoteSources {
    pub fn new(registry: RemoteRegistry<RemoteHandle>) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            registry,
            controller: Mutex::new(Weak::new()),
            state: Mutex::new(State::default()),
            refresh_lock: tokio::sync::Mutex::new(()),
            me: me.clone(),
        })
    }

    /// 挂到 window-controller：之后的查询会经 [`ExternalSources`] 认领远端 scope。
    pub fn attach_to(self: &Arc<Self>, controller: &Arc<Controller>) {
        *self
            .controller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::downgrade(controller);
        controller.set_external(self.clone());
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn publish(&self, state: &State, task_deltas: Vec<Value>, workspace_deltas: Vec<Value>) {
        if task_deltas.is_empty() && workspace_deltas.is_empty() {
            return;
        }
        let controller = self
            .controller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .upgrade();
        if let Some(controller) = controller {
            controller.publish_external(ExternalUpdate {
                rows: state.rows(),
                workspaces: state.facts(),
                task_deltas,
                workspace_deltas,
            });
        }
    }

    fn resolve(&self, scope: &Value) -> Resolution {
        let Some(path) = scope["workspacePath"].as_str() else {
            return Resolution::Local;
        };
        let identity = scope["workspaceIdentity"].as_str();
        let remote_identity = identity.is_some_and(|identity| {
            zcode_shared::workspace_identity::parse_remote(identity).is_some()
        });
        match self.registry.find_session_for_workspace(path, identity) {
            Ok(Some(session)) => match session.scope() {
                Some(scope) => {
                    Resolution::Remote(scope, session.online && session.state == ConnState::Online)
                }
                None if remote_identity => Resolution::Skip,
                None => Resolution::Local,
            },
            Ok(None) if remote_identity => Resolution::Skip,
            Ok(None) => Resolution::Local,
            Err(error) => {
                log::warn(format!("window Controller source resolve failed: {error}"));
                if remote_identity {
                    Resolution::Skip
                } else {
                    Resolution::Local
                }
            }
        }
    }

    fn handle(&self, scope: &RemoteScope) -> Option<Arc<RemoteHandle>> {
        self.registry.resolve_scoped(scope).ok()
    }

    /// 登记并在需要时读取远端分区（`refreshSource`）。
    #[expect(clippy::await_holding_invalid_type, reason = "存量，U2 改为 actor")]
    pub async fn refresh_source(
        &self,
        scope: &RemoteScope,
        online: bool,
        force: bool,
    ) -> Result<(), RpcError> {
        let _guard = self.refresh_lock.lock().await;
        let key = source_key(scope);
        let handle = if online { self.handle(scope) } else { None };
        let was_online = {
            let mut state = self.lock();
            state
                .sources
                .entry(key.clone())
                .or_insert_with(|| Source::new(scope.clone()))
                .online
        };
        if handle.is_none() && was_online {
            self.disconnect_source(scope);
            return Ok(());
        }
        let Some(handle) = handle else {
            return Ok(());
        };
        let handle_id = Arc::as_ptr(&handle) as usize;
        self.ensure_live(scope, &handle);
        if !force
            && self
                .lock()
                .sources
                .get(&key)
                .and_then(|source| source.loaded_for)
                == Some(handle_id)
        {
            return Ok(());
        }
        let partitions = read_partitions(&handle.connection.client, scope).await?;
        let memberships: Vec<(Value, Value)> = merge_partitions(partitions)
            .into_iter()
            .map(|(meta, membership)| (normalize_meta(&meta, scope), membership))
            .collect();
        let mut state = self.lock();
        let replaced = state.pending_replacement.remove(&key);
        let replaced_source = replaced
            .as_ref()
            .filter(|previous| source_key(previous) != key)
            .and_then(|previous| state.sources.remove(&source_key(previous)));
        let Some(source) = state.sources.get_mut(&key) else {
            return Ok(());
        };
        let previous_fact = source.fact();
        let previous_rows = std::mem::take(&mut source.rows);
        source.memberships = Some(memberships);
        source.online = true;
        source.connection_state = "online";
        source.loaded_for = Some(handle_id);
        source.rows = source.build_rows();
        let mut task_deltas = Vec::new();
        let mut workspace_deltas = Vec::new();
        if let Some(mut replaced_source) = replaced_source {
            // 重连换了 remoteSessionId：一个 task 帧里同时发布旧行移除与新行加入，消费者看不到空投影。
            replaced_source.stop();
            for row in replaced_source.rows.values() {
                task_deltas.push(json!({"op": "task.removed", "address": row["address"]}));
            }
            workspace_deltas.push(removed_workspace(&replaced_source.scope));
        }
        task_deltas.extend(diff(&previous_rows, &source.rows));
        let fact = source.fact();
        if fact != previous_fact || !workspace_deltas.is_empty() {
            workspace_deltas.push(json!({"op": "workspace.upserted", "workspace": fact}));
        }
        let state = &*state;
        self.publish(state, task_deltas, workspace_deltas);
        Ok(())
    }

    /// 在线 source 的远端事件订阅与 sessions-index 观察者（每个 source 各一个，换连接时重建）。
    fn ensure_live(&self, scope: &RemoteScope, handle: &Arc<RemoteHandle>) {
        let key = source_key(scope);
        let mut state = self.lock();
        let Some(source) = state.sources.get_mut(&key) else {
            return;
        };
        let handle_id = Arc::as_ptr(handle) as usize;
        if source.loaded_for.is_some_and(|loaded| loaded != handle_id) {
            source.stop();
        }
        if source.events.is_none() {
            let mut events = handle.connection.client.listen(
                "zcode-task",
                "onDynamicWorkspaceEvent",
                RpcValue::Json(workspace_target(scope)),
            );
            let (me, scope) = (self.me.clone(), scope.clone());
            source.events = Some(
                tokio::spawn(async move {
                    while events.next().await.is_some() {
                        let Some(sources) = me.upgrade() else { return };
                        if let Err(error) = sources.refresh_source(&scope, true, true).await {
                            log::warn(format!(
                                "window Controller source refresh failed, scope=remote: {}",
                                error.message
                            ));
                        }
                    }
                })
                .abort_handle(),
            );
        }
        if source.observer.is_none() {
            let subscription = Arc::new(Mutex::new(None));
            source.observer = Some(Observer {
                task: tokio::spawn(observe_sessions(
                    self.me.clone(),
                    handle.connection.client.clone(),
                    scope.clone(),
                    subscription.clone(),
                ))
                .abort_handle(),
                subscription,
                client: handle.connection.client.clone(),
            });
        }
    }

    fn apply_overlays(&self, scope: &RemoteScope, sessions: HashMap<String, Value>) {
        let key = source_key(scope);
        let mut state = self.lock();
        let Some(source) = state.sources.get_mut(&key) else {
            return;
        };
        if !source.online {
            return;
        }
        source.overlays = sessions;
        let next = source.build_rows();
        let deltas: Vec<Value> = next
            .iter()
            .filter(|(key, row)| source.rows.get(*key) != Some(row))
            .map(|(_, row)| json!({"op": "task.upserted", "task": row}))
            .collect();
        source.rows = next;
        let state = &*state;
        self.publish(state, deltas, Vec::new());
    }

    /// 断线：行改为 offline、workspace 事实为 offline / disconnected（`disconnectSource`）。
    pub fn disconnect_source(&self, scope: &RemoteScope) {
        let key = source_key(scope);
        let mut state = self.lock();
        let Some(source) = state.sources.get_mut(&key) else {
            return;
        };
        if !source.online && source.connection_state == "disconnected" {
            return;
        }
        source.stop();
        source.online = false;
        source.connection_state = "disconnected";
        source.loaded_for = None;
        let mut deltas = Vec::new();
        for row in source.rows.values_mut() {
            row["sourceAvailability"] = json!("offline");
            deltas.push(json!({"op": "task.upserted", "task": row.clone()}));
        }
        let fact = source.fact();
        let state = &*state;
        self.publish(
            state,
            deltas,
            vec![json!({"op": "workspace.upserted", "workspace": fact})],
        );
    }

    /// 重连成功：新 source 首次读取成功时原子替换上一代离线投影；失败时继续展示旧的离线行。
    pub async fn replace_disconnected_source(&self, previous: &RemoteScope, next: &RemoteScope) {
        {
            let mut state = self.lock();
            let previous_key = source_key(previous);
            let retained = state
                .pending_replacement
                .remove(&previous_key)
                .unwrap_or_else(|| previous.clone());
            if source_key(&retained) != previous_key {
                // 连续重连都在首个快照前失败：可信行仍属于更早的一代，摘掉中间的空 source。
                if let Some(mut middle) = state.sources.remove(&previous_key) {
                    middle.stop();
                    let deltas = middle
                        .rows
                        .values()
                        .map(|row| json!({"op": "task.removed", "address": row["address"]}))
                        .collect();
                    let state_ref = &*state;
                    self.publish(state_ref, deltas, vec![removed_workspace(previous)]);
                }
            }
            state.pending_replacement.insert(source_key(next), retained);
            state
                .sources
                .entry(source_key(next))
                .or_insert_with(|| Source::new(next.clone()));
        }
        if let Err(error) = self.refresh_source(next, true, true).await {
            // source 已连接但 tasks-index 暂时不可读：保留离线可信投影，后续查询成功后再原子替换。
            log::warn(format!(
                "failed to atomically replace disconnected Controller source: {}",
                error.message
            ));
        }
    }

    /// 释放或换代：发全部行的 task.removed 与 workspace.removed（连同仍被借用的上一代）。
    pub fn remove_source(&self, scope: &RemoteScope) {
        let key = source_key(scope);
        let mut state = self.lock();
        let Some(mut source) = state.sources.remove(&key) else {
            return;
        };
        source.stop();
        let mut task_deltas: Vec<Value> = source
            .rows
            .values()
            .map(|row| json!({"op": "task.removed", "address": row["address"]}))
            .collect();
        let mut workspace_deltas = vec![removed_workspace(scope)];
        let retained = state.pending_replacement.remove(&key);
        state
            .pending_replacement
            .retain(|_, previous| source_key(previous) != key);
        if let Some(retained) = retained.filter(|retained| source_key(retained) != key)
            && let Some(mut previous) = state.sources.remove(&source_key(&retained))
        {
            previous.stop();
            task_deltas.extend(
                previous
                    .rows
                    .values()
                    .map(|row| json!({"op": "task.removed", "address": row["address"]})),
            );
            workspace_deltas.push(removed_workspace(&retained));
        }
        let state = &*state;
        self.publish(state, task_deltas, workspace_deltas);
    }

    fn selected_keys(&self, scopes: &[Value]) -> Vec<(String, RemoteScope, bool)> {
        let mut selected = Vec::new();
        for scope in scopes {
            if let Resolution::Remote(scope, online) = self.resolve(scope) {
                selected.push((source_key(&scope), scope, online));
            }
        }
        selected
    }

    async fn search_items(&self, query: &Value, scopes: &[Value]) -> Vec<Value> {
        let mut items = Vec::new();
        for (key, scope, online) in self.selected_keys(scopes) {
            let Some(handle) = online.then(|| self.handle(&scope)).flatten() else {
                continue;
            };
            let mut scoped = query.as_object().cloned().unwrap_or_default();
            scoped.shift_remove("limit");
            scoped.insert("workspaceScopes".into(), json!([workspace_target(&scope)]));
            let result = match handle
                .connection
                .client
                .call(
                    "zcode-task",
                    "listTaskList",
                    vec![RpcValue::Json(Value::Object(scoped))],
                )
                .await
            {
                Ok(result) => result.into_json(),
                Err(error) => {
                    log::warn(format!(
                        "window Controller source search failed, scope=remote: {}",
                        error.message
                    ));
                    continue;
                }
            };
            let state = self.lock();
            let source = state.sources.get(&key);
            for item in result["items"].as_array().into_iter().flatten() {
                let normalized = normalize_meta(item, &scope);
                let task_id = normalized["taskId"].as_str().unwrap_or_default();
                let projected =
                    source.and_then(|source| source.rows.get(&task_key(&scope, task_id)));
                let mut entry = projected
                    .and_then(|row| row["meta"].as_object().cloned())
                    .unwrap_or_else(|| normalized.as_object().cloned().unwrap_or_default());
                entry.insert("remoteSessionId".into(), json!(scope.remote_session_id));
                entry.insert("sourceAvailability".into(), json!("online"));
                let live = match projected {
                    Some(row) => row["liveStatus"].clone(),
                    None => json!(match normalized["status"].as_str() {
                        Some("completed") => "completed",
                        Some("error") => "error",
                        _ => "idle",
                    }),
                };
                entry.insert("liveStatus".into(), live);
                if let Some(activity) = projected
                    .map(|row| &row["activity"])
                    .filter(|a| a.is_object())
                {
                    entry.insert("activity".into(), activity.clone());
                }
                if let Some(snippets) = item.get("searchSnippets") {
                    entry.insert("searchSnippets".into(), snippets.clone());
                }
                items.push(Value::Object(entry));
            }
        }
        items
    }

    fn projected_items(&self, scopes: &[Value], kind: &str) -> Vec<Value> {
        let mut keys: Vec<String> = Vec::new();
        let state_keys = self.selected_keys(scopes);
        let state = self.lock();
        for (key, _, _) in state_keys {
            if let Some(previous) = state.pending_replacement.get(&key) {
                // 新 source 的首个快照失败时继续展示上一代离线可信投影。
                keys.push(source_key(previous));
            }
            keys.push(key);
        }
        keys.iter()
            .filter_map(|key| state.sources.get(key))
            .flat_map(|source| source.rows.values())
            .filter(|row| matches_kind(&row["membership"], kind))
            .map(|row| {
                let mut item = row["meta"].as_object().cloned().unwrap_or_default();
                item.insert(
                    "remoteSessionId".into(),
                    row["address"]["remoteSessionId"].clone(),
                );
                item.insert(
                    "sourceAvailability".into(),
                    row["sourceAvailability"].clone(),
                );
                item.insert("liveStatus".into(), row["liveStatus"].clone());
                if row["activity"].is_object() {
                    item.insert("activity".into(), row["activity"].clone());
                }
                Value::Object(item)
            })
            .collect()
    }

    /// 列表写操作（`executeMutation`）：source 必须唯一且在线，写远端后强制重读。
    pub async fn mutate(&self, address: &Value, mutation: &Value) -> Result<Value, RpcError> {
        let scope = RemoteScope {
            remote_session_id: address["remoteSessionId"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            workspace_path: address["workspacePath"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            workspace_identity: address["workspaceIdentity"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        };
        let task_id = address["taskId"].as_str().unwrap_or_default().to_owned();
        let kind = mutation["kind"].as_str().unwrap_or_default().to_owned();
        let key = source_key(&scope);
        {
            let state = self.lock();
            let source = state.sources.get(&key);
            let deleting_archived =
                matches!(kind.as_str(), "delete-archived" | "delete-archived-batch");
            match source {
                Some(source)
                    if deleting_archived
                        || source.rows.contains_key(&task_key(&scope, &task_id)) =>
                {
                    if !source.online {
                        return Err(fault("远程 source 当前离线，禁止列表写操作"));
                    }
                }
                _ => return Err(fault("没有与任务地址匹配的 source")),
            }
        }
        let handle = self
            .handle(&scope)
            .ok_or_else(|| fault("远程 source 当前离线，禁止列表写操作"))?;
        let client = &handle.connection.client;
        let base = json!({
            "taskId": task_id,
            "workspacePath": scope.workspace_path,
            "workspaceIdentity": scope.workspace_identity,
        });
        let with = |extra: Value| {
            let mut params = base.as_object().cloned().unwrap_or_default();
            if let Some(extra) = extra.as_object() {
                params.extend(extra.clone());
            }
            vec![RpcValue::Json(Value::Object(params))]
        };
        let call =
            |method: &'static str, args: Vec<RpcValue>| client.call("zcode-task", method, args);
        let mut result = Value::Null;
        match kind.as_str() {
            "pin" => {
                call(
                    "setTaskPinned",
                    with(json!({"pinned": mutation["pinned"] == true})),
                )
                .await?;
            }
            "archive" if mutation["archived"] == true => {
                call("archiveTask", with(json!({}))).await?;
            }
            "archive" => {
                call("unarchiveTask", with(json!({}))).await?;
            }
            "delete" => {
                call("deleteTask", with(json!({}))).await?;
            }
            "delete-archived" => {
                result = call("deleteArchivedTask", with(json!({})))
                    .await?
                    .into_json();
            }
            "delete-archived-batch" => {
                result = call(
                    "deleteArchivedTasks",
                    vec![RpcValue::Json(json!({
                        "workspacePath": scope.workspace_path,
                        "workspaceIdentity": scope.workspace_identity,
                        "taskIds": mutation["taskIds"],
                    }))],
                )
                .await?
                .into_json();
            }
            "mark-read" => {
                let mut extra = json!({"unread": false});
                if !mutation["expectedUnreadAt"].is_null() {
                    extra["expectedUnreadAt"] = mutation["expectedUnreadAt"].clone();
                }
                call("setTaskUnread", with(extra)).await?;
            }
            "mark-unread" => {
                call("setTaskUnread", with(json!({"unread": true}))).await?;
            }
            // open / resume 只验证唯一 source；对话随后仍走 scoped channel。
            "open" | "resume" => {}
            other => return Err(fault(&format!("未知 Controller mutation: {other}"))),
        }
        if !matches!(kind.as_str(), "open" | "resume")
            && let Err(error) = self.refresh_source(&scope, true, true).await
        {
            // 写入已成功：列表重读失败不改报失败，后续事件仍会刷新投影。
            log::warn(format!(
                "window Controller source refresh failed, scope=remote: {}",
                error.message
            ));
        }
        if matches!(kind.as_str(), "delete-archived" | "delete-archived-batch") {
            return Ok(result);
        }
        Ok(self
            .lock()
            .sources
            .get(&key)
            .and_then(|source| source.rows.get(&task_key(&scope, &task_id)))
            .map(|row| row["meta"].clone())
            .unwrap_or(Value::Null))
    }

    /// 远端 attachment 上的 zcode-task 列表写操作（`createControllerRoutedTaskService`）：参数里的
    /// path / identity 必须与 attachment scope 一致，再经投影写远端。
    pub async fn mutate_for_attachment(
        &self,
        attachment: &RemoteScope,
        params: &Value,
        mutation: Value,
    ) -> Result<Value, RpcError> {
        if params["workspacePath"].as_str() != Some(attachment.workspace_path.as_str())
            || params["workspaceIdentity"].as_str() != Some(attachment.workspace_identity.as_str())
        {
            return Err(fault("列表 mutation 与 remote attachment scope 不匹配"));
        }
        let session = self
            .registry
            .get_session(&attachment.remote_session_id)
            .filter(|session| session.scope().as_ref() == Some(attachment))
            .ok_or_else(|| fault("列表 mutation 与 remote attachment source 不匹配"))?;
        let online = session.online && session.state == ConnState::Online;
        self.refresh_source(attachment, online, false).await?;
        let task_id = params["taskId"]
            .as_str()
            .or_else(|| params["taskIds"][0].as_str())
            .unwrap_or_default();
        let mut address = address_for(attachment, task_id);
        if mutation["kind"] == "delete-archived-batch" {
            address["taskId"] = json!(task_id);
        }
        self.mutate(&address, &mutation).await
    }
}

impl ExternalSources for RemoteSources {
    fn claims(&self, scope: &Value) -> bool {
        !matches!(self.resolve(scope), Resolution::Local)
    }

    fn refresh<'a>(&'a self, scopes: &'a [Value]) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            for (_, scope, online) in self.selected_keys(scopes) {
                if let Err(error) = self.refresh_source(&scope, online, false).await {
                    // 一个远端 source 的瞬时读取失败不能清空其他 source 的可信投影。
                    log::warn(format!(
                        "window Controller source refresh failed, scope=remote: {}",
                        error.message
                    ));
                }
            }
        })
    }

    fn list_items<'a>(
        &'a self,
        query: &'a Value,
        scopes: &'a [Value],
    ) -> BoxFuture<'a, Vec<Value>> {
        Box::pin(async move {
            let searching = query["search"]
                .as_str()
                .is_some_and(|search| !search.trim().is_empty());
            if searching {
                self.search_items(query, scopes).await
            } else {
                self.projected_items(scopes, query["kind"].as_str().unwrap_or("timeline"))
            }
        })
    }

    fn mutate<'a>(
        &'a self,
        address: &'a Value,
        mutation: &'a Value,
    ) -> BoxFuture<'a, Result<Value, RpcError>> {
        Box::pin(RemoteSources::mutate(self, address, mutation))
    }
}

async fn read_partitions(client: &RpcClient, scope: &RemoteScope) -> Result<Partitions, RpcError> {
    let request = || vec![RpcValue::Json(workspace_target(scope))];
    let (active, pinned, archived) = futures_util::future::try_join3(
        client.call("zcode-task", "listTasks", request()),
        client.call("zcode-task", "listPinnedTasks", request()),
        client.call("zcode-task", "listArchivedTasks", request()),
    )
    .await?;
    let list = |value: RpcValue| match value.into_json() {
        Value::Array(items) => items,
        _ => Vec::new(),
    };
    Ok(Partitions {
        active: list(active),
        pinned: list(pinned),
        archived: list(archived),
    })
}

/// 远端 sessions-index 观察者（`windowHostSessionsIndexObserver.ts`）：用 existing-only 订阅，不拉起远端
/// CLI；远端 runtime unavailable 时清空摘要，available 后重新订阅。帧经重组，seq 缺口时 forceSnapshot。
async fn observe_sessions(
    sources: Weak<RemoteSources>,
    client: RpcClient,
    scope: RemoteScope,
    current: Arc<Mutex<Option<String>>>,
) {
    let key = workspace_key(&scope);
    let target = workspace_target(&scope);
    let remember = |subscription: Option<String>| {
        *current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = subscription;
    };
    let mut lifecycle = client.listen(
        "zcode-agent",
        "onAgentRuntimeLifecycle",
        RpcValue::Undefined,
    );
    let mut frames = client.listen(
        "zcode-agent",
        "onDynamicSessionsIndexFrame",
        RpcValue::Json(target.clone()),
    );
    let publish = |sessions: HashMap<String, Value>| {
        if let Some(sources) = sources.upgrade() {
            sources.apply_overlays(&scope, sessions);
        }
    };
    loop {
        let mut params = target.as_object().cloned().unwrap_or_default();
        params.insert("visibility".into(), json!("background"));
        params.insert("runtimePolicy".into(), json!("existing-only"));
        let subscribed = client
            .call(
                "zcode-agent",
                "subscribeSessionsIndexV4",
                vec![RpcValue::Json(Value::Object(params))],
            )
            .await;
        let subscription_id = match subscribed {
            Ok(ack) => ack.into_json()["ack"]["subscriptionId"]
                .as_str()
                .map(str::to_owned),
            Err(_) => None,
        };
        remember(subscription_id.clone());
        let Some(subscription_id) = subscription_id else {
            // 远端没有在跑的 runtime：等它 available 再订阅，不为列表查询拉起 CLI。
            loop {
                let Some(event) = lifecycle.next().await else {
                    return;
                };
                let event = event.into_json();
                if event["workspaceKey"] == key.as_str() && event["state"] == "available" {
                    break;
                }
            }
            continue;
        };
        let mut sessions: HashMap<String, Value> = HashMap::new();
        let mut assembler = Assembler::new();
        let mut cursor: Option<(Value, u64)> = None;
        let restart = loop {
            tokio::select! {
                event = lifecycle.next() => {
                    let Some(event) = event else { return };
                    let event = event.into_json();
                    if event["workspaceKey"] == key.as_str() && event["state"] != "available" {
                        publish(HashMap::new());
                        // 等 available 后重新订阅。
                        loop {
                            let Some(event) = lifecycle.next().await else { return };
                            let event = event.into_json();
                            if event["workspaceKey"] == key.as_str() && event["state"] == "available" {
                                break;
                            }
                        }
                        break true;
                    }
                }
                wire = frames.next() => {
                    let Some(wire) = wire else { return };
                    let wire = wire.into_json();
                    if wire["subscriptionId"] != subscription_id.as_str() {
                        continue;
                    }
                    let logical = match assembler.accept(&wire) {
                        Assembled::Pending => continue,
                        Assembled::Frame { frame, .. } => Some(frame),
                        Assembled::Fault(_) => None,
                    };
                    let mut recover = logical.is_none();
                    if let Some(logical) = logical {
                        let payload = &logical["payload"];
                        let to_seq = logical["toSeq"].as_u64().unwrap_or_default();
                        match payload["kind"].as_str() {
                            Some("snapshot") => {
                                sessions = payload["snapshot"]["sessions"]
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .filter_map(|session| {
                                        Some((session["sessionId"].as_str()?.to_owned(), session.clone()))
                                    })
                                    .collect();
                                cursor = Some((payload["snapshot"]["logEpoch"].clone(), to_seq));
                                publish(sessions.clone());
                            }
                            Some("deltas") if cursor.as_ref().map(|cursor| cursor.1) == logical["fromSeq"].as_u64() => {
                                for delta in payload["deltas"].as_array().into_iter().flatten() {
                                    match delta["op"].as_str() {
                                        Some("session.upserted") => {
                                            if let Some(id) = delta["session"]["sessionId"].as_str() {
                                                sessions.insert(id.to_owned(), delta["session"].clone());
                                            }
                                        }
                                        Some("session.removed") => {
                                            if let Some(id) = delta["sessionId"].as_str() {
                                                sessions.remove(id);
                                            }
                                        }
                                        _ => recover = true,
                                    }
                                }
                                if let Some(cursor) = cursor.as_mut() {
                                    cursor.1 = to_seq;
                                }
                                publish(sessions.clone());
                            }
                            _ => recover = true,
                        }
                    }
                    if recover {
                        assembler.reset();
                        let mut params = target.as_object().cloned().unwrap_or_default();
                        params.insert("subscriptionId".into(), json!(subscription_id));
                        params.insert(
                            "base".into(),
                            cursor
                                .as_ref()
                                .map(|(epoch, seq)| json!({"logEpoch": epoch, "seq": seq}))
                                .unwrap_or(Value::Null),
                        );
                        params.insert("forceSnapshot".into(), json!(true));
                        params.insert("runtimePolicy".into(), json!("existing-only"));
                        if client
                            .call("zcode-agent", "resyncSessionsIndexV4", vec![RpcValue::Json(Value::Object(params))])
                            .await
                            .is_err()
                        {
                            break true;
                        }
                    }
                }
            }
        };
        remember(None);
        if restart {
            let mut params = target.as_object().cloned().unwrap_or_default();
            params.insert("subscriptionId".into(), json!(subscription_id));
            params.insert("runtimePolicy".into(), json!("existing-only"));
            let _ = client
                .call(
                    "zcode-agent",
                    "unsubscribeSessionsIndexV4",
                    vec![RpcValue::Json(Value::Object(params))],
                )
                .await;
        }
    }
}
