//! 远端 attachment 上的 `zcode-task` / `zcode-session`（`remoteWorkspaceServiceCollection.ts` 的
//! createRemotePromptAttachment*、createReportingRemoteZCodeTaskService 与
//! createControllerRoutedTaskService），规则见 rust-host-remote.md §2.5：
//!
//! - `sendPrompt`、`enqueueTaskCommand`（zcode-session 只有 `sendPrompt`）：本机附件先经连接器上传到
//!   远端私有目录并改写路径，再转发；
//! - `sendPrompt` 到 task ready 之间计入该 workspace 的运行中任务（WSL 释放屏障）；
//! - `sendPrompt` 期间持有 task run lease，把用户消息与远端流事件镜像给 Main 的 realtime bus
//!   （`mirrorRemotePrompt`，rust-host-remote.md §2.9.7）；
//! - 列表写操作经 window-controller 投影解析唯一远端地址后写远端；
//! - 返回的 task meta 登记 session route，并转发远端 workspace 的会话消息请求给 Main。
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio::task::AbortHandle;
use zcode_host_control::{ControlSink, HostToMain};
use zcode_host_kit::{
    CallContext, CallFuture, EventHub, EventStream, Reply, RpcError, RpcValue, ServiceChannel, log,
};
use zcode_host_rpc::RpcClient;

use crate::tasks::realtime::TaskRealtimeBridge;

use super::registry::{RemoteScope, workspace_key};
use super::scope::ForwardChannel;
use super::sources::RemoteSources;

const TASK_CHANNEL: &str = "zcode-task";
const SESSION_CHANNEL: &str = "zcode-session";

type ReportFn = Arc<dyn Fn(&str, Option<&str>, u64) + Send + Sync>;

/// (workspacePath, workspaceIdentity)
type Context = (String, Option<String>);

#[derive(Default)]
struct TrackerState {
    /// taskId → (workspacePath, workspaceIdentity)
    metas: HashMap<String, (String, Option<String>)>,
    /// workspaceKey → (上下文, 运行中的 taskId)
    active: HashMap<String, (Context, HashSet<String>)>,
    ready: HashMap<String, AbortHandle>,
    workspaces: HashMap<String, AbortHandle>,
}

/// 每条远端连接一个：记住 task meta、跟踪运行中任务、转发会话消息请求。
pub struct TaskTracker {
    client: RpcClient,
    control: Arc<dyn ControlSink>,
    report: ReportFn,
    state: Mutex<TrackerState>,
    /// 跟踪中的任务 ready（一轮结束）：(workspaceKey, taskId)。Bot 订阅不可用时的终态兜底用它。
    ready_events: EventHub<(String, String)>,
}

fn identity_of(value: &Value) -> Option<String> {
    value["workspaceIdentity"]
        .as_str()
        .filter(|identity| !identity.is_empty())
        .map(str::to_owned)
}

fn target_of(path: &str, identity: Option<&str>) -> Value {
    let mut target = Map::new();
    target.insert("workspacePath".into(), json!(path));
    if let Some(identity) = identity {
        target.insert("workspaceIdentity".into(), json!(identity));
    }
    Value::Object(target)
}

impl TaskTracker {
    pub fn new(client: RpcClient, control: Arc<dyn ControlSink>, report: ReportFn) -> Arc<Self> {
        Arc::new(Self {
            client,
            control,
            report,
            state: Mutex::new(TrackerState::default()),
            ready_events: EventHub::new(),
        })
    }

    /// 已登记的 task meta：(workspacePath, workspaceIdentity)。
    pub fn meta(&self, task_id: &str) -> Option<(String, Option<String>)> {
        self.lock().metas.get(task_id).cloned()
    }

    pub fn ready_events(&self) -> &EventHub<(String, String)> {
        &self.ready_events
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TrackerState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `isZCodeTaskMeta`：taskId、workspacePath、traceId 都是字符串。
    fn remember_meta(self: &Arc<Self>, meta: &Value) {
        let (Some(task_id), Some(path), true) = (
            meta["taskId"].as_str(),
            meta["workspacePath"].as_str(),
            meta["traceId"].is_string(),
        ) else {
            return;
        };
        let identity = identity_of(meta);
        let key = workspace_key(path, identity.as_deref());
        let subscribe = {
            let mut state = self.lock();
            state
                .metas
                .insert(task_id.to_owned(), (path.to_owned(), identity.clone()));
            !state.workspaces.contains_key(&key)
        };
        if subscribe {
            let mut events = self.client.listen(
                TASK_CHANNEL,
                "onDynamicWorkspaceEvent",
                RpcValue::Json(target_of(path, identity.as_deref())),
            );
            let control = self.control.clone();
            let task = tokio::spawn(async move {
                while let Some(event) = events.next().await {
                    let event = event.into_json();
                    if event["type"] == "workspace_session_message_send_requested" {
                        control.send(HostToMain::Raw(json!({
                            "type": "session-message-send-requested",
                            "request": event["request"],
                        })));
                    }
                }
            });
            self.lock().workspaces.insert(key, task.abort_handle());
        }
        self.control.send(HostToMain::Raw(json!({
            "type": "session-route-announce",
            "route": {"sessionId": task_id},
        })));
    }

    pub(crate) fn remember_from_result(self: &Arc<Self>, result: &Value) {
        if let Some(items) = result.as_array() {
            for item in items {
                self.remember_from_result(item);
            }
            return;
        }
        self.remember_meta(result);
        if !result.is_object() {
            return;
        }
        for item in result["items"].as_array().into_iter().flatten() {
            self.remember_meta(item);
        }
        if result["snapshot"].is_object() {
            self.remember_meta(&result["snapshot"]["meta"]);
        }
        self.remember_meta(&result["meta"]);
    }

    fn report_count(&self, context: &(String, Option<String>), count: u64) {
        let (path, identity) = context;
        (self.report)(path, identity.as_deref(), count);
        let mut message = json!({
            "type": "workspace-running-task-count-changed",
            "workspacePath": path,
            "runningTaskCount": count,
        });
        if let Some(identity) = identity {
            message["workspaceIdentity"] = json!(identity);
        }
        self.control.send(HostToMain::Raw(message));
    }

    /// sendPrompt 只是远端 ACK：计数要等 task ready 才结束，避免误放行 WSL workspace 的释放。
    pub(crate) fn begin(self: &Arc<Self>, task_id: &str) -> bool {
        let (context, count) = {
            let mut state = self.lock();
            let Some(context) = state.metas.get(task_id).cloned() else {
                return false;
            };
            let key = workspace_key(&context.0, context.1.as_deref());
            let entry = state
                .active
                .entry(key)
                .or_insert_with(|| (context.clone(), HashSet::new()));
            if !entry.1.insert(task_id.to_owned()) {
                return false;
            }
            (context, entry.1.len() as u64)
        };
        self.report_count(&context, count);
        let mut ready = self.client.listen(
            TASK_CHANNEL,
            "onDynamicTaskReady",
            RpcValue::String(task_id.to_owned()),
        );
        let tracker = Arc::downgrade(self);
        let id = task_id.to_owned();
        let task = tokio::spawn(async move {
            if ready.next().await.is_some()
                && let Some(tracker) = tracker.upgrade()
            {
                if let Some((path, identity)) = tracker.meta(&id) {
                    tracker
                        .ready_events
                        .emit((workspace_key(&path, identity.as_deref()), id.clone()));
                }
                tracker.finish(&id);
            }
        });
        if let Some(previous) = self
            .lock()
            .ready
            .insert(task_id.to_owned(), task.abort_handle())
        {
            previous.abort();
        }
        true
    }

    pub(crate) fn finish(&self, task_id: &str) {
        let (context, count) = {
            let mut state = self.lock();
            if let Some(task) = state.ready.remove(task_id) {
                task.abort();
            }
            let Some(context) = state.metas.get(task_id).cloned() else {
                return;
            };
            let key = workspace_key(&context.0, context.1.as_deref());
            let Some(entry) = state.active.get_mut(&key) else {
                return;
            };
            if !entry.1.remove(task_id) {
                return;
            }
            let count = entry.1.len() as u64;
            if count == 0 {
                state.active.remove(&key);
            }
            (context, count)
        };
        self.report_count(&context, count);
    }

    fn clear_workspace(&self, params: &Value) {
        let Some(path) = params["workspacePath"].as_str() else {
            return;
        };
        let key = workspace_key(path, identity_of(params).as_deref());
        let mut state = self.lock();
        // pooled 连接不随 tab 退出：runtime 释放后同步解除代理层引用，避免 meta 与监听无限增长。
        if let Some(task) = state.workspaces.remove(&key) {
            task.abort();
        }
        let tasks: Vec<String> = state
            .metas
            .iter()
            .filter(|(_, (meta_path, identity))| {
                workspace_key(meta_path, identity.as_deref()) == key
            })
            .map(|(task_id, _)| task_id.clone())
            .collect();
        for task_id in tasks {
            state.metas.remove(&task_id);
            if let Some(task) = state.ready.remove(&task_id) {
                task.abort();
            }
        }
        state.active.remove(&key);
    }

    pub fn dispose(&self) {
        let mut state = self.lock();
        let state = &mut *state;
        for (_, task) in state.ready.drain().chain(state.workspaces.drain()) {
            task.abort();
        }
    }
}

/// 本机附件上传到远端并改写路径（`materializeRemotePromptAttachments`，由连接器执行）。
async fn materialize(
    backend: Option<&RpcClient>,
    params: Value,
    task_id: Option<&str>,
    trace_id: Option<&str>,
) -> Result<Value, RpcError> {
    let (Some(backend), Some(task_id), Some(trace_id), Some(content)) =
        (backend, task_id, trace_id, params["content"].as_str())
    else {
        return Ok(params);
    };
    let mut input = json!({"taskId": task_id, "traceId": trace_id, "content": content});
    if params["attachments"].is_array() {
        input["attachments"] = params["attachments"].clone();
    }
    let prepared = backend
        .call(
            "remote-backend",
            "materializePromptAttachments",
            vec![RpcValue::Json(input)],
        )
        .await?
        .into_json();
    let mut next = params.as_object().cloned().unwrap_or_default();
    next.insert("content".into(), prepared["content"].clone());
    if params.get("attachments").is_some() || !prepared["attachments"].is_null() {
        next.insert("attachments".into(), prepared["attachments"].clone());
    }
    Ok(Value::Object(next))
}

fn first_arg(args: &[RpcValue]) -> Value {
    args.first()
        .cloned()
        .map(RpcValue::into_json)
        .unwrap_or(Value::Null)
}

const LIST_MUTATIONS: &[&str] = &[
    "setTaskPinned",
    "archiveTask",
    "unarchiveTask",
    "deleteTask",
    "setTaskUnread",
    "deleteArchivedTasks",
    "deleteArchivedTask",
];

const REMEMBERED_RESULTS: &[&str] = &[
    "createTask",
    "resumeTask",
    "listTasks",
    "listPinnedTasks",
    "listTaskList",
    "listArchivedTasks",
    "getTaskMeta",
    "getTaskSnapshot",
    "getTaskSnapshotWithEtag",
];

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

/// 远端 `zcode-task` 的 wrapper（`createRemotePromptAttachmentTaskService` 包
/// `createReportingRemoteZCodeTaskService`）：每条远端连接一个，Renderer 的远端 attachment 与 Bot
/// 远端运行时共用，保证两条入口的附件物化、运行中任务与 realtime 镜像是同一份实现。
pub struct RemoteTaskOps {
    client: RpcClient,
    backend: Option<RpcClient>,
    tracker: Arc<TaskTracker>,
    /// InitLocal 带 hostId 时 tasks 区域建立的 bridge；没有时不做 lease 与镜像（同 Node 的
    /// `activeSessionRealtimePort ?? undefined`）。
    realtime: Option<Arc<TaskRealtimeBridge>>,
}

impl RemoteTaskOps {
    pub fn new(
        client: RpcClient,
        backend: Option<RpcClient>,
        tracker: Arc<TaskTracker>,
        realtime: Option<Arc<TaskRealtimeBridge>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            client,
            backend,
            tracker,
            realtime,
        })
    }

    pub fn client(&self) -> &RpcClient {
        &self.client
    }

    pub fn tracker(&self) -> &Arc<TaskTracker> {
        &self.tracker
    }

    /// 结果需要登记 task meta 的成员与 `releaseWorkspacePreparation`；请求立即发出（保持调用顺序），
    /// 结果保持远端编码。
    pub fn call_remembered(
        &self,
        method: &str,
        args: Vec<RpcValue>,
    ) -> BoxFuture<'static, Result<Vec<u8>, RpcError>> {
        let release = method == "releaseWorkspacePreparation";
        let params = first_arg(&args);
        let call = self.client.call_encoded(TASK_CHANNEL, method, args);
        let tracker = self.tracker.clone();
        Box::pin(async move {
            let encoded = call.await?;
            if release {
                tracker.clear_workspace(&params);
            } else if let Ok(result) = zcode_host_rpc::Reader::new(&encoded).value() {
                tracker.remember_from_result(&result.into_json());
            }
            Ok(encoded)
        })
    }

    /// 以 JSON 参数调用一个 zcode-task 成员（wrapper 处理的成员走 wrapper），结果解码成 JSON
    /// （undefined 为 null）。Bot 远端运行时用。
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let args = vec![RpcValue::Json(params)];
        let encoded = if method == "sendPrompt" || method == "enqueueTaskCommand" {
            self.send_prompt(method, first_arg(&args)).await?
        } else if REMEMBERED_RESULTS.contains(&method) || method == "releaseWorkspacePreparation" {
            self.call_remembered(method, args).await?
        } else {
            self.client.call_encoded(TASK_CHANNEL, method, args).await?
        };
        Ok(zcode_host_rpc::Reader::new(&encoded)
            .value()
            .map(RpcValue::into_json)
            .unwrap_or(Value::Null))
    }

    /// `sendPrompt` / `enqueueTaskCommand`：附件物化 → 运行中任务跟踪 → （sendPrompt）realtime 镜像。
    pub async fn send_prompt(&self, method: &str, params: Value) -> Result<Vec<u8>, RpcError> {
        let (task_id, trace_id) = (
            params_str(&params, "taskId"),
            params_str(&params, "traceId"),
        );
        let params = materialize(
            self.backend.as_ref(),
            params,
            task_id.as_deref(),
            trace_id.as_deref(),
        )
        .await?;
        let tracked = method == "sendPrompt"
            && params["content"].is_string()
            && params["traceId"].is_string()
            && params["taskId"]
                .as_str()
                .is_some_and(|task_id| self.tracker.begin(task_id));
        let result = if method == "sendPrompt" {
            self.mirrored(params.clone()).await
        } else {
            self.client
                .call_encoded(TASK_CHANNEL, method, vec![RpcValue::Json(params.clone())])
                .await
        };
        if result.is_err()
            && tracked
            && let Some(task_id) = params["taskId"].as_str()
        {
            self.tracker.finish(task_id);
        }
        result
    }

    /// `mirrorRemotePrompt`：远端 zcode-server 没有 desktop realtime port，由窗口 Host 持有 lease，
    /// 把用户消息与 ACK 前的远端流事件发给 Main 的 realtime bus，结束后释放 lease。
    async fn mirrored(&self, params: Value) -> Result<Vec<u8>, RpcError> {
        let send = |params: Value| {
            self.client
                .call_encoded(TASK_CHANNEL, "sendPrompt", vec![RpcValue::Json(params)])
        };
        let (Some(realtime), Some(task_id), Some(trace_id), true) = (
            self.realtime.as_ref(),
            params["taskId"].as_str().map(str::to_owned),
            params["traceId"].as_str().map(str::to_owned),
            params["content"].is_string(),
        ) else {
            return send(params).await;
        };
        let Some((path, identity)) = self.tracker.meta(&task_id) else {
            return send(params).await;
        };
        let mut target = Map::new();
        target.insert("workspacePath".into(), json!(path));
        if let Some(identity) = &identity {
            target.insert("workspaceIdentity".into(), json!(identity));
        }
        target.insert(
            "workspaceKey".into(),
            json!(workspace_key(&path, identity.as_deref())),
        );
        target.insert("taskId".into(), json!(task_id));
        target.insert("runId".into(), json!(trace_id));
        target.insert("traceId".into(), json!(trace_id));
        let target = Value::Object(target);
        let acquired = match realtime.acquire_task_run_lease(target.clone()).await {
            Ok(result) => result["acquired"] == true,
            Err(error) => {
                log::warn(format!(
                    "Bot remote runtime realtime lease failed: {}",
                    error.message
                ));
                false
            }
        };
        if !acquired {
            return send(params).await;
        }
        let mut op = Map::new();
        op.insert("kind".into(), json!("user_message"));
        op.insert("messageId".into(), json!(format!("user-{trace_id}")));
        op.insert("content".into(), params["content"].clone());
        if !params["attachments"].is_null() {
            op.insert("attachments".into(), params["attachments"].clone());
        }
        op.insert("timestamp".into(), json!(now_ms()));
        realtime.publish_stream_op(target.clone(), Value::Object(op));
        // 先挂监听再发送（同一条连接按序）；ACK 之前到达的事件先于结果处理，保持 Node 的到达顺序。
        let mut events = self.client.listen(
            TASK_CHANNEL,
            "onDynamicStreamEvent",
            RpcValue::String(task_id),
        );
        let call = send(params);
        tokio::pin!(call);
        let result = loop {
            tokio::select! {
                biased;
                event = events.next() => match event {
                    Some(event) => {
                        let event = event.into_json();
                        if event["type"] != "task_stream_mirror_batch"
                            && event["type"] != "task_snapshot_updated"
                        {
                            realtime.publish_stream_op(
                                target.clone(),
                                json!({"kind": "stream_event", "event": event}),
                            );
                        }
                    }
                    None => break (&mut call).await,
                },
                result = &mut call => break result,
            }
        };
        drop(events);
        realtime.release_task_run_lease(target);
        result
    }
}

pub struct RemoteTaskChannel {
    me: std::sync::Weak<RemoteTaskChannel>,
    ops: Arc<RemoteTaskOps>,
    sources: Arc<RemoteSources>,
    scope: RemoteScope,
    forward: Arc<ForwardChannel>,
}

impl RemoteTaskChannel {
    pub fn new(
        ops: Arc<RemoteTaskOps>,
        sources: Arc<RemoteSources>,
        scope: RemoteScope,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            forward: ForwardChannel::new(ops.client.clone(), TASK_CHANNEL),
            ops,
            sources,
            scope,
        })
    }

    async fn list_mutation(&self, method: &str, params: Value) -> Result<Reply, RpcError> {
        let mutation = match method {
            "setTaskPinned" => json!({"kind": "pin", "pinned": params["pinned"] == true}),
            "archiveTask" => json!({"kind": "archive", "archived": true}),
            "unarchiveTask" => json!({"kind": "archive", "archived": false}),
            "deleteTask" => json!({"kind": "delete"}),
            "deleteArchivedTask" => json!({"kind": "delete-archived"}),
            "deleteArchivedTasks" => {
                if params["taskIds"].as_array().is_none_or(Vec::is_empty) {
                    return Ok(Reply::Value(
                        json!({"deletedTaskIds": [], "skippedTaskIds": [], "failedTaskIds": []}),
                    ));
                }
                json!({"kind": "delete-archived-batch", "taskIds": params["taskIds"]})
            }
            _ if params["unread"] == true => json!({"kind": "mark-unread"}),
            _ => {
                let mut mutation = json!({"kind": "mark-read"});
                if !params["expectedUnreadAt"].is_null() {
                    mutation["expectedUnreadAt"] = params["expectedUnreadAt"].clone();
                }
                mutation
            }
        };
        let result = self
            .sources
            .mutate_for_attachment(&self.scope, &params, mutation)
            .await?;
        match method {
            "deleteTask" => Ok(Reply::Undefined),
            "deleteArchivedTask" => Ok(Reply::Value(json!(result == true))),
            "deleteArchivedTasks" => Ok(Reply::Value(result)),
            _ if result.is_null() => {
                let label = match method {
                    "setTaskPinned" => "pin",
                    "setTaskUnread" => "unread",
                    _ => "archive",
                };
                Err(RpcError::new(
                    "Error",
                    format!("{label} mutation 后 task 投影缺失"),
                ))
            }
            _ => Ok(Reply::Value(result)),
        }
    }
}

fn params_str(params: &Value, key: &str) -> Option<String> {
    params[key].as_str().map(str::to_owned)
}

impl ServiceChannel for RemoteTaskChannel {
    fn call(&self, context: &CallContext, method: &str, args: Vec<RpcValue>) -> CallFuture {
        let Some(channel) = self.me.upgrade() else {
            return self.forward.call(context, method, args);
        };
        let method = method.to_owned();
        if LIST_MUTATIONS.contains(&method.as_str()) {
            let params = first_arg(&args);
            return Box::pin(async move { channel.list_mutation(&method, params).await });
        }
        if method == "sendPrompt" || method == "enqueueTaskCommand" {
            let params = first_arg(&args);
            return Box::pin(async move {
                channel
                    .ops
                    .send_prompt(&method, params)
                    .await
                    .map(Reply::Encoded)
            });
        }
        if REMEMBERED_RESULTS.contains(&method.as_str()) || method == "releaseWorkspacePreparation"
        {
            // 结果保持远端编码原样交给 Renderer。
            let call = self.ops.call_remembered(&method, args);
            return Box::pin(async move { call.await.map(Reply::Encoded) });
        }
        self.forward.call(context, &method, args)
    }

    fn listen(
        &self,
        context: &CallContext,
        event: &str,
        arg: RpcValue,
    ) -> Result<EventStream, RpcError> {
        self.forward.listen(context, event, arg)
    }
}

pub struct RemoteSessionChannel {
    me: std::sync::Weak<RemoteSessionChannel>,
    client: RpcClient,
    backend: Option<RpcClient>,
    forward: Arc<ForwardChannel>,
}

impl RemoteSessionChannel {
    pub fn new(client: RpcClient, backend: Option<RpcClient>) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            forward: ForwardChannel::new(client.clone(), SESSION_CHANNEL),
            client,
            backend,
        })
    }
}

impl ServiceChannel for RemoteSessionChannel {
    fn call(&self, context: &CallContext, method: &str, args: Vec<RpcValue>) -> CallFuture {
        let channel = match self.me.upgrade() {
            Some(channel) if method == "sendPrompt" => channel,
            _ => return self.forward.call(context, method, args),
        };
        let params = first_arg(&args);
        Box::pin(async move {
            // zcode-session 的附件按 sessionId 归属，traceId 取 inputId（缺省时同 sessionId）。
            let task_id = params_str(&params, "sessionId");
            let trace_id = params_str(&params, "inputId").or_else(|| task_id.clone());
            let params = materialize(
                channel.backend.as_ref(),
                params,
                task_id.as_deref(),
                trace_id.as_deref(),
            )
            .await?;
            channel
                .client
                .call_encoded(SESSION_CHANNEL, "sendPrompt", vec![RpcValue::Json(params)])
                .await
                .map(Reply::Encoded)
        })
    }

    fn listen(
        &self,
        context: &CallContext,
        event: &str,
        arg: RpcValue,
    ) -> Result<EventStream, RpcError> {
        self.forward.listen(context, event, arg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn materialize_passes_through_without_backend_or_identity() {
        let params = json!({"taskId": "t", "traceId": "r", "content": "c"});
        assert_eq!(
            materialize(None, params.clone(), Some("t"), Some("r"))
                .await
                .unwrap(),
            params
        );
    }

    #[test]
    fn list_mutations_and_remembered_results_are_disjoint() {
        for method in LIST_MUTATIONS {
            assert!(!REMEMBERED_RESULTS.contains(method));
        }
    }
}
