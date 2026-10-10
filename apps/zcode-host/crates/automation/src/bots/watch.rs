//! 回复流水线（botsService.ts `watchTaskStream`）：会话事件 → 回复颗粒度 → 各通道消息。
//!
//! 每个 (workspaceKey, taskId) 只有一个订阅；订阅建立（收到初始快照）后才返回，调用方随后才发送
//! 输入，快速完成的轮次也不会漏。事件在同一个任务里串行处理（第三方消息是离散气泡，乱序会让
//! 变更摘要抢在正文前面）。
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;
use zcode_host_kit::log;

use super::feishu::Actor;
use super::feishu::cards::{self, StreamBlock, StreamState, StreamStatus};
use super::model::{is_feishu, message as msg, normalize_reply_mode, workspace_key};
use super::now_ms;
use super::providers::{OutboundExtras, TypingTarget, outbound, typing_is_reaction};
use super::reply::{
    extract_messages, is_tool_terminal, split_long_reply, tool_reply, tool_summary_line,
};
use super::service::{Bots, Typing, guard};
use super::stream::{Projector, StreamEvent, ToolState};
use super::util::{collapse_whitespace, js_len, js_slice, text};
use crate::agent::WorkspaceTarget;

/// `FEISHU_STREAMING_CARD_MIN_UPDATE_INTERVAL_MS`。
pub const CARD_MIN_UPDATE_INTERVAL_MS: u64 = 1_000;
const CARD_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
pub const CARD_BACKOFF_BASE_MS: i64 = 1_000;
const CARD_CIRCUIT_THRESHOLD: u32 = 3;
/// 订阅后等初始快照的上限；超时退回只用终态事件。
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);
/// 回推等待上限：超过后放弃订阅（长任务仍会正常完成，只是不再回推）。
const WATCH_LIMIT: Duration = Duration::from_secs(24 * 60 * 60);
/// `BOT_TYPING_INTERVAL_MS`。
const TYPING_INTERVAL: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, PartialEq)]
enum Part {
    Content(String),
    Thought(String),
    ToolCall,
}

#[derive(Debug, Clone)]
enum Timeline {
    Message(String),
    Tools(Vec<String>),
}

/// 一个流式卡片的同步状态（`syncStreamingCardReply` 的闭包变量）。
struct Card {
    handle: Option<String>,
    segment_index: usize,
    blocks: Vec<Timeline>,
    status: StreamStatus,
    last_update_at: i64,
    failures: u32,
    next_attempt_at: i64,
    circuit_open: bool,
}

impl Default for Card {
    fn default() -> Self {
        Self {
            handle: None,
            segment_index: 0,
            blocks: Vec::new(),
            status: StreamStatus::Running,
            last_update_at: 0,
            failures: 0,
            next_attempt_at: 0,
            circuit_open: false,
        }
    }
}

impl Card {
    fn append_chunk(&mut self, content: &str) {
        if content.is_empty() {
            return;
        }
        if let Some(Timeline::Message(last)) = self.blocks.last_mut() {
            last.push_str(content);
            return;
        }
        self.blocks.push(Timeline::Message(content.to_owned()));
    }

    fn append_messages(&mut self, messages: &[String]) {
        for message in messages.iter().map(|m| m.trim()).filter(|m| !m.is_empty()) {
            match self.blocks.last_mut() {
                Some(Timeline::Message(last)) if !last.trim().is_empty() => {
                    *last = format!("{}\n\n{message}", last.trim());
                }
                _ => self.blocks.push(Timeline::Message(message.to_owned())),
            }
        }
    }

    fn has_message_text(&self) -> bool {
        self.blocks
            .iter()
            .any(|block| matches!(block, Timeline::Message(text) if !text.trim().is_empty()))
    }

    fn append_tool(&mut self, tool_id: &str) {
        let known = self.blocks.iter().any(
            |block| matches!(block, Timeline::Tools(ids) if ids.iter().any(|id| id == tool_id)),
        );
        if known {
            return;
        }
        if let Some(Timeline::Tools(ids)) = self.blocks.last_mut() {
            ids.push(tool_id.to_owned());
            return;
        }
        self.blocks.push(Timeline::Tools(vec![tool_id.to_owned()]));
    }
}

/// `watchTaskStream` 的闭包状态。
pub(crate) struct Watch {
    pub bot: Value,
    pub actor: Actor,
    pub context: Value,
    pub user: Value,
    parts: Vec<Part>,
    buffer: String,
    sent_any: bool,
    part_tools: HashSet<String>,
    tools: HashMap<String, ToolState>,
    sent_tool_replies: HashSet<String>,
    card: Card,
}

fn task_id(context: &Value) -> String {
    text(context, "activeTaskId").unwrap_or_default().to_owned()
}

pub(crate) fn context_target(context: &Value) -> WorkspaceTarget {
    WorkspaceTarget {
        workspace_path: text(context, "workspacePath")
            .unwrap_or_default()
            .to_owned(),
        workspace_identity: text(context, "workspaceIdentity").map(str::to_owned),
        remote_session_id: None,
    }
}

/// `formatStatusStreamToolProgress`。
fn tool_progress(tool: &ToolState, update: bool) -> String {
    let title = collapse_whitespace(if tool.tool_name.is_empty() {
        "tool"
    } else {
        &tool.tool_name
    });
    let read = |value: &Value| -> Option<String> {
        if let Some(text) = value.as_str() {
            let text = collapse_whitespace(text);
            return (!text.is_empty()).then_some(text);
        }
        ["command", "path", "file_path", "filePath", "prompt"]
            .iter()
            .filter_map(|key| value.get(*key).and_then(Value::as_str))
            .map(collapse_whitespace)
            .find(|text| !text.is_empty())
    };
    let detail = read(&tool.input).or_else(|| if update { read(&tool.output) } else { None });
    let status = if update {
        format!(" [{}]", tool.status)
    } else {
        String::new()
    };
    match detail {
        Some(detail) => format!("{title}{status}: {detail}"),
        None => format!("{title}{status}"),
    }
}

/// `truncateLiveStatusProgressText`：保留末尾 1000 个码元。
fn tail_1000(value: &str) -> String {
    let len = js_len(value);
    if len > 1000 {
        js_slice(value, len - 1000, len)
    } else {
        value.to_owned()
    }
}

impl Watch {
    fn new(bot: Value, actor: Actor, context: Value, user: Value) -> Self {
        Self {
            bot,
            actor,
            context,
            user,
            parts: Vec::new(),
            buffer: String::new(),
            sent_any: false,
            part_tools: HashSet::new(),
            tools: HashMap::new(),
            sent_tool_replies: HashSet::new(),
            card: Card::default(),
        }
    }

    fn mode(&self) -> &'static str {
        normalize_reply_mode(
            text(&self.bot, "provider").unwrap_or_default(),
            text(&self.user, "replyMode"),
        )
    }

    /// `supportsStreamingCardReply`。
    fn streaming_card(&self) -> bool {
        self.mode() == "streaming_card"
            && is_feishu(text(&self.bot, "provider").unwrap_or_default())
    }

    fn workspace_path(&self) -> &str {
        text(&self.context, "workspacePath").unwrap_or_default()
    }

    fn append_part(&mut self, part: Part) {
        match (self.parts.last_mut(), &part) {
            (Some(Part::Content(last)), Part::Content(next))
            | (Some(Part::Thought(last)), Part::Thought(next)) => last.push_str(next),
            _ => self.parts.push(part),
        }
    }

    /// `presentation.latestPart`：最后一个正文 part。
    fn latest_content(&self) -> Option<&str> {
        self.parts.iter().rev().find_map(|part| match part {
            Part::Content(content) => Some(content.as_str()),
            _ => None,
        })
    }

    fn card_blocks(&self, locale: Option<&str>) -> Vec<StreamBlock> {
        let title = msg(locale, "streamingToolSummaries", &[]);
        let latest_tools = self
            .card
            .blocks
            .iter()
            .rposition(|block| matches!(block, Timeline::Tools(_)));
        let mut blocks = Vec::new();
        for (index, block) in self.card.blocks.iter().enumerate() {
            match block {
                Timeline::Message(body) => {
                    let body = body.trim();
                    if !body.is_empty() {
                        blocks.push(StreamBlock::Message(body.to_owned()));
                    }
                }
                Timeline::Tools(ids) => {
                    let summaries: Vec<String> = ids
                        .iter()
                        .filter_map(|id| self.tools.get(id))
                        .map(|tool| tool_summary_line(tool, self.workspace_path(), locale))
                        .collect();
                    if summaries.is_empty() {
                        continue;
                    }
                    blocks.push(StreamBlock::Tools {
                        summaries,
                        title: Some(title.clone()),
                        expanded: Some(
                            self.card.status == StreamStatus::Running
                                && Some(index) == latest_tools,
                        ),
                    });
                }
            }
        }
        if blocks.is_empty() {
            blocks.push(StreamBlock::Message(msg(locale, "streamingWorking", &[])));
        }
        blocks
    }
}

impl Bots {
    /// `watchTaskStream`：订阅会话帧并在订阅建立后返回；订阅不可用时退回终态事件兜底。
    pub(crate) async fn watch_task_stream(
        self: &Arc<Self>,
        bot: &Value,
        actor: &Actor,
        context: &Value,
        user: &Value,
    ) {
        let task = task_id(context);
        if task.is_empty() {
            return;
        }
        let target = context_target(context);
        let key = format!(
            "{}::{task}",
            workspace_key(&target.workspace_path, target.workspace_identity.as_deref())
        );
        {
            let mut watches = guard(&self.watches);
            if watches.contains_key(&key) {
                return;
            }
            watches.insert(key.clone(), None);
        }
        let tasks = match self.tasks_for(&target) {
            Ok(tasks) => tasks,
            Err(error) => {
                log::warn(format!("bots watch skipped task={task}: {error}"));
                guard(&self.watches).remove(&key);
                return;
            }
        };
        let subscribed = match tasks.subscribe(target.clone(), task.clone()).await {
            Ok(mut frames) => {
                let mut projector = Projector::new();
                match tokio::time::timeout(SNAPSHOT_TIMEOUT, frames.next()).await {
                    Ok(Some(first)) => {
                        projector.apply(&first);
                        projector.is_initialized().then_some((frames, projector))
                    }
                    _ => None,
                }
            }
            Err(error) => {
                log::warn(format!(
                    "bots subscribe conversation failed task={task}: {error}"
                ));
                None
            }
        };
        let Some((frames, projector)) = subscribed else {
            guard(&self.watches).remove(&key);
            self.watch_terminal_fallback(bot.clone(), actor.clone(), target, task.clone(), None);
            self.start_typing(bot, actor, &task);
            return;
        };
        let watch = Watch::new(bot.clone(), actor.clone(), context.clone(), user.clone());
        let bots = self.clone();
        let loop_key = key.clone();
        let handle = self.spawner.spawn("bot-watch", async move {
            let run = bots.clone().run_watch(watch, frames, projector);
            let _ = tokio::time::timeout(WATCH_LIMIT, run).await;
            guard(&bots.watches).remove(&loop_key);
        });
        if let Some(slot) = guard(&self.watches).get_mut(&key) {
            *slot = Some(handle);
        }
        self.start_typing(bot, actor, &task);
    }

    async fn run_watch(
        self: Arc<Self>,
        mut watch: Watch,
        mut frames: futures_util::stream::BoxStream<'static, Value>,
        mut projector: Projector,
    ) {
        let task = task_id(&watch.context);
        while let Some(frame) = frames.next().await {
            for event in projector.apply(&frame) {
                let terminal = matches!(event, StreamEvent::Complete | StreamEvent::Failed(_));
                if let Err(error) = self.handle_stream_event(&mut watch, event).await {
                    log::warn(format!(
                        "bot task stream event failed task={task}: {error:#}"
                    ));
                }
                if terminal {
                    return;
                }
            }
        }
        // 帧流结束（CLI 换代或退订）而没有终态：清掉运行中标记与 typing。
        guard(&self.running).remove(&task);
        self.stop_typing(&task);
    }

    async fn send_watch_text(&self, watch: &Watch, body: String) -> anyhow::Result<()> {
        self.send_outbound(
            &watch.bot,
            &outbound(&watch.actor, body, OutboundExtras::default()),
        )
        .await
    }

    /// `flushAssistantReplyBuffer`。
    async fn flush_buffer(&self, watch: &mut Watch, force: bool) -> anyhow::Result<()> {
        if watch.mode() == "summary_changes" || watch.streaming_card() || watch.buffer.is_empty() {
            return Ok(());
        }
        let (messages, rest) = extract_messages(&watch.buffer, force);
        watch.buffer = rest;
        for body in messages {
            watch.sent_any = true;
            self.send_watch_text(watch, body).await?;
        }
        Ok(())
    }

    /// `updateLiveStatusProgress`：正文 / 思考按种类累积（保留末尾 1000 码元），工具取最新一条。
    fn update_live_progress(&self, task: &str, kind: &'static str, text_value: &str) {
        let mut progress = guard(&self.live_progress);
        if kind == "tool" {
            progress.insert(task.to_owned(), (kind, text_value.to_owned()));
            return;
        }
        let normalized = collapse_whitespace(text_value);
        if normalized.is_empty() {
            return;
        }
        let next = match progress.get(task) {
            Some((previous_kind, previous)) if *previous_kind == kind => {
                tail_1000(&format!("{previous}{normalized}"))
            }
            _ => tail_1000(&normalized),
        };
        progress.insert(task.to_owned(), (kind, next));
    }

    async fn handle_stream_event(
        self: &Arc<Self>,
        watch: &mut Watch,
        event: StreamEvent,
    ) -> anyhow::Result<()> {
        let task = task_id(&watch.context);
        match event {
            StreamEvent::MessageChunk(content) => {
                self.update_live_progress(&task, "message", &content);
                watch.append_part(Part::Content(content.clone()));
                if watch.streaming_card() {
                    watch.card.append_chunk(&content);
                    self.sync_card(watch, "agent_message_chunk", false).await;
                    return Ok(());
                }
                if watch.mode() != "summary_changes" {
                    watch.buffer.push_str(&content);
                    self.flush_buffer(watch, false).await?;
                }
            }
            StreamEvent::ThoughtChunk(content) => {
                self.update_live_progress(&task, "thought", &content);
                watch.append_part(Part::Thought(content));
            }
            StreamEvent::ToolCall(tool) => self.handle_tool(watch, tool, false).await?,
            StreamEvent::ToolUpdate(tool) => self.handle_tool(watch, tool, true).await?,
            StreamEvent::Interaction(interaction) => {
                self.stop_typing(&task);
                self.seal_card(watch).await;
                if interaction["kind"] == "userInput" {
                    self.handle_elicitation_request(watch, &interaction).await?;
                } else if interaction["kind"] == "permission" {
                    self.handle_permission_request(watch, &interaction).await?;
                }
            }
            StreamEvent::InteractionResolved(request_id) => {
                self.clear_elicitation_for_request(&mut watch.context, &request_id)
                    .await?;
            }
            StreamEvent::Complete => self.finish_watch(watch, None).await?,
            StreamEvent::Failed(error) => self.finish_watch(watch, Some(error)).await?,
        }
        Ok(())
    }

    async fn handle_tool(
        self: &Arc<Self>,
        watch: &mut Watch,
        tool: ToolState,
        update: bool,
    ) -> anyhow::Result<()> {
        let task = task_id(&watch.context);
        self.update_live_progress(&task, "tool", &tool_progress(&tool, update));
        if watch.streaming_card() {
            watch.card.append_tool(&tool.tool_id);
        } else {
            self.flush_buffer(watch, true).await?;
        }
        // tool_call_update 可能先于 tool_call 首帧：像 UI 一样补一个 tool-call part 边界。
        if watch.part_tools.insert(tool.tool_id.clone()) {
            watch.append_part(Part::ToolCall);
        }
        // `updateBotReplyToolCalls`：首帧状态为 pending，更新帧按字段合并。
        let state = if update {
            let previous = watch.tools.get(&tool.tool_id);
            ToolState {
                input: if tool.input.is_null() {
                    previous.map(|p| p.input.clone()).unwrap_or(Value::Null)
                } else {
                    tool.input.clone()
                },
                output: if tool.output.is_null() {
                    previous.map(|p| p.output.clone()).unwrap_or(Value::Null)
                } else {
                    tool.output.clone()
                },
                error: tool
                    .error
                    .clone()
                    .or_else(|| previous.and_then(|p| p.error.clone())),
                ..tool.clone()
            }
        } else {
            ToolState {
                status: "pending",
                output: Value::Null,
                error: None,
                ..tool.clone()
            }
        };
        watch.tools.insert(tool.tool_id.clone(), state.clone());
        if watch.streaming_card() {
            let force = !update || is_tool_terminal(state.status);
            self.sync_card(
                watch,
                if update {
                    "tool_call_update"
                } else {
                    "tool_call"
                },
                force,
            )
            .await;
        }
        if update
            && watch.mode() == "assistant_toolcalls_changes"
            && is_tool_terminal(state.status)
            && watch.sent_tool_replies.insert(state.tool_id.clone())
        {
            watch.sent_any = true;
            let locale = self.locale().await;
            let body = tool_reply(&state, watch.workspace_path(), locale.as_deref());
            self.send_watch_text(watch, body).await?;
        }
        Ok(())
    }

    /// `syncStreamingCardReply`：节流、失败退避与熔断、按元素预算分段。
    async fn sync_card(&self, watch: &mut Watch, trigger: &str, force: bool) {
        if !watch.streaming_card() {
            return;
        }
        let now = now_ms();
        // 失败退避和熔断先于 force 判断：单次 400 不能被每个流事件放大成请求风暴。
        if watch.card.circuit_open || now < watch.card.next_attempt_at {
            return;
        }
        let interval = *guard(&self.card_interval_ms) as i64;
        if watch.card.handle.is_some() && !force && now - watch.card.last_update_at < interval {
            return;
        }
        let locale = self.locale().await;
        let state = StreamState {
            locale: locale.clone(),
            blocks: watch.card_blocks(locale.as_deref()),
            status: watch.card.status,
        };
        let states = cards::split_streaming(&state);
        let receive_id = watch.actor.provider_user_id.clone();
        let operation = if watch.card.handle.is_some() {
            "update"
        } else {
            "create"
        };
        let bot = watch.bot.clone();
        let card = &mut watch.card;
        let work = async {
            let start = card.segment_index.min(states.len() - 1);
            for (index, segment) in states.iter().enumerate().skip(start) {
                match &card.handle {
                    None => {
                        let created = self
                            .feishu_create_stream_card(&bot, &receive_id, segment)
                            .await?;
                        // 创建接口可能 code=0 却不返回 message_id：当成失败进入退避，不能跳过未投递的段。
                        card.handle = Some(created.ok_or_else(|| {
                            anyhow::anyhow!("Feishu create streaming card returned no message_id.")
                        })?);
                    }
                    Some(handle) => {
                        let handle = handle.clone();
                        self.feishu_update_card(&bot, &handle, &cards::streaming(segment))
                            .await?;
                    }
                }
                if index < states.len() - 1 {
                    // 已满的卡片封存为历史段，后续块只写新卡片。
                    card.segment_index = index + 1;
                    card.handle = None;
                }
            }
            anyhow::Ok(())
        };
        let result = match tokio::time::timeout(CARD_REQUEST_TIMEOUT, work).await {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!("Feishu streaming card request timed out.")),
        };
        let card = &mut watch.card;
        match result {
            Ok(()) => {
                card.last_update_at = now_ms();
                card.failures = 0;
                card.next_attempt_at = 0;
            }
            Err(error) => {
                card.failures += 1;
                let task = task_id(&watch.context);
                if card.failures >= CARD_CIRCUIT_THRESHOLD {
                    card.circuit_open = true;
                    log::warn(format!(
                        "Feishu streaming card circuit opened task={task} trigger={trigger} operation={operation} failures={}: {error:#}",
                        card.failures
                    ));
                } else {
                    let delay = *guard(&self.card_backoff_ms) * 2i64.pow(card.failures - 1);
                    card.next_attempt_at = now_ms() + delay;
                    log::warn(format!(
                        "Feishu streaming card sync failed task={task} trigger={trigger} operation={operation} failures={} retryDelayMs={delay}: {error:#}",
                        card.failures
                    ));
                }
            }
        }
    }

    /// `sealStreamingCardReply`：交互前后的输出属于两段，封存当前卡片，之后写新卡片。
    pub(crate) async fn seal_card(&self, watch: &mut Watch) {
        if watch.card.handle.is_none() {
            return;
        }
        watch.card.status = StreamStatus::Sealed;
        self.sync_card(watch, "seal", true).await;
        watch.card.handle = None;
        watch.card.segment_index = 0;
        watch.card.blocks.clear();
        watch.card.status = StreamStatus::Running;
        watch.card.last_update_at = 0;
    }

    /// 终态（task_complete / task_error）。
    async fn finish_watch(
        self: &Arc<Self>,
        watch: &mut Watch,
        error: Option<String>,
    ) -> anyhow::Result<()> {
        let task = task_id(&watch.context);
        guard(&self.running).remove(&task);
        guard(&self.live_progress).remove(&task);
        self.stop_typing(&task);
        let locale = self.locale().await;
        let locale = locale.as_deref();
        let error = error.map(|error| {
            if error.trim().is_empty() {
                msg(locale, "streamingStatusFailed", &[])
            } else {
                error
            }
        });
        let pending = watch.context["pendingElicitation"].clone();
        if pending["taskId"] == task.as_str() {
            self.clear_pending_elicitation_selection(&pending);
            watch.context["pendingElicitation"] = Value::Null;
            self.write_context(&strip_pending(&watch.context)).await?;
        }
        let card_key = self.actor_key(&watch.actor);
        let transient = guard(&self.transient_cards).get(&card_key).cloned();
        if let Some(card) = transient.filter(|card| card.task_id == task) {
            let fallback = if pending.is_object() {
                self.completed_elicitation_outbound(&watch.actor, &pending, locale, "cancel")
            } else {
                let body = match &error {
                    Some(error) => msg(locale, "taskFailed", &[("message", error)]),
                    None => msg(locale, "received", &[]),
                };
                outbound(&watch.actor, body, OutboundExtras::default())
            };
            let _ = card;
            self.finalize_transient_card(&watch.actor, &fallback).await;
        }
        if let Some(error) = error {
            let failed = msg(locale, "taskFailed", &[("message", &error)]);
            if watch.streaming_card() {
                watch.card.status = StreamStatus::Error;
                if !watch.card.has_message_text() {
                    watch.card.append_messages(&[failed]);
                }
                self.sync_card(watch, "task_error", true).await;
                return Ok(());
            }
            return self.send_watch_text(watch, failed).await;
        }
        // Node 的任务快照 fileChanges 固定为空，完成消息实际不带变更摘要（见 spec 2.6）。
        if watch.streaming_card() {
            watch.card.status = StreamStatus::Completed;
            self.sync_card(watch, "task_complete", true).await;
            watch.sent_any = true;
            return Ok(());
        }
        let messages = if watch.mode() == "summary_changes" {
            watch
                .latest_content()
                .filter(|content| !content.trim().is_empty())
                .map(|content| split_long_reply(&content.replace("\r\n", "\n")))
                .unwrap_or_default()
        } else {
            self.flush_buffer(watch, true).await?;
            Vec::new()
        };
        if messages.is_empty() && !watch.sent_any {
            let done = if locale == Some("en-US") {
                "Task completed."
            } else {
                "任务已完成。"
            };
            return self.send_watch_text(watch, done.to_owned()).await;
        }
        for body in messages {
            watch.sent_any = true;
            self.send_watch_text(watch, body).await?;
        }
        Ok(())
    }

    // ── typing ──

    /// `startTyping`：飞书加 Typing reaction，Telegram / 微信每 4 秒重发一次。
    pub(crate) fn start_typing(self: &Arc<Self>, bot: &Value, actor: &Actor, task: &str) {
        let provider = text(bot, "provider").unwrap_or_default();
        let target_id = actor.reply_target();
        if target_id.is_empty() || guard(&self.typing).contains_key(task) {
            return;
        }
        let target = TypingTarget {
            provider_user_id: target_id.to_owned(),
            provider_message_id: actor.provider_message_id.clone(),
            provider_context_token: actor.provider_context_token.clone(),
        };
        if typing_is_reaction(provider) {
            guard(&self.typing).insert(
                task.to_owned(),
                Typing {
                    bot: bot.clone(),
                    target: target.clone(),
                    repeat: None,
                },
            );
            let bots = self.clone();
            let bot = bot.clone();
            drop(self.spawner.spawn("bot-typing", async move {
                bots.send_typing(&bot, &target).await
            }));
            return;
        }
        if !matches!(provider, "telegram" | "weixin") {
            return;
        }
        let bots = self.clone();
        let repeat_bot = bot.clone();
        let repeat_target = target.clone();
        let repeat = self.spawner.spawn("bot-typing-repeat", async move {
            loop {
                bots.send_typing(&repeat_bot, &repeat_target).await;
                tokio::time::sleep(TYPING_INTERVAL).await;
            }
        });
        guard(&self.typing).insert(
            task.to_owned(),
            Typing {
                bot: bot.clone(),
                target,
                repeat: Some(repeat),
            },
        );
    }

    /// `stopTyping`。
    pub(crate) fn stop_typing(self: &Arc<Self>, task: &str) {
        let Some(typing) = guard(&self.typing).remove(task) else {
            return;
        };
        if let Some(repeat) = typing.repeat {
            repeat.abort();
            return;
        }
        let bots = self.clone();
        drop(self.spawner.spawn("bot-typing-stop", async move {
            bots.stop_typing_reaction(&typing.bot, &typing.target).await
        }));
    }

    /// `sendTyping`（入站命令）：飞书等 reaction 建好再返回，其余通道后台发送。
    pub(crate) async fn send_inbound_typing(self: &Arc<Self>, bot: &Value, actor: &Actor) {
        let target_id = actor.reply_target();
        if target_id.is_empty()
            || !matches!(
                text(bot, "provider").unwrap_or_default(),
                "telegram" | "weixin" | "feishu" | "lark"
            )
        {
            return;
        }
        let target = TypingTarget {
            provider_user_id: target_id.to_owned(),
            provider_message_id: actor.provider_message_id.clone(),
            provider_context_token: actor.provider_context_token.clone(),
        };
        if is_feishu(text(bot, "provider").unwrap_or_default()) {
            // 短命令 typing 由同步回复后显式删除：必须等 reaction 创建完成，否则删除可能先执行。
            self.send_typing(bot, &target).await;
        } else {
            let bots = self.clone();
            let bot = bot.clone();
            drop(self.spawner.spawn("bot-typing", async move {
                bots.send_typing(&bot, &target).await
            }));
        }
    }

    /// `stopInboundTyping`：短命令回复后删除本条入站消息的 Typing reaction（长任务的不删）。
    pub(crate) async fn stop_inbound_typing(&self, bot: &Value, actor: &Actor) {
        let Some(message_id) = actor.provider_message_id.as_deref() else {
            return;
        };
        if !is_feishu(text(bot, "provider").unwrap_or_default()) || actor.reply_target().is_empty()
        {
            return;
        }
        let long_running = guard(&self.typing).values().any(|typing| {
            typing.bot["id"] == bot["id"]
                && typing.target.provider_message_id.as_deref() == Some(message_id)
        });
        if long_running {
            return;
        }
        let target = TypingTarget {
            provider_user_id: actor.reply_target().to_owned(),
            provider_message_id: Some(message_id.to_owned()),
            provider_context_token: actor.provider_context_token.clone(),
        };
        self.stop_typing_reaction(bot, &target).await;
    }

    /// 会话帧订阅不可用时的兜底：等 sessions-index 终态，读最后一段助手文本回推（R6 行为）。
    pub(crate) fn watch_terminal_fallback(
        self: &Arc<Self>,
        bot: Value,
        actor: Actor,
        workspace: WorkspaceTarget,
        task: String,
        input_id: Option<String>,
    ) {
        let Ok(tasks) = self.tasks_for(&workspace) else {
            return;
        };
        let mut outcomes = tasks.terminal_outcomes();
        let bots = self.clone();
        let key = workspace_key(
            &workspace.workspace_path,
            workspace.workspace_identity.as_deref(),
        );
        drop(self.spawner.spawn("bot-terminal-fallback", async move {
            let wait = async {
                while let Some(outcome) = outcomes.next().await {
                    if outcome.workspace_key == key
                        && outcome.session_id == task
                        && input_id
                            .as_ref()
                            .is_none_or(|id| outcome.input_id.as_ref() == Some(id))
                    {
                        return Some(outcome);
                    }
                }
                None
            };
            let outcome = tokio::time::timeout(WATCH_LIMIT, wait).await.ok().flatten();
            guard(&bots.running).remove(&task);
            bots.stop_typing(&task);
            let Some(outcome) = outcome else {
                return;
            };
            let reply = tasks.final_reply(workspace, task.clone()).await;
            let locale = bots.locale().await;
            let locale = locale.as_deref();
            let body = if outcome.outcome == "failed" {
                let reason = reply
                    .error
                    .unwrap_or_else(|| msg(locale, "streamingStatusFailed", &[]));
                Some(msg(locale, "taskFailed", &[("message", &reason)]))
            } else {
                reply.text
            };
            let Some(body) = body else {
                return;
            };
            if let Err(error) = bots
                .send_outbound(&bot, &outbound(&actor, body, OutboundExtras::default()))
                .await
            {
                log::warn(format!(
                    "bots terminal reply failed bot={} task={task}: {error:#}",
                    actor.bot_id
                ));
            }
        }));
    }
}

/// 写回上下文前去掉值为 null 的可选字段（TS 里赋 undefined 的键在 JSON 中不存在）。
pub(crate) fn strip_pending(context: &Value) -> Value {
    let mut next = context.clone();
    super::model::strip_nulls(
        &mut next,
        &[
            "pendingElicitation",
            "pendingPermissionOptions",
            "draftOptions",
        ],
    );
    next
}
