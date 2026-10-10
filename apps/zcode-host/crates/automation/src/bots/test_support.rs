//! bots 测试共用的设施：127.0.0.1 上的可编程 HTTP mock、内存 TaskPort（按脚本产生 V4 会话帧）、
//! 隔离的 Bots 实例。不连任何真实服务，不读写真实 HOME。
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use futures_util::{FutureExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use zcode_host_kit::EventHub;

use super::providers::{Endpoints, Providers};
use super::service::{Bots, BotsDeps};
use super::store::BotsRepo;
use super::{FinalReply, NoRemote, RemoteBots, TaskPort};
use crate::account::credential::CredentialStore;
use crate::agent::WorkspaceTarget;
use crate::automation::driver::{SendText, TerminalOutcome};
use crate::settings::SettingStore;

/// mock 收到的一个请求。
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: HashMap<String, String>,
    pub body: Value,
    pub raw: Vec<u8>,
}

/// mock 的应答：状态码与 JSON（或原始字节）。`delay` 用于模拟悬挂。
pub struct Answer {
    pub status: u16,
    pub body: Vec<u8>,
    pub delay: Duration,
}

impl Answer {
    pub fn json(status: u16, value: Value) -> Self {
        Self {
            status,
            body: value.to_string().into_bytes(),
            delay: Duration::ZERO,
        }
    }
}

type Responder = Arc<dyn Fn(&Recorded) -> Option<Answer> + Send + Sync>;

#[derive(Clone)]
pub struct MockServer {
    pub origin: String,
    pub requests: Arc<Mutex<Vec<Recorded>>>,
    responders: Arc<Mutex<Vec<Responder>>>,
}

async fn read_request(stream: &mut TcpStream) -> Option<Recorded> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break index;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let method = request_line.next().unwrap_or_default().to_owned();
    let target = request_line.next().unwrap_or_default().to_owned();
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_owned(), query.to_owned()),
        None => (target, String::new()),
    };
    Some(Recorded {
        method,
        path,
        query,
        headers,
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        raw: body,
    })
}

impl MockServer {
    pub async fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let server = Self {
            origin,
            requests: Arc::default(),
            responders: Arc::default(),
        };
        let shared = server.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    continue;
                };
                let shared = shared.clone();
                tokio::spawn(async move {
                    let Some(request) = read_request(&mut stream).await else {
                        return;
                    };
                    shared.requests.lock().unwrap().push(request.clone());
                    let responders = shared.responders.lock().unwrap().clone();
                    let answer = responders
                        .iter()
                        .rev()
                        .find_map(|responder| responder(&request))
                        .unwrap_or_else(|| Answer::json(404, json!({"code": 404})));
                    if !answer.delay.is_zero() {
                        tokio::time::sleep(answer.delay).await;
                    }
                    let head = format!(
                        "HTTP/1.1 {} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        answer.status,
                        answer.body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&answer.body).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        server
    }

    /// 登记应答规则；后登记的优先。
    pub fn on(&self, responder: impl Fn(&Recorded) -> Option<Answer> + Send + Sync + 'static) {
        self.responders.lock().unwrap().push(Arc::new(responder));
    }

    /// 路径以 `suffix` 结尾的请求。
    pub fn to(&self, suffix: &str) -> Vec<Recorded> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.path.ends_with(suffix))
            .cloned()
            .collect()
    }
}

pub async fn eventually<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    for _ in 0..400 {
        if let Some(value) = check() {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

/// 一次模拟的模型回复：先若干工具（完成），再分块的正文，最后收口。
#[derive(Clone, Default)]
pub struct Script {
    pub tools: Vec<(String, Value)>,
    pub chunks: Vec<String>,
    pub fail: Option<String>,
    /// 回复前先出现的待处理交互（V4 PendingInteraction）；出现后暂停，等 resolve 再继续。
    pub interaction: Option<Value>,
    pub delay: Duration,
}

/// 一个会话的 mock 状态。
#[derive(Default)]
struct Session {
    rows: Vec<Value>,
    next_row: i64,
    seq: u64,
    sinks: Vec<tokio::sync::mpsc::UnboundedSender<Value>>,
    status: String,
    model: Value,
}

/// 内存 TaskPort：create/send 写日志，send 按 [`Script`] 产生 V4 帧。
pub struct FakeTasks {
    pub terminal: EventHub<TerminalOutcome>,
    pub created: Mutex<Vec<(String, Value, Option<String>)>>,
    pub sent: Mutex<Vec<(String, SendText, usize)>>,
    pub resolved: Mutex<Vec<(String, String, Value)>>,
    pub set_models: Mutex<Vec<(String, Value)>>,
    pub thought_levels: Mutex<Vec<(String, String)>>,
    pub stopped: Mutex<Vec<String>>,
    pub script: Mutex<Script>,
    pub view: Mutex<Value>,
    pub config_options: Mutex<Value>,
    pub subscribe_fails: Mutex<bool>,
    pub create_fails: Mutex<Option<String>>,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    pending_resume: Arc<Mutex<HashMap<String, Script>>>,
}

pub fn default_view() -> Value {
    json!({
        "preferredSelection": {"providerId": "zai", "modelId": "GLM-5", "options": {"reasoningLevel": "high"}},
        "providers": [
            {"providerId": "zai", "providerName": "Z.ai", "models": [
                {"modelId": "GLM-5", "config": {"optionSpecs": {"reasoningLevel": {"values": ["low", "high"]}}}},
                {"modelId": "GLM-4.7", "config": {"optionSpecs": {"reasoningLevel": {"values": ["medium"]}}}}]},
            {"providerId": "mock", "providerName": "Mock", "models": [
                {"modelId": "m1", "config": {"optionSpecs": {"reasoningLevel": {"values": ["off", "on"]}}}}]},
        ],
    })
}

impl Default for FakeTasks {
    fn default() -> Self {
        Self {
            terminal: EventHub::new("test", 1024, zcode_host_kit::Overflow::CloseSubscriber),
            created: Mutex::default(),
            sent: Mutex::default(),
            resolved: Mutex::default(),
            set_models: Mutex::default(),
            thought_levels: Mutex::default(),
            stopped: Mutex::default(),
            script: Mutex::new(Script {
                chunks: vec!["Hi from ".into(), "the agent".into()],
                ..Script::default()
            }),
            view: Mutex::new(default_view()),
            config_options: Mutex::new(json!([
                {"id": "model", "category": "model", "type": "select", "currentValue": "zai/GLM-5$high", "options": []},
                {"id": "mode", "category": "mode", "type": "select", "currentValue": "yolo",
                 "options": [{"value": "build", "name": "Ask"}, {"value": "yolo", "name": "Full access"}]},
                {"id": "thought_level", "category": "thought_level", "type": "select", "currentValue": "high",
                 "options": [{"value": "low", "name": "Low"}, {"value": "high", "name": "High"}]},
            ])),
            subscribe_fails: Mutex::new(false),
            create_fails: Mutex::new(None),
            sessions: Arc::default(),
            pending_resume: Arc::default(),
        }
    }
}

fn emit(sessions: &Mutex<HashMap<String, Session>>, task: &str, deltas: Vec<Value>) {
    let mut sessions = sessions.lock().unwrap();
    let Some(session) = sessions.get_mut(task) else {
        return;
    };
    let from = session.seq;
    session.seq += 1;
    let frame = json!({"topic": format!("conversation/{task}"), "subscriptionId": "s", "fromSeq": from,
        "toSeq": session.seq, "payload": {"kind": "deltas", "deltas": deltas}});
    session
        .sinks
        .retain(|sink| sink.send(frame.clone()).is_ok());
}

fn append_row(sessions: &Mutex<HashMap<String, Session>>, task: &str, mut row: Value) -> i64 {
    let mut guard = sessions.lock().unwrap();
    let session = guard.get_mut(task).unwrap();
    session.next_row += 1;
    let id = session.next_row;
    row["rowId"] = json!(id);
    session.rows.push(row);
    id
}

/// 按脚本播放一轮回复（工具 → 正文 → 收口）。
async fn play(sessions: Arc<Mutex<HashMap<String, Session>>>, task: String, script: Script) {
    tokio::time::sleep(Duration::from_millis(20) + script.delay).await;
    let turn =
        json!({"kind": "turnHeader", "turnId": "t", "state": "running", "origin": "userInput"});
    let turn_id = append_row(&sessions, &task, turn.clone());
    let mut turn_row = turn;
    turn_row["rowId"] = json!(turn_id);
    emit(
        &sessions,
        &task,
        vec![
            json!({"op": "row.appended", "row": turn_row}),
            json!({"op": "state.updated", "patch": {"control": {"phase": "running"}}}),
        ],
    );
    for (index, (name, input)) in script.tools.iter().enumerate() {
        let tool = json!({"kind": "toolCall", "turnId": "t", "toolCallId": format!("tool-{index}"),
            "toolName": name, "status": "running", "inputText": "", "input": input});
        let id = append_row(&sessions, &task, tool.clone());
        let mut row = tool;
        row["rowId"] = json!(id);
        emit(
            &sessions,
            &task,
            vec![json!({"op": "row.appended", "row": row.clone()})],
        );
        row["status"] = json!("success");
        emit(
            &sessions,
            &task,
            vec![json!({"op": "row.upserted", "row": row})],
        );
    }
    if !script.chunks.is_empty() {
        let text =
            json!({"kind": "assistantText", "turnId": "t", "text": "", "state": "streaming"});
        let id = append_row(&sessions, &task, text.clone());
        let mut row = text;
        row["rowId"] = json!(id);
        emit(
            &sessions,
            &task,
            vec![json!({"op": "row.appended", "row": row})],
        );
        for chunk in &script.chunks {
            emit(
                &sessions,
                &task,
                vec![json!({"op": "row.delta", "rowId": id, "path": "text", "append": chunk})],
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    let state = if script.fail.is_some() {
        "failed"
    } else {
        "completedSuccess"
    };
    let mut deltas = vec![
        json!({"op": "row.upserted", "row": {"kind": "turnHeader", "rowId": turn_id,
        "turnId": "t", "state": state, "origin": "userInput"}}),
    ];
    let mut control =
        json!({"phase": if script.fail.is_some() { "error" } else { "completedSuccess" }});
    if let Some(error) = &script.fail {
        control["lastError"] = json!({"message": error});
    }
    deltas.push(json!({"op": "state.updated", "patch": {"control": control}}));
    emit(&sessions, &task, deltas);
    if let Some(session) = sessions.lock().unwrap().get_mut(&task) {
        session.status = if script.fail.is_some() {
            "error".into()
        } else {
            "completed".into()
        };
    }
}

impl FakeTasks {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn ensure_session(&self, task: &str) {
        self.sessions.lock().unwrap().entry(task.to_owned()).or_insert_with(|| Session {
            status: "completed".into(),
            model: json!({"providerId": "zai", "modelId": "GLM-5", "options": {"reasoningLevel": "high"}}),
            ..Session::default()
        });
    }
}

impl TaskPort for FakeTasks {
    fn create(
        &self,
        target: WorkspaceTarget,
        selection: Value,
        mode: Option<String>,
    ) -> BoxFuture<'static, Result<String, String>> {
        if let Some(error) = self.create_fails.lock().unwrap().clone() {
            return async move { Err(error) }.boxed();
        }
        let mut created = self.created.lock().unwrap();
        created.push((target.workspace_path, selection.clone(), mode));
        let id = format!("task-{}", created.len());
        self.ensure_session(&id);
        if let Some(session) = self.sessions.lock().unwrap().get_mut(&id) {
            session.status = String::new();
            session.model = selection;
        }
        async move { Ok(id) }.boxed()
    }

    fn resume(
        &self,
        _: WorkspaceTarget,
        task: String,
    ) -> BoxFuture<'static, Result<Option<Value>, String>> {
        self.ensure_session(&task);
        let model = self.sessions.lock().unwrap()[&task].model.clone();
        async move { Ok(Some(model)) }.boxed()
    }

    fn send(
        &self,
        target: WorkspaceTarget,
        task: String,
        input: SendText,
    ) -> BoxFuture<'static, Result<(), String>> {
        self.send_with_attachments(target, task, input, Vec::new())
    }

    fn send_with_attachments(
        &self,
        _: WorkspaceTarget,
        task: String,
        input: SendText,
        attachments: Vec<Value>,
    ) -> BoxFuture<'static, Result<(), String>> {
        self.sent
            .lock()
            .unwrap()
            .push((task.clone(), input, attachments.len()));
        self.ensure_session(&task);
        if let Some(session) = self.sessions.lock().unwrap().get_mut(&task) {
            session.status = "running".into();
        }
        let script = self.script.lock().unwrap().clone();
        let sessions = self.sessions.clone();
        let pending = self.pending_resume.clone();
        async move {
            match script.interaction.clone() {
                Some(interaction) => {
                    let mut rest = script;
                    rest.interaction = None;
                    pending.lock().unwrap().insert(task.clone(), rest);
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        emit(&sessions, &task, vec![
                            json!({"op": "state.updated", "patch": {"control": {"phase": "running"},
                                "pendingInteractions": [interaction]}}),
                        ]);
                    });
                }
                None => {
                    tokio::spawn(play(sessions, task, script));
                }
            }
            Ok(())
        }
        .boxed()
    }

    fn stop(&self, _: WorkspaceTarget, task: String) -> BoxFuture<'static, Result<(), String>> {
        self.stopped.lock().unwrap().push(task);
        async { Ok(()) }.boxed()
    }

    fn list(&self, _: String) -> BoxFuture<'static, Vec<Value>> {
        let tasks: Vec<Value> = self
            .created
            .lock()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(index, (path, _, _))| {
                let id = format!("task-{}", index + 1);
                let status = self.sessions.lock().unwrap().get(&id).map(|s| s.status.clone()).unwrap_or_default();
                let mut task = json!({"taskId": id, "title": format!("Task {}", index + 1), "workspacePath": path,
                    "provider": "glm", "model": "zai/GLM-5$high"});
                if !status.is_empty() {
                    task["status"] = json!(status);
                }
                task
            })
            .collect();
        async move { tasks }.boxed()
    }

    fn deleted_ids(&self, _: String) -> BoxFuture<'static, Vec<String>> {
        async { Vec::new() }.boxed()
    }

    fn terminal_outcomes(&self) -> BoxStream<'static, TerminalOutcome> {
        self.terminal.subscribe().boxed()
    }

    fn final_reply(&self, _: WorkspaceTarget, _: String) -> BoxFuture<'static, FinalReply> {
        async {
            FinalReply {
                text: Some("Fallback reply".into()),
                error: None,
            }
        }
        .boxed()
    }

    fn model_view(&self, input: Option<Value>) -> BoxFuture<'static, Value> {
        let mut view = self.view.lock().unwrap().clone();
        if let Some(input) = input {
            view["effectiveSelection"] = input["selection"].clone();
        }
        async move { view }.boxed()
    }

    fn read_session(
        &self,
        _: WorkspaceTarget,
        task: String,
    ) -> BoxFuture<'static, Result<Value, String>> {
        let session = self
            .sessions
            .lock()
            .unwrap()
            .get(&task)
            .map(|s| (s.status.clone(), s.model.clone()));
        async move {
            let Some((status, model)) = session else {
                return Err(format!("Session not found: {task}"));
            };
            Ok(json!({"session": {"sessionId": task, "status": status, "workspace": {"workspacePath": "/tmp/ws-bot"}},
                "settings": {"model": {"current": model}}, "projection": {}, "messages": []}))
        }
        .boxed()
    }

    fn config_options(
        &self,
        _: WorkspaceTarget,
        _: String,
    ) -> BoxFuture<'static, Result<Value, String>> {
        let options = self.config_options.lock().unwrap().clone();
        async move { Ok(options) }.boxed()
    }

    fn set_model(
        &self,
        _: WorkspaceTarget,
        task: String,
        selection: Value,
    ) -> BoxFuture<'static, Result<(), String>> {
        self.set_models.lock().unwrap().push((task, selection));
        async { Ok(()) }.boxed()
    }

    fn set_thought_level(
        &self,
        _: WorkspaceTarget,
        task: String,
        level: String,
    ) -> BoxFuture<'static, Result<(), String>> {
        self.thought_levels.lock().unwrap().push((task, level));
        async { Ok(()) }.boxed()
    }

    fn resolve_interaction(
        &self,
        _: WorkspaceTarget,
        task: String,
        interaction: String,
        answer: Value,
    ) -> BoxFuture<'static, Result<(), String>> {
        self.resolved
            .lock()
            .unwrap()
            .push((task.clone(), interaction, answer));
        let rest = self.pending_resume.lock().unwrap().remove(&task);
        let sessions = self.sessions.clone();
        async move {
            if let Some(rest) = rest {
                emit(
                    &sessions,
                    &task,
                    vec![json!({"op": "state.updated", "patch": {"pendingInteractions": []}})],
                );
                tokio::spawn(play(sessions, task, rest));
            }
            Ok(())
        }
        .boxed()
    }

    fn subscribe(
        &self,
        _: WorkspaceTarget,
        task: String,
    ) -> BoxFuture<'static, Result<BoxStream<'static, Value>, String>> {
        if *self.subscribe_fails.lock().unwrap() {
            return async { Err("subscribe unavailable".to_owned()) }.boxed();
        }
        self.ensure_session(&task);
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        {
            let mut sessions = self.sessions.lock().unwrap();
            let session = sessions.get_mut(&task).unwrap();
            let snapshot = json!({"topic": format!("conversation/{task}"), "subscriptionId": "s", "fromSeq": 0,
                "toSeq": session.seq, "payload": {"kind": "snapshot", "snapshot": {
                    "control": {"phase": if session.status.is_empty() { "draft" } else { "completedSuccess" }, "lastError": null},
                    "pendingInteractions": [], "rows": {"window": session.rows}}}});
            let _ = sender.send(snapshot);
            session.sinks.push(sender);
        }
        async move {
            Ok(futures_util::stream::poll_fn(move |context| receiver.poll_recv(context)).boxed())
        }
        .boxed()
    }
}

/// 一个隔离的 Bots：配置目录在临时 HOME 下，所有通道指向 `origin`。
pub fn bots_with(dir: &std::path::Path, origin: &str, tasks: Arc<dyn TaskPort>) -> Arc<Bots> {
    bots_in(dir, origin, tasks, false)
}

/// 同 [`bots_with`]；`background` 为 true 时配置变更会自动对齐通道运行时（轮询 / 长连接）。
pub fn bots_in(
    dir: &std::path::Path,
    origin: &str,
    tasks: Arc<dyn TaskPort>,
    background: bool,
) -> Arc<Bots> {
    bots_with_remote(dir, origin, tasks, Arc::new(NoRemote), background)
}

/// 同 [`bots_in`]，注入给定的远端运行时。
pub fn bots_with_remote(
    dir: &std::path::Path,
    origin: &str,
    tasks: Arc<dyn TaskPort>,
    remote: Arc<dyn RemoteBots>,
    background: bool,
) -> Arc<Bots> {
    let config_dir = dir.join(".zcode").join("v2");
    std::fs::create_dir_all(&config_dir).unwrap();
    if !config_dir.join("setting.json").exists() {
        std::fs::write(
            config_dir.join("setting.json"),
            json!({"lastWorkspaceSession": [{"kind": "local", "workspacePath": "/tmp/ws-bot"},
                {"kind": "local", "workspacePath": "/tmp/ws-other"}]})
            .to_string(),
        )
        .unwrap();
    }
    let bots = Bots::new(BotsDeps {
        repo: BotsRepo::new(config_dir.clone()),
        credentials: CredentialStore::new(&config_dir, "test"),
        providers: Arc::new(Providers::new(Endpoints::local(origin)).unwrap()),
        settings: SettingStore::new(&zcode_host_base::testing::spawner(), dir),
        tasks,
        remote,
        attachments_dir: config_dir.join("bot-attachments"),
        background,
    });
    *bots.name_retry_ms.lock().unwrap() = vec![0];
    *bots.webhook_retry_ms.lock().unwrap() = vec![0, 10, 20];
    bots
}

/// 一个已绑定 `user` 的启用 Bot 的保存参数。
pub fn bound_bot(id: &str, provider: &str, user: &str, extra: Value) -> Value {
    let mut bot = json!({"id": id, "name": id, "provider": provider, "enabled": true,
        "providerUserId": user, "allowedWorkspaces": ["*"],
        "allowedCommands": {"status": true, "new": true, "workspace": true, "model": true,
            "thoughtLevel": true, "reply": true},
        "currentOptions": {}});
    for (key, value) in extra.as_object().cloned().unwrap_or_default() {
        bot[key] = value;
    }
    bot
}
