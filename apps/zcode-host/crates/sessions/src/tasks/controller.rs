//! `window-controller` channel（packages/desktop/src/host/windowHostControllerService.ts、
//! windowHostControllerProjection.ts）：tasks-index 行与 sessions-index 实时摘要在内存里的投影。
//!
//! - 行集合只来自 tasks-index 的三个分区；sessions-index 只给已有行叠加标题、活性与待处理交互。
//! - 两个 topic（`controller/tasks-index`、`controller/workspaces`）各有一个 `{logEpoch, seq}`；每个
//!   增量帧让 seq 恰好加一。订阅与 resync 都回整份快照（`fromSeq: 0`）。
//! - 帧按连接分发：每条连接只收到自己订阅的 subscriptionId。
//! - 列表读取有缓存：source 第一次被查询时读取分区，之后只有 workspace 事件或修改才强制重读。
//!
//! 与 Node 的差异：只有本地 source。搜索（`search`）逐个 source 调用 zcode-task 的全文检索
//! `listTaskList`，再叠加投影行的实时状态（rust-host-tasks.md §2.3）。
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use zcode_host_kit::{Args, EventHub, Overflow, Reply, RpcError, ServiceBuilder, log};

use super::{Partitions, TaskSource, WorkspaceEvents};
use crate::agent::WorkspaceTarget;
use crate::agent::service::uuid_v4;
use crate::agent::sessions_index::{IndexEvent, SessionsIndexObserver};
use crate::support::collate::locale_compare;

pub const CHANNEL: &str = "window-controller";
const TASKS_TOPIC: &str = "controller/tasks-index";
const WORKSPACES_TOPIC: &str = "controller/workspaces";

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn fault(message: &str) -> RpcError {
    RpcError::new("Error", message)
}

struct Topic {
    log_epoch: String,
    seq: u64,
}

struct Source {
    target: WorkspaceTarget,
    /// `(meta, membership)`；None 表示还没读过分区。
    memberships: Option<Vec<(Value, Value)>>,
    rows: BTreeMap<String, Value>,
}

struct State {
    sources: BTreeMap<String, Source>,
    tasks: Topic,
    workspaces: Topic,
    /// subscriptionId → (连接, topic)
    subscribers: HashMap<String, (u64, &'static str)>,
    /// 远端 source 的行与 workspace 事实（由 [`ExternalSources`] 推送，rust-host-remote.md §2.7）。
    external: ExternalProjection,
}

/// 远端 source 的投影快照；与本地行共用同一个 `{logEpoch, seq}`。
#[derive(Default)]
struct ExternalProjection {
    rows: BTreeMap<String, Value>,
    workspaces: Vec<Value>,
}

/// 窗口内非本地的 source（远端 workspace，rust-host-remote.md §2.7）。实现方自己读取远端
/// tasks-index 与实时摘要，经 [`Controller::publish_external`] 推送变化；controller 只合并。
pub trait ExternalSources: Send + Sync {
    /// 查询 scope 不属于本地：远端 session 存在，或 identity 是远端 identity（此时跳过，绝不读本地）。
    fn claims(&self, scope: &Value) -> bool;
    /// 列表查询前刷新被认领的 scope；单个 source 失败不影响其他 source。
    fn refresh<'a>(&'a self, scopes: &'a [Value]) -> futures_util::future::BoxFuture<'a, ()>;
    /// 被认领 scope 的列表项（已按 kind 过滤、未排序）；带 `search` 时逐 source 调用远端全文检索。
    fn list_items<'a>(
        &'a self,
        query: &'a Value,
        scopes: &'a [Value],
    ) -> futures_util::future::BoxFuture<'a, Vec<Value>>;
    /// address 带 `remoteSessionId` 的列表写操作；返回投影里的最新 meta。
    fn mutate<'a>(
        &'a self,
        address: &'a Value,
        mutation: &'a Value,
    ) -> futures_util::future::BoxFuture<'a, Result<Value, RpcError>>;
}

/// 一次远端 source 变化：最新的行与 workspace 事实，以及要发出的增量。
pub struct ExternalUpdate {
    pub rows: BTreeMap<String, Value>,
    pub workspaces: Vec<Value>,
    pub task_deltas: Vec<Value>,
    pub workspace_deltas: Vec<Value>,
}

pub struct Controller {
    tasks: Arc<dyn TaskSource>,
    observer: Arc<SessionsIndexObserver>,
    state: Mutex<State>,
    frames: EventHub<Arc<(u64, Value)>>,
    /// 串行化分区读取与应用，避免较早的读取结果覆盖较新的。
    refresh_lock: tokio::sync::Mutex<()>,
    external: Mutex<Option<Arc<dyn ExternalSources>>>,
}

/// readSourceTaskIndex：active → pinned → archived，同一 taskId 后者覆盖前者。
pub fn merge_partitions(partitions: Partitions) -> Vec<(Value, Value)> {
    let mut order: Vec<String> = Vec::new();
    let mut merged: HashMap<String, (Value, Value)> = HashMap::new();
    let groups = [
        (
            partitions.active,
            json!({"pinned": false, "archived": false, "active": true}),
        ),
        (
            partitions.pinned,
            json!({"pinned": true, "archived": false, "active": true}),
        ),
        (
            partitions.archived,
            json!({"pinned": false, "archived": true, "active": false}),
        ),
    ];
    for (metas, membership) in groups {
        for meta in metas {
            let Some(task_id) = meta["taskId"].as_str().map(str::to_owned) else {
                continue;
            };
            if !merged.contains_key(&task_id) {
                order.push(task_id.clone());
            }
            merged.insert(task_id, (meta, membership.clone()));
        }
    }
    order
        .into_iter()
        .filter_map(|task_id| merged.remove(&task_id))
        .collect()
}

fn address_for(target: &WorkspaceTarget, task_id: &str) -> Value {
    let mut address = Map::new();
    address.insert("workspacePath".into(), json!(target.workspace_path));
    if let Some(identity) = target
        .workspace_identity
        .as_deref()
        .map(str::trim)
        .filter(|identity| !identity.is_empty())
    {
        address.insert("workspaceIdentity".into(), json!(identity));
    }
    address.insert("taskId".into(), json!(task_id));
    Value::Object(address)
}

fn task_key(target: &WorkspaceTarget, task_id: &str) -> String {
    format!("local\0{}\0{task_id}", target.key())
}

/// sessionOverlay 的 liveStatus。
pub fn live_status(summary: &Value) -> &'static str {
    let summary_count = &summary["pendingInteractionSummary"];
    let pending = summary["pendingInteraction"].is_object()
        || summary_count["permissionCount"].as_u64().unwrap_or(0)
            + summary_count["userInputCount"].as_u64().unwrap_or(0)
            > 0;
    if pending {
        return "waiting";
    }
    match summary["phase"].as_str() {
        Some("prewarming" | "running") => "running",
        Some("error") => "error",
        Some("completedSuccess" | "completedInterrupted") => "completed",
        _ => "idle",
    }
}

pub fn activity(summary: &Value) -> Value {
    let mut activity = Map::new();
    activity.insert("phase".into(), summary["phase"].clone());
    activity.insert("lastActivityAt".into(), summary["lastActivityAt"].clone());
    activity.insert(
        "hasBackgroundWork".into(),
        json!(summary["hasBackgroundWork"] == true),
    );
    if summary["pendingInteractionSummary"].is_object() {
        activity.insert(
            "pendingInteractions".into(),
            summary["pendingInteractionSummary"].clone(),
        );
    }
    if summary["workflowActivity"].is_object() {
        activity.insert(
            "workflowActivity".into(),
            summary["workflowActivity"].clone(),
        );
    }
    Value::Object(activity)
}

pub fn status_from_phase(phase: &Value) -> Option<&'static str> {
    match phase.as_str() {
        Some("prewarming" | "running") => Some("running"),
        Some("completedSuccess" | "completedInterrupted") => Some("completed"),
        Some("error") => Some("error"),
        _ => None,
    }
}

pub fn pending_interaction(summary: &Value) -> Option<Value> {
    let pending = summary["pendingInteraction"].as_object()?;
    let mut out = Map::new();
    for key in ["interactionId", "kind", "toolName", "autoResolution"] {
        if let Some(value) = pending.get(key).filter(|value| !value.is_null()) {
            out.insert(key.to_owned(), value.clone());
        }
    }
    Some(Value::Object(out))
}

/// buildSourceRows：tasks-index 行叠加 sessions-index 摘要。
fn build_rows(source: &Source, sessions: &HashMap<String, Value>) -> BTreeMap<String, Value> {
    let mut rows = BTreeMap::new();
    for (base_meta, membership) in source.memberships.iter().flatten() {
        let Some(task_id) = base_meta["taskId"].as_str() else {
            continue;
        };
        let summary = sessions.get(task_id);
        let mut meta = base_meta.clone();
        if let (Some(summary), Some(object)) = (summary, meta.as_object_mut()) {
            let custom = summary["titleSource"] == "custom";
            if summary["title"].is_string() && (base_meta["titleOverridden"] != true || custom) {
                object.insert("title".into(), summary["title"].clone());
            }
            if custom {
                object.insert("titleOverridden".into(), json!(true));
            }
            if summary["lastActivityAt"].is_number() {
                object.insert("updatedAt".into(), summary["lastActivityAt"].clone());
            }
            match status_from_phase(&summary["phase"]) {
                Some(status) => {
                    object.insert("status".into(), json!(status));
                }
                None => {
                    object.remove("status");
                }
            }
            match pending_interaction(summary) {
                Some(pending) => {
                    object.insert("pendingInteraction".into(), pending);
                }
                None => {
                    object.remove("pendingInteraction");
                }
            }
        }
        let live = match summary {
            Some(summary) => live_status(summary),
            // 没有实时摘要说明当前没有可证明的运行；SQLite 里的 running 可能来自上次进程。
            None => match meta["status"].as_str() {
                Some("completed") => "completed",
                Some("error") => "error",
                _ => "idle",
            },
        };
        let mut row = Map::new();
        row.insert("address".into(), address_for(&source.target, task_id));
        row.insert("meta".into(), meta);
        row.insert("membership".into(), membership.clone());
        row.insert("sourceAvailability".into(), json!("online"));
        row.insert("liveStatus".into(), json!(live));
        if let Some(summary) = summary {
            row.insert("activity".into(), activity(summary));
        }
        rows.insert(task_key(&source.target, task_id), Value::Object(row));
    }
    rows
}

fn workspace_fact(target: &WorkspaceTarget) -> Value {
    let mut fact = Map::new();
    fact.insert("workspacePath".into(), json!(target.workspace_path));
    if let Some(identity) = target
        .workspace_identity
        .as_deref()
        .map(str::trim)
        .filter(|identity| !identity.is_empty())
    {
        fact.insert("workspaceIdentity".into(), json!(identity));
    }
    fact.insert("sourceAvailability".into(), json!("online"));
    fact.insert("connectionState".into(), json!("online"));
    Value::Object(fact)
}

fn updated_at(row: &Value) -> f64 {
    row["meta"]["updatedAt"].as_f64().unwrap_or(0.0)
}

impl State {
    fn all_rows(&self) -> Vec<Value> {
        let mut rows: Vec<(String, Value)> = self
            .sources
            .values()
            .flat_map(|source| {
                source
                    .rows
                    .iter()
                    .map(|(key, row)| (key.clone(), row.clone()))
            })
            .chain(
                self.external
                    .rows
                    .iter()
                    .map(|(key, row)| (key.clone(), row.clone())),
            )
            .collect();
        rows.sort_by(|(left_key, left), (right_key, right)| {
            updated_at(right)
                .total_cmp(&updated_at(left))
                .then_with(|| left_key.cmp(right_key))
        });
        rows.into_iter().map(|(_, row)| row).collect()
    }

    fn snapshot_frame(&self, subscription_id: &str, topic: &'static str) -> Value {
        let (state, snapshot) = if topic == TASKS_TOPIC {
            (
                &self.tasks,
                json!({"protocolVersion": 1, "logEpoch": self.tasks.log_epoch, "tasks": self.all_rows()}),
            )
        } else {
            let workspaces: Vec<Value> = self
                .sources
                .values()
                .map(|source| workspace_fact(&source.target))
                .chain(self.external.workspaces.iter().cloned())
                .collect();
            (
                &self.workspaces,
                json!({"protocolVersion": 1, "logEpoch": self.workspaces.log_epoch, "workspaces": workspaces}),
            )
        };
        json!({
            "topic": topic,
            "subscriptionId": subscription_id,
            "logEpoch": state.log_epoch,
            "fromSeq": 0,
            "toSeq": state.seq,
            "sentAt": now_ms(),
            "payload": {"kind": "snapshot", "snapshot": snapshot},
        })
    }

    /// 发出一个增量帧（seq 恰好加一），返回每个订阅者的 `(连接, 帧)`。
    fn delta_frames(&mut self, topic: &'static str, deltas: Vec<Value>) -> Vec<(u64, Value)> {
        if deltas.is_empty() {
            return Vec::new();
        }
        let state = if topic == TASKS_TOPIC {
            &mut self.tasks
        } else {
            &mut self.workspaces
        };
        let from_seq = state.seq;
        state.seq += 1;
        let (log_epoch, to_seq) = (state.log_epoch.clone(), state.seq);
        self.subscribers
            .iter()
            .filter(|(_, (_, subscribed))| *subscribed == topic)
            .map(|(subscription_id, (connection, _))| {
                (
                    *connection,
                    json!({
                        "topic": topic,
                        "subscriptionId": subscription_id,
                        "logEpoch": log_epoch,
                        "fromSeq": from_seq,
                        "toSeq": to_seq,
                        "sentAt": now_ms(),
                        "payload": {"kind": "deltas", "deltas": deltas},
                    }),
                )
            })
            .collect()
    }
}

fn diff_rows(previous: &BTreeMap<String, Value>, next: &BTreeMap<String, Value>) -> Vec<Value> {
    let mut deltas = Vec::new();
    for (key, row) in next {
        if previous.get(key) != Some(row) {
            deltas.push(json!({"op": "task.upserted", "task": row}));
        }
    }
    for (key, row) in previous {
        if !next.contains_key(key) {
            deltas.push(json!({"op": "task.removed", "address": row["address"]}));
        }
    }
    deltas
}

fn target_of(scope: &Value) -> Option<WorkspaceTarget> {
    if scope["remoteSessionId"].is_string() {
        return None;
    }
    WorkspaceTarget::from_value(scope).ok().map(|mut target| {
        target.remote_session_id = None;
        target
    })
}

pub fn matches_kind(membership: &Value, kind: &str) -> bool {
    let pinned = membership["pinned"] == true;
    let archived = membership["archived"] == true;
    match kind {
        "pinned" => pinned && !archived,
        "archived" => archived,
        "timeline" => !pinned && !archived,
        "active" => !archived,
        _ => false,
    }
}

impl Controller {
    pub fn start(
        tasks: Arc<dyn TaskSource>,
        observer: Arc<SessionsIndexObserver>,
        workspace_events: WorkspaceEvents,
    ) -> Arc<Self> {
        let controller = Arc::new(Self {
            tasks,
            observer: observer.clone(),
            state: Mutex::new(State {
                sources: BTreeMap::new(),
                tasks: Topic {
                    log_epoch: uuid_v4(),
                    seq: 0,
                },
                workspaces: Topic {
                    log_epoch: uuid_v4(),
                    seq: 0,
                },
                subscribers: HashMap::new(),
                external: ExternalProjection::default(),
            }),
            // 每条连接的 `zcode-task` 帧，订阅者是该连接的 Renderer 订阅（Gateway 立即搬进出站队列）。
            // 落后 1024 条说明转发卡住，结束它的订阅（docs/specs/rust-u2-task-scope.md §6.2）。
            frames: EventHub::new("task-controller-frames", 1024, Overflow::CloseSubscriber),
            refresh_lock: tokio::sync::Mutex::new(()),
            external: Mutex::new(None),
        });
        let mut index_events = Box::pin(observer.events().subscribe());
        let weak = Arc::downgrade(&controller);
        tokio::spawn(async move {
            while let Some(event) = index_events.next().await {
                let Some(controller) = weak.upgrade() else {
                    return;
                };
                let target = match event.as_ref() {
                    IndexEvent::Snapshot { target, .. }
                    | IndexEvent::Upserted { target, .. }
                    | IndexEvent::Removed { target, .. }
                    | IndexEvent::Offline { target } => target,
                };
                controller.apply_overlays(&target.key());
            }
        });
        // workspace 事件（新建、改名、未读、置顶……）一律强制重读该 source 的分区。
        let mut events = Box::pin(workspace_events.subscribe());
        let weak = Arc::downgrade(&controller);
        tokio::spawn(async move {
            while let Some(event) = events.next().await {
                let Some(controller) = weak.upgrade() else {
                    return;
                };
                let target = controller
                    .lock()
                    .sources
                    .get(&event.workspace_key)
                    .map(|source| source.target.clone());
                if let Some(target) = target
                    && let Err(error) = controller.refresh(&target, true).await
                {
                    log::warn(format!("controller refresh failed: {}", error.message));
                }
            }
        });
        controller
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 挂上远端 source（rust-host-remote.md §2.7）。
    pub fn set_external(&self, external: Arc<dyn ExternalSources>) {
        *self
            .external
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(external);
    }

    fn external(&self) -> Option<Arc<dyn ExternalSources>> {
        self.external
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 远端 source 变化：更新缓存的行与事实，并按同一个 seq 发出增量（task 先于 workspace）。
    pub fn publish_external(&self, update: ExternalUpdate) {
        let frames = {
            let mut state = self.lock();
            state.external = ExternalProjection {
                rows: update.rows,
                workspaces: update.workspaces,
            };
            let mut frames = state.delta_frames(TASKS_TOPIC, update.task_deltas);
            frames.extend(state.delta_frames(WORKSPACES_TOPIC, update.workspace_deltas));
            frames
        };
        self.emit(frames);
    }

    fn emit(&self, frames: Vec<(u64, Value)>) {
        for frame in frames {
            self.frames.emit(Arc::new(frame));
        }
    }

    fn apply_overlays(&self, key: &str) {
        let sessions = self.observer.sessions(key);
        let frames = {
            let mut state = self.lock();
            let Some(source) = state.sources.get_mut(key) else {
                return;
            };
            if source.memberships.is_none() {
                return;
            }
            let next = build_rows(source, &sessions);
            let deltas = diff_rows(&source.rows, &next);
            source.rows = next;
            state.delta_frames(TASKS_TOPIC, deltas)
        };
        self.emit(frames);
    }

    /// 登记 source（首次登记发 workspace.upserted），按需读取分区并更新投影。
    #[expect(clippy::await_holding_invalid_type, reason = "存量，U2 改为 actor")]
    pub async fn refresh(&self, target: &WorkspaceTarget, force: bool) -> Result<(), RpcError> {
        let _guard = self.refresh_lock.lock().await;
        let key = target.key();
        let (registered, loaded) = {
            let mut state = self.lock();
            let registered = !state.sources.contains_key(&key);
            let source = state.sources.entry(key.clone()).or_insert_with(|| Source {
                target: target.clone(),
                memberships: None,
                rows: BTreeMap::new(),
            });
            let loaded = source.memberships.is_some();
            let frames = if registered {
                let fact = workspace_fact(target);
                state.delta_frames(
                    WORKSPACES_TOPIC,
                    vec![json!({"op": "workspace.upserted", "workspace": fact})],
                )
            } else {
                Vec::new()
            };
            drop(state);
            self.emit(frames);
            (registered, loaded)
        };
        if loaded && !force && !registered {
            return Ok(());
        }
        let partitions = self.tasks.partitions(target.clone()).await?;
        let memberships = merge_partitions(partitions);
        let sessions = self.observer.sessions(&key);
        let frames = {
            let mut state = self.lock();
            let Some(source) = state.sources.get_mut(&key) else {
                return Ok(());
            };
            source.memberships = Some(memberships);
            let next = build_rows(source, &sessions);
            let deltas = diff_rows(&source.rows, &next);
            source.rows = next;
            state.delta_frames(TASKS_TOPIC, deltas)
        };
        self.emit(frames);
        Ok(())
    }

    pub async fn list_task_list(&self, query: &Value) -> Result<Value, RpcError> {
        let kind = query["kind"].as_str().unwrap_or("timeline");
        // 远端 scope 由 ExternalSources 认领：远端 history scope 未连接时也绝不落回本地 tasks-index。
        let external = self.external();
        let (claimed, local_scopes): (Vec<Value>, Vec<Value>) = query["workspaceScopes"]
            .as_array()
            .into_iter()
            .flatten()
            .cloned()
            .partition(|scope| {
                external
                    .as_ref()
                    .is_some_and(|external| external.claims(scope))
            });
        if let Some(external) = &external {
            external.refresh(&claimed).await;
        }
        let targets: Vec<WorkspaceTarget> = local_scopes.iter().filter_map(target_of).collect();
        for target in &targets {
            // 一个 source 读取失败不能清空其他 source 的投影。
            if let Err(error) = self.refresh(target, false).await {
                log::warn(format!(
                    "controller source refresh failed for {}: {}",
                    target.key(),
                    error.message
                ));
            }
        }
        let searching = query["search"]
            .as_str()
            .is_some_and(|search| !search.trim().is_empty());
        let mut items = if searching {
            self.search(query, &targets).await
        } else {
            self.projected_items(&targets, kind)
        };
        if let Some(external) = &external {
            items.extend(external.list_items(query, &claimed).await);
        }
        let sort_key = if query["sortBy"] == "created" {
            "createdAt"
        } else {
            "updatedAt"
        };
        items.sort_by(|left, right| {
            let at = |item: &Value| item[sort_key].as_f64().unwrap_or(0.0);
            at(right).total_cmp(&at(left)).then_with(|| {
                locale_compare(
                    left["taskId"].as_str().unwrap_or_default(),
                    right["taskId"].as_str().unwrap_or_default(),
                )
            })
        });
        let total = items.len();
        if let Some(limit) = query["limit"].as_u64() {
            items.truncate(limit as usize);
        }
        let has_more = total > items.len();
        Ok(json!({"items": items, "total": total, "hasMore": has_more}))
    }

    /// 全文搜索：逐个 source 调用 zcode-task `listTaskList`（只含该 source、不带 limit），
    /// 再与投影行对齐取实时状态；某个 source 失败只记日志，不影响其他 source。
    async fn search(&self, query: &Value, targets: &[WorkspaceTarget]) -> Vec<Value> {
        let mut items = Vec::new();
        for target in targets {
            let mut scoped = query.clone();
            if let Some(object) = scoped.as_object_mut() {
                object.remove("limit");
                let mut scope = Map::new();
                scope.insert("workspacePath".into(), json!(target.workspace_path));
                if let Some(identity) = target
                    .workspace_identity
                    .as_deref()
                    .filter(|id| !id.is_empty())
                {
                    scope.insert("workspaceIdentity".into(), json!(identity));
                }
                object.insert("workspaceScopes".into(), json!([scope]));
            }
            let result = match self.tasks.search(scoped).await {
                Ok(result) => result,
                Err(error) => {
                    log::warn(format!(
                        "controller source search failed for {}: {}",
                        target.key(),
                        error.message
                    ));
                    continue;
                }
            };
            let state = self.lock();
            let source = state.sources.get(&target.key());
            for item in result["items"].as_array().into_iter().flatten() {
                let Some(mut normalized) = item.as_object().cloned() else {
                    continue;
                };
                // normalizeTaskMeta：身份字段以 source 为准。
                normalized.insert("workspacePath".into(), json!(target.workspace_path));
                match target
                    .workspace_identity
                    .as_deref()
                    .filter(|id| !id.is_empty())
                {
                    Some(identity) => {
                        normalized.insert("workspaceIdentity".into(), json!(identity));
                    }
                    None => {
                        normalized.shift_remove("workspaceIdentity");
                    }
                }
                let task_id = normalized["taskId"].as_str().unwrap_or_default().to_owned();
                let projected =
                    source.and_then(|source| source.rows.get(&task_key(target, &task_id)));
                let mut entry = match projected.and_then(|row| row["meta"].as_object()) {
                    Some(meta) => meta.clone(),
                    None => normalized.clone(),
                };
                entry.insert("sourceAvailability".into(), json!("online"));
                let live_status = match projected {
                    Some(row) => row["liveStatus"].clone(),
                    None => json!(match normalized.get("status").and_then(Value::as_str) {
                        Some("completed") => "completed",
                        Some("error") => "error",
                        _ => "idle",
                    }),
                };
                entry.insert("liveStatus".into(), live_status);
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

    fn projected_items(&self, targets: &[WorkspaceTarget], kind: &str) -> Vec<Value> {
        let selected: Vec<String> = targets.iter().map(WorkspaceTarget::key).collect();
        {
            let state = self.lock();
            selected
                .iter()
                .filter_map(|key| state.sources.get(key))
                .flat_map(|source| source.rows.values())
                .filter(|row| matches_kind(&row["membership"], kind))
                .map(|row| {
                    let mut item = row["meta"].as_object().cloned().unwrap_or_default();
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
    }

    /// `deleteArchivedTask({address})`：删除已持久成功后再强制刷新；刷新失败不改写删除结果。
    pub async fn delete_archived_task(&self, address: &Value) -> Result<bool, RpcError> {
        let target =
            target_of(address).ok_or_else(|| fault("列表 mutation 无法解析唯一 source"))?;
        let task_id = address["taskId"].as_str().unwrap_or_default().to_owned();
        let deleted = self.tasks.delete_archived(target.clone(), task_id).await?;
        if let Err(error) = self.refresh(&target, true).await {
            log::warn(format!("controller refresh failed: {}", error.message));
        }
        Ok(deleted)
    }

    /// `deleteArchivedTasks({address, taskIds})`：整批结束后只刷新一次。
    pub async fn delete_archived_tasks(&self, params: &Value) -> Result<Value, RpcError> {
        let task_ids: Vec<String> = params["taskIds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|id| id.as_str().map(str::to_owned))
            .collect();
        if task_ids.is_empty() {
            return Ok(json!({"deletedTaskIds": [], "skippedTaskIds": [], "failedTaskIds": []}));
        }
        let target = target_of(&params["address"])
            .ok_or_else(|| fault("列表 mutation 无法解析唯一 source"))?;
        let result = self
            .tasks
            .delete_archived_batch(target.clone(), task_ids)
            .await?;
        if !result.is_object() {
            return Err(fault("归档删除批次未返回逐项目标结果"));
        }
        if let Err(error) = self.refresh(&target, true).await {
            log::warn(format!("controller refresh failed: {}", error.message));
        }
        Ok(result)
    }

    pub async fn mutate_task(&self, address: &Value, mutation: &Value) -> Result<Value, RpcError> {
        if address["remoteSessionId"].is_string() {
            let external = self
                .external()
                .ok_or_else(|| fault("没有与任务地址匹配的 source"))?;
            return external.mutate(address, mutation).await;
        }
        let target =
            target_of(address).ok_or_else(|| fault("列表 mutation 无法解析唯一 source"))?;
        let task_id = address["taskId"]
            .as_str()
            .filter(|task_id| !task_id.trim().is_empty())
            .ok_or_else(|| fault("列表 mutation 无法解析唯一 source"))?
            .to_owned();
        self.refresh(&target, false).await?;
        let row_key = task_key(&target, &task_id);
        let exists = self
            .lock()
            .sources
            .get(&target.key())
            .is_some_and(|source| source.rows.contains_key(&row_key));
        if !exists {
            return Err(fault("列表 mutation 无法解析唯一 source"));
        }
        if !matches!(mutation["kind"].as_str(), Some("open" | "resume")) {
            self.tasks
                .mutate(target.clone(), task_id.clone(), mutation.clone())
                .await?;
            self.refresh(&target, true).await?;
        }
        Ok(self
            .lock()
            .sources
            .get(&target.key())
            .and_then(|source| source.rows.get(&row_key))
            .map(|row| row["meta"].clone())
            .unwrap_or(Value::Null))
    }

    fn subscribe(&self, connection: u64, params: &Value) -> Result<Value, RpcError> {
        let topic = match params["topic"].as_str() {
            Some(TASKS_TOPIC) => TASKS_TOPIC,
            Some(WORKSPACES_TOPIC) => WORKSPACES_TOPIC,
            _ => return Err(fault("invalid controller topic")),
        };
        let subscription_id = uuid_v4();
        let (frame, log_epoch) = {
            let mut state = self.lock();
            state
                .subscribers
                .insert(subscription_id.clone(), (connection, topic));
            let frame = state.snapshot_frame(&subscription_id, topic);
            let log_epoch = frame["logEpoch"].clone();
            (frame, log_epoch)
        };
        self.emit(vec![(connection, frame)]);
        Ok(
            json!({"ack": {"subscriptionId": subscription_id, "mode": "snapshot", "logEpoch": log_epoch}}),
        )
    }

    fn resync(&self, connection: u64, params: &Value) -> Result<Value, RpcError> {
        let subscription_id = params["subscriptionId"].as_str().unwrap_or_default();
        let (frame, log_epoch) = {
            let state = self.lock();
            let Some((owner, topic)) = state.subscribers.get(subscription_id).copied() else {
                return Err(fault("unknown controller subscription"));
            };
            if owner != connection {
                return Err(fault("unknown controller subscription"));
            }
            let frame = state.snapshot_frame(subscription_id, topic);
            let log_epoch = frame["logEpoch"].clone();
            (frame, log_epoch)
        };
        self.emit(vec![(connection, frame)]);
        Ok(
            json!({"ack": {"subscriptionId": subscription_id, "mode": "snapshot", "logEpoch": log_epoch}}),
        )
    }

    fn unsubscribe(&self, connection: u64, params: &Value) {
        let subscription_id = params["subscriptionId"].as_str().unwrap_or_default();
        let mut state = self.lock();
        if state
            .subscribers
            .get(subscription_id)
            .is_some_and(|(owner, _)| *owner == connection)
        {
            state.subscribers.remove(subscription_id);
        }
    }

    fn connection_closed(&self, connection: u64) {
        self.lock()
            .subscribers
            .retain(|_, (owner, _)| *owner != connection);
    }
}

fn first(args: &Args) -> Value {
    args.raw(0).cloned().unwrap_or(Value::Null)
}

pub fn service(controller: Arc<Controller>) -> ServiceBuilder {
    let (delete_one, delete_batch) = (controller.clone(), controller.clone());
    let (list, mutate, subscribe, resync, unsubscribe, frames, closed) = (
        controller.clone(),
        controller.clone(),
        controller.clone(),
        controller.clone(),
        controller.clone(),
        controller.clone(),
        controller,
    );
    ServiceBuilder::new(CHANNEL)
        .method("listTaskList", move |_, args| {
            let controller = list.clone();
            async move {
                controller
                    .list_task_list(&first(&args))
                    .await
                    .map(Reply::Value)
            }
        })
        .method("deleteArchivedTask", move |_, args| {
            let controller = delete_one.clone();
            async move {
                let deleted = controller
                    .delete_archived_task(&first(&args)["address"])
                    .await?;
                Ok(Reply::Value(json!(deleted)))
            }
        })
        .method("deleteArchivedTasks", move |_, args| {
            let controller = delete_batch.clone();
            async move {
                controller
                    .delete_archived_tasks(&first(&args))
                    .await
                    .map(Reply::Value)
            }
        })
        .method("mutateTask", move |_, args| {
            let controller = mutate.clone();
            async move {
                let params = first(&args);
                controller
                    .mutate_task(&params["address"], &params["mutation"])
                    .await
                    .map(Reply::Value)
            }
        })
        .method("subscribeControllerV4", move |context, args| {
            let result = subscribe.subscribe(context.connection, &first(&args));
            async move { result.map(Reply::Value) }
        })
        .method("resyncControllerV4", move |context, args| {
            let result = resync.resync(context.connection, &first(&args));
            async move { result.map(Reply::Value) }
        })
        .method("unsubscribeControllerV4", move |context, args| {
            unsubscribe.unsubscribe(context.connection, &first(&args));
            async move { Ok(Reply::Undefined) }
        })
        .event("onDynamicControllerFrame", move |context, _| {
            let connection = context.connection;
            let events = frames
                .frames
                .subscribe_filtered(move |frame| frame.0 == connection)
                .map(|frame| Reply::Value(frame.1.clone()));
            Ok(Box::pin(events))
        })
        .on_connection_closed(move |context| closed.connection_closed(context.connection))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::future::BoxFuture;

    #[test]
    fn rows_overlay_live_summary_on_index_meta() {
        let source = Source {
            target: WorkspaceTarget::local("/w"),
            memberships: Some(merge_partitions(Partitions {
                active: vec![
                    json!({"taskId": "a", "title": "Index", "workspacePath": "/w", "updatedAt": 1, "status": "running"}),
                    json!({"taskId": "b", "title": "Mine", "titleOverridden": true, "workspacePath": "/w", "updatedAt": 2}),
                ],
                pinned: vec![
                    json!({"taskId": "b", "title": "Mine", "titleOverridden": true, "workspacePath": "/w", "updatedAt": 2}),
                ],
                archived: vec![],
            })),
            rows: BTreeMap::new(),
        };
        let sessions = HashMap::from([(
            "b".to_owned(),
            json!({"sessionId": "b", "title": "Generated", "phase": "running", "lastActivityAt": 9, "hasBackgroundWork": false,
                   "pendingInteraction": {"interactionId": "i", "kind": "permission"}}),
        )]);
        let rows = build_rows(&source, &sessions);
        let a = &rows[&task_key(&source.target, "a")];
        // 没有实时摘要：SQLite 里的 running 不能当作正在运行。
        assert_eq!(a["liveStatus"], "idle");
        assert_eq!(
            a["membership"],
            json!({"pinned": false, "archived": false, "active": true})
        );
        let b = &rows[&task_key(&source.target, "b")];
        assert_eq!(b["membership"]["pinned"], true);
        assert_eq!(
            b["meta"]["title"], "Mine",
            "manual title wins over generated"
        );
        assert_eq!(b["meta"]["updatedAt"], 9);
        assert_eq!(b["meta"]["status"], "running");
        assert_eq!(b["liveStatus"], "waiting");
        assert_eq!(b["activity"]["phase"], "running");
        assert!(matches_kind(&b["membership"], "pinned"));
        assert!(!matches_kind(&b["membership"], "timeline"));
    }

    struct Fake(Mutex<Partitions>);

    impl TaskSource for Fake {
        fn partitions(
            &self,
            _: WorkspaceTarget,
        ) -> BoxFuture<'static, Result<Partitions, RpcError>> {
            let partitions = self.0.lock().unwrap().clone();
            Box::pin(async move { Ok(partitions) })
        }
        fn mutate(
            &self,
            _: WorkspaceTarget,
            task_id: String,
            mutation: Value,
        ) -> BoxFuture<'static, Result<(), RpcError>> {
            let mut partitions = self.0.lock().unwrap();
            if mutation["kind"] == "pin" {
                let moved: Vec<Value> = partitions
                    .active
                    .iter()
                    .filter(|m| m["taskId"] == task_id.as_str())
                    .cloned()
                    .collect();
                partitions.pinned.extend(moved);
            }
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn frames_follow_seq_rules_and_mutations_refresh() {
        let runtime = crate::agent::AgentRuntime::new(
            crate::agent::AgentRuntimeConfig {
                base_env: Default::default(),
                command_vars: Default::default(),
                fallback_cwd: None,
            },
            crate::settings::SettingStore::new(std::path::Path::new("/nonexistent")),
            crate::providers::ProviderRuntime::new(None, "/nonexistent/p.json".into()),
        );
        let observer = SessionsIndexObserver::start(runtime);
        let events = crate::tasks::workspace_events();
        let fake = Arc::new(Fake(Mutex::new(Partitions {
            active: vec![
                json!({"taskId": "t1", "title": "T", "workspacePath": "/w", "createdAt": 1, "updatedAt": 1}),
            ],
            ..Default::default()
        })));
        let controller = Controller::start(fake, observer, events);
        let mut frames = Box::pin(controller.frames.subscribe_filtered(|frame| frame.0 == 7));
        let ack = controller
            .subscribe(7, &json!({"topic": TASKS_TOPIC}))
            .unwrap();
        let snapshot = frames.next().await.unwrap();
        assert_eq!(snapshot.1["payload"]["kind"], "snapshot");
        assert_eq!(snapshot.1["subscriptionId"], ack["ack"]["subscriptionId"]);
        assert_eq!(snapshot.1["fromSeq"], 0);

        let list = controller
            .list_task_list(&json!({"kind": "timeline", "workspaceScopes": [{"workspacePath": "/w"}], "sortBy": "updated"}))
            .await
            .unwrap();
        assert_eq!(list["total"], 1);
        assert_eq!(list["items"][0]["liveStatus"], "idle");
        let delta = frames.next().await.unwrap();
        assert_eq!(delta.1["payload"]["deltas"][0]["op"], "task.upserted");
        assert_eq!(
            (delta.1["fromSeq"].as_u64(), delta.1["toSeq"].as_u64()),
            (Some(0), Some(1))
        );

        let meta = controller
            .mutate_task(
                &json!({"workspacePath": "/w", "taskId": "t1"}),
                &json!({"kind": "pin", "pinned": true}),
            )
            .await
            .unwrap();
        assert_eq!(meta["taskId"], "t1");
        let pinned = controller
            .list_task_list(&json!({"kind": "pinned", "workspaceScopes": [{"workspacePath": "/w"}], "sortBy": "updated"}))
            .await
            .unwrap();
        assert_eq!(pinned["total"], 1);
        let missing = controller
            .mutate_task(
                &json!({"workspacePath": "/w", "taskId": "nope"}),
                &json!({"kind": "mark-read"}),
            )
            .await
            .unwrap_err();
        assert_eq!(missing.message, "列表 mutation 无法解析唯一 source");
    }
}
