use super::{Event, RunEvent};
use crate::{
    contract::{
        ClientMsg, Method, ModelIdentity, ModelPort, RuntimeClock, RuntimeError, RuntimePorts,
        ServerMsg, SessionStore, StorageCommitFailure, ToolPort,
    },
    domain::session::Session,
};
use anyhow::Result;
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Actor output: one ordered channel carrying replies, events and host requests.
pub type RuntimeOutput = mpsc::Sender<Vec<ServerMsg>>;
/// A request as the actor sees it; `token` echoes back in the reply.
pub(super) struct Call {
    pub token: u64,
    pub method: Method,
    pub params: Value,
}
pub(super) struct Active {
    pub selection: tokio::sync::watch::Sender<ModelIdentity>,
    pub cancel: CancellationToken,
    pub run_id: String,
    pub turn_id: String,
    /// Attribution of this run's model requests; the loop holds a copy.
    pub origin: std::sync::Arc<crate::contract::RequestOrigin>,
    /// Execution-scoped model and credentials (`sendText.modelExecution`).
    pub execution: Option<std::sync::Arc<super::submission::Execution>>,
    /// Permission inputs for this run's tool calls; the engine publishes updates.
    pub permissions: tokio::sync::watch::Sender<std::sync::Arc<super::permissions::Snapshot>>,
    /// What started the run (spec 9.12).
    pub kind: crate::domain::legacy_stream::RunKind,
    /// Node `record.activeAbortController`: legacy send / compact / goal refuse
    /// to start while it is held (spec 9.12).
    pub legacy_lock: bool,
    /// The agent step request as the recovery sees it (spec rust-m7-stream-recovery §2).
    pub step: crate::domain::stream_recovery::StepProbe,
    /// Usage facts in progress (spec rust-m9-usage-logs §2.3).
    pub usage: crate::domain::usage::RunUsage,
    /// The step request in flight: its model window and context breakdown
    /// (spec rust-m9-usage-logs §4.1).
    pub request: Option<super::usage_state::StepRequest>,
}
pub struct Engine {
    /// The live telemetry normalizer (spec rust-m9-usage-logs §5).
    pub(super) telemetry: crate::domain::telemetry::Normalizer,
    /// Local TTFT records and their clock (spec rust-m9-usage-logs §7).
    pub(super) ttft: crate::domain::local_ttft::Recorder,
    pub(super) ttft_clock: super::local_ttft::TtftClock,
    pub(super) child_updates:
        BTreeMap<String, tokio::sync::watch::Sender<crate::domain::subagent::Task>>,
    pub(super) uploads: crate::domain::attachment_upload::Uploads,
    pub(super) auxiliary: BTreeMap<String, super::auxiliary::Auxiliary>,
    pub(super) registry: Option<Arc<dyn crate::contract::ModelRegistry>>,
    /// Key of the startup workspace, the default for requests naming none.
    pub(super) default_workspace: String,
    /// Open workspaces by key (spec rust-cli-architecture, Workspace 作用域).
    pub(super) scopes: BTreeMap<String, super::workspace_scope::Scope>,
    /// Opens workspaces other than the startup one; `None` serves only that.
    pub(super) workspaces: Option<Arc<dyn crate::contract::WorkspaceSource>>,
    pub(super) scope_clock: u64,
    pub(super) config: Option<ModelIdentity>,
    pub(super) store: Arc<dyn SessionStore>,
    pub(super) model: Option<Arc<dyn ModelPort>>,
    /// Startup workspace's tool port; views share session-keyed tool state, so
    /// process-wide operations (cancel, evict, shutdown, checkpoints) use it.
    pub(super) tools: Arc<dyn ToolPort>,
    pub(super) clock: Arc<dyn RuntimeClock>,
    pub(super) sessions: BTreeMap<String, Session>,
    pub(super) closed: std::collections::BTreeSet<String>,
    pub(super) session_access: BTreeMap<String, u64>,
    pub(super) access_seq: u64,
    pub(super) durable_acks: std::collections::BTreeSet<String>,
    pub(super) acks: BTreeMap<String, Value>,
    pub(super) active: BTreeMap<String, Active>,
    /// Session grants; project inputs live in each workspace scope.
    pub(super) permissions: super::permissions::Permissions,
    /// Input options handed from admission to run start, keyed by (session, turn).
    pub(super) submissions: BTreeMap<(String, String), super::submission::Submission>,
    /// Everything waiting on an external answer; released per owner on every terminal path.
    pub(super) waiters: super::waiters::Waiters,
    /// Open delivery topics and their subscriber count; a conversation topic pins its session.
    pub(super) interest: BTreeMap<String, usize>,
    pub(super) outbox: Vec<ServerMsg>,
    pub(super) auto_resolution_preference: bool,
    pub(super) question_timing: (u64, u64),
    /// `-p`: no permission client, every ask is denied (spec rust-m4-headless 3.2).
    pub(super) headless: bool,
    pub(super) events: mpsc::Sender<RunEvent>,
    pub(super) event_rx: mpsc::Receiver<RunEvent>,
    /// User config hooks and workspace hook trust.
    pub(super) hooks: super::workspace_trust::HookState,
    /// Generated session titles (Node enables them for protocol sessions only).
    pub(super) titles: bool,
    /// `<instance>/engine`: runs, hooks, titles and auxiliary requests. Closed
    /// at the end of `wind_down`; dropping the engine aborts what is left
    /// (spec rust-u2-task-scope §4.1).
    pub(super) tasks: zcode_async::TaskScope,
    #[cfg(debug_assertions)]
    pub(super) fault: super::engine_fault::Fault,
}
impl Engine {
    /// An engine whose default workspace is `workspace` (a plain path is local).
    /// Its tasks belong to a child of `tasks`, the runtime instance's scope.
    pub async fn new(
        workspace: impl Into<crate::contract::Workspace>,
        config: Option<ModelIdentity>,
        ports: RuntimePorts,
        tasks: &zcode_async::Spawner,
    ) -> Result<Self> {
        let workspace = workspace.into();
        let RuntimePorts {
            context,
            store,
            model,
            tools,
            clock,
        } = ports;
        let (events, event_rx) = mpsc::channel(128);
        // 修复：此前把 workspace key 当作路径传入，远端 identity 进程的官方 MCP 身份头
        // workspacePath 会变成 identity 字符串；这里传真实路径。
        tools.attach_events(events.clone(), &workspace.path);
        // 未完成的文件回退事务在会话被加载时恢复（ensure_session），启动只读取 metadata 索引。
        let ports = super::workspace_scope::ScopePorts::startup(context, tools.clone());
        let epoch = clock.id();
        let default =
            super::workspace_scope::Scope::startup(&workspace, store.as_ref(), ports, epoch)
                .await?;
        Ok(Self {
            telemetry: Default::default(),
            ttft: crate::domain::local_ttft::Recorder::new(clock.id()),
            ttft_clock: Default::default(),
            child_updates: BTreeMap::new(),
            uploads: Default::default(),
            auxiliary: BTreeMap::new(),
            registry: None,
            default_workspace: workspace.key.clone(),
            scopes: BTreeMap::from([(workspace.key, default)]),
            workspaces: None,
            scope_clock: 0,
            config,
            store,
            model,
            tools,
            clock: clock.clone(),
            sessions: BTreeMap::new(),
            closed: Default::default(),
            session_access: Default::default(),
            access_seq: 0,
            durable_acks: Default::default(),
            acks: BTreeMap::new(),
            active: BTreeMap::new(),
            submissions: BTreeMap::new(),
            permissions: Default::default(),
            waiters: Default::default(),
            interest: BTreeMap::new(),
            outbox: vec![],
            auto_resolution_preference: true,
            headless: false,
            question_timing: (60_000, 300_000),
            events,
            event_rx,
            hooks: Default::default(),
            titles: false,
            tasks: tasks.child("engine"),
            #[cfg(debug_assertions)]
            fault: Default::default(),
        })
    }
    pub async fn serve(
        mut self,
        mut input: mpsc::Receiver<ClientMsg>,
        output: RuntimeOutput,
        cancel: CancellationToken,
    ) -> Result<()> {
        let mut refresh = tokio::time::interval(std::time::Duration::from_secs(1));
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // 工具层的进程遥测（MCP 进程启动、崩溃、资源采样）经 Engine 输出（spec rust-m9-usage-logs §6）。
        let mut process_events = self.tools.process_events();
        let serving = async {
            loop {
                let question_delay = self.question_delay();
                tokio::select! {
                    _=async {if let Some(delay)=question_delay {tokio::time::sleep(delay).await} else {std::future::pending::<()>().await}}=>{self.advance_questions().await?;self.flush(&output).await?;},
                    _=cancel.cancelled()=>break,
                    message=input.recv()=>match message {
                        Some(ClientMsg::Request{token,method,params})=>self.request(Call{token,method,params},&output).await?,
                        Some(ClientMsg::HostReply{id,result})=> self.host_reply(id, result),
                        Some(ClientMsg::TopicReleased{topic})=>{self.release_topic(&topic);self.trim_resident().await?;},
                        Some(ClientMsg::ConnectionClosed{connection})=>{self.uploads.clear_connection(&connection);self.trim_resident().await?;},
                        Some(ClientMsg::Eof) | None=>break,
                    },
                    Some(event)=self.event_rx.recv()=>{
                        let terminal=matches!(&event.event,Event::Finished {..}) || matches!(&event.event,Event::Background {task,..} if task.status!="running");
                        self.apply_event(event).await?;self.flush(&output).await?;
                        if terminal {self.trim_resident().await?;}
                    },
                    Some((method, params))=async {match process_events.as_mut() {Some(rx)=>rx.recv().await, None=>std::future::pending().await}}=>{
                        self.outbox.push(ServerMsg::HostNotification{method,params});self.flush(&output).await?;
                    },
                    _=refresh.tick()=>{
                        self.uploads.prune(self.clock.now());if self.ttft.active() {self.ttft_tick();self.flush(&output).await?;}
                        if let Some(registry) = &self.registry && registry.refresh(None).await.unwrap_or(false) { self.refresh_catalog()?; self.flush(&output).await?; }
                    },
                }
            }
            Ok::<_, anyhow::Error>(())
        };
        // Engine 循环 panic 时只结束循环本身：转成错误后照常收口（取消 run、回收工具进程树），
        // 否则 run 任务与它们的子进程会留在嵌入方的进程里（spec rust-u3-single-process §2.5）。
        let result = tokio::select! {biased; _=cancel.cancelled()=>Ok(()), result=zcode_async::catch_panic(serving)=>result.unwrap_or_else(|panicked| Err(super::panicked("engine", panicked)))};
        self.wind_down(result).await
    }
    async fn request(&mut self, call: Call, output: &RuntimeOutput) -> Result<()> {
        #[cfg(debug_assertions)]
        self.fault.check(&call.params);
        self.outbox.clear();
        let token = call.token;
        // 这些请求在后台完成，稍后以同一 token 回复；这里只回复启动失败。
        let deferred = match call.method {
            Method::McpList => Some(self.start_mcp_query(&call).await),
            method if method.is_plugin() => Some(self.start_plugin_request(&call).await),
            Method::UsageStats
            | Method::LegacyUsageStats
            | Method::ConversationUsage
            | Method::SessionUsage => return self.usage_query(&call, output).await,
            Method::WorkspaceGenerateText | Method::ProviderTestModelConnectivity => {
                Some(self.start_auxiliary(&call))
            }
            _ => None,
        };
        if let Some(started) = deferred {
            if let Err(error) = started {
                // 保持既有 -32602 响应，启动失败均为请求校验问题。
                let error = RuntimeError::Coded {
                    code: -32602,
                    message: error.to_string(),
                };
                output
                    .send(vec![ServerMsg::Reply {
                        token,
                        result: Err(error),
                    }])
                    .await?;
            }
            return Ok(());
        }
        let p = &call.params;
        let result = match call.method {
            // 与 Node CommandInbox 一致：信封或 payload 不合法时回 rejected ACK，而非 RPC 错误。
            Method::Command => match crate::domain::protocol::validate_command(p) {
                Err(issue) => Ok(crate::domain::protocol::invalid_payload_ack(p, &issue)),
                Ok(()) => match serde_json::from_value(call.params.clone()) {
                    Ok(command) => self.ttft_command(command, p).await,
                    Err(_) => Err(RuntimeError::invalid_params("Invalid command envelope")),
                },
            },
            Method::TopicOpen => self.open_topic(p).await,
            Method::TopicSnapshot => self.topic_snapshot_value(p),
            Method::AttachmentBegin
            | Method::AttachmentChunk
            | Method::AttachmentCommit
            | Method::AttachmentAbort => self.attachment_upload(call.method.as_str(), p).await,
            Method::AttachmentRead
            | Method::AttachmentPreviewSource
            | Method::ConversationAttachmentRead
            | Method::ConversationAttachmentStat
            | Method::ConversationRowsRange
            | Method::ConversationPlans => self.conversation_query(call.method, p).await,
            Method::WorkspaceUpdateInteractionPreferences => self.interaction_preferences(p).await,
            Method::WorkspaceHooksTrustGrant => self.trust_grant(p).await,
            Method::ConversationFileChanges => self.file_changes(p).await,
            Method::ConversationFileRewindPreview => self.rewind_preview(p).await,
            Method::SessionRead => self.read_cold_session(p).await,
            Method::ConversationInputQueue => self.input_queue(p).await,
            Method::CommandsQuery => self.query_acks(p).await,
            Method::SessionList => self.list_sessions(p).await,
            Method::SessionSubagents => self.subagents_query(p).await,
            Method::SessionClose => self.close_runtime_session(p).await,
            Method::SkillsReferenceCatalog => self.skill_catalog(p).await,
            Method::SessionCreate => self.legacy_create(p).await,
            Method::SessionResume => self.legacy_resume(p).await,
            Method::SessionSubscribe => self.legacy_subscribe(p),
            Method::SessionDebug => self.session_debug(p),
            Method::SessionSetModel | Method::SessionSetThoughtLevel | Method::SessionSetMode => {
                self.legacy_setter(call.method, p).await
            }
            Method::SessionSend => self.legacy_send(p).await,
            Method::SessionCompact => self.legacy_compact(p).await,
            Method::SessionGoal => self.legacy_goal(p).await,
            Method::ProviderUpdateAccountConfig => self.update_account(p).await,
            method => self.query(method, p),
        };
        let storage_failed = result
            .as_ref()
            .err()
            .is_some_and(anyhow::Error::is::<StorageCommitFailure>);
        let result = result.map_err(|error| {
            // 失败请求不能发布半途产生的事件。
            self.outbox.clear();
            let error = RuntimeError::classify(&error);
            // 只记录方法与错误码：错误文本可能包含用户路径或内容，不写入日志。
            tracing::warn!(
                target: "zcode::runtime",
                event = "rpc.request.failed",
                method = call.method.as_str(),
                code = error.code(),
                "Request failed"
            );
            error
        });
        let mut batch = vec![ServerMsg::Reply { token, result }];
        batch.append(&mut self.outbox);
        output.send(batch).await?;
        if storage_failed {
            tracing::error!(
                target: "zcode::runtime",
                event = "storage.commit.failed",
                method = call.method.as_str(),
                "Storage commit failed; stopping runtime"
            );
            return Err(StorageCommitFailure.into());
        }
        self.trim_resident().await?;
        Ok(())
    }
    async fn flush(&mut self, output: &RuntimeOutput) -> Result<()> {
        if !self.outbox.is_empty() {
            output.send(std::mem::take(&mut self.outbox)).await?;
        }
        Ok(())
    }
}
