//! zcode-task 的 task 命令（enqueue / promote / cancel）映射到 runtime 的输入队列
//! （docs/specs/rust-u3-stage-b.md 3.4）：对真实 runtime（缺省进程内；设置
//! `ZCODE_TEST_CLI_RUST_BINARY` 时走子进程链路）与可挂起的模拟模型。HOME 与数据目录都是临时目录。
#![expect(
    clippy::disallowed_methods,
    reason = "集成测试从进程环境取被测二进制与 PATH"
)]
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use zcode_host_services::agent::sessions_index::SessionsIndexObserver;
use zcode_host_services::agent::wire::{Assembled, Assembler};
use zcode_host_services::agent::{
    AgentRuntime, AgentRuntimeConfig, FrameKind, RuntimePolicy, WorkspaceTarget,
};
use zcode_host_services::providers::ProviderRuntime;
use zcode_host_services::settings::SettingStore;
use zcode_host_services::tasks::agent_calls;
use zcode_host_services::tasks::context::{TaskContext, TaskPaths};
use zcode_host_services::tasks::lifecycle::TaskLifecycle;
use zcode_host_services::tasks::service::TaskService;
use zcode_host_services::tasks::{WorkspaceEvents, syncer};
use zcode_host_services::tasks_index::TasksIndex;

const LIMIT: Duration = Duration::from_secs(90);
/// 带这个标记的输入，模型请求要等测试放行才回复。
const HOLD: &str = "[hold]";
const TITLE: &str = "Generate a concise title";

fn runtime_vars() -> BTreeMap<String, String> {
    match std::env::var_os("ZCODE_TEST_CLI_RUST_BINARY").map(PathBuf::from) {
        Some(cli) => BTreeMap::from([
            (
                "ZCODE_AGENT_SERVER_COMMAND".to_owned(),
                cli.to_string_lossy().into_owned(),
            ),
            (
                "ZCODE_AGENT_SERVER_RUNTIME".to_owned(),
                "zcode-cli-rust".to_owned(),
            ),
        ]),
        None => BTreeMap::new(),
    }
}

/// 这次请求要回答的用户输入：最后一条不是系统提醒（`<…>` 包裹）的 user 消息。被停止的轮没有
/// assistant 回复，它的输入仍在历史里，所以不能看“最后一条 assistant 之后的全部消息”。
fn held(body: &str) -> bool {
    let request: Value = serde_json::from_str(body).unwrap_or_default();
    let text = |content: &Value| match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect(),
        _ => String::new(),
    };
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .rev()
        .filter(|m| m["role"] == "user")
        .map(|m| text(&m["content"]))
        .find(|text| !text.trim_start().starts_with('<'))
        .is_some_and(|text| text.contains(HOLD))
}

async fn read_body(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let start = loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8_lossy(&buffer[..start]).to_lowercase();
    let length = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while buffer.len() < start + length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    Some(String::from_utf8_lossy(&buffer[start..]).into_owned())
}

/// OpenAI 兼容的模拟模型：带 HOLD 的输入等 `release` 放行一次再回复；连接被关闭（run 被取消）时
/// 计数照样减回。
struct Mock {
    holding: Arc<AtomicUsize>,
    release: Arc<Semaphore>,
}

fn serve_mock_model(listener: TcpListener) -> Mock {
    let holding = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Semaphore::new(0));
    let (count, gate) = (holding.clone(), release.clone());
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let (count, gate) = (count.clone(), gate.clone());
            tokio::spawn(async move {
                let Some(body) = read_body(&mut stream).await else {
                    return;
                };
                if !body.contains(TITLE) && held(&body) {
                    count.fetch_add(1, Ordering::SeqCst);
                    let mut rest = [0u8; 64];
                    tokio::select! {
                        permit = gate.acquire() => permit.unwrap().forget(),
                        _ = async { while matches!(stream.read(&mut rest).await, Ok(n) if n > 0) {} } => {
                            count.fetch_sub(1, Ordering::SeqCst);
                            return;
                        }
                    }
                    count.fetch_sub(1, Ordering::SeqCst);
                }
                let chunk = |delta: Value, finish: Value| {
                    json!({"id": "mock", "object": "chat.completion.chunk", "created": 0,
                        "model": "my-model", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
                };
                let mut stop = chunk(json!({}), json!("stop"));
                stop["usage"] =
                    json!({"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2});
                let body = format!(
                    "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    chunk(json!({"role": "assistant", "content": "done"}), Value::Null),
                    stop
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    Mock { holding, release }
}

fn write_provider_config(home: &Path, port: u16) -> PathBuf {
    let config_dir = home.join(".zcode/v2");
    std::fs::create_dir_all(&config_dir).unwrap();
    let personal = config_dir.join("provider_config.json");
    std::fs::write(
        &personal,
        json!({"schemaVersion": 1, "config": {
            "providerOrder": ["new-provider"],
            "providerConfigRules": {"providerRules": [{
                "providerId": "new-provider", "providerName": "Mock",
                "config": {
                    "group": "standard-personal",
                    "access": {"type": "api-key", "apiKey": "sk-test"},
                    "api": {"type": "openai-chat-completions", "baseUrl": format!("http://127.0.0.1:{port}/v1")},
                    "personalModelIds": ["my-model"], "modelOrder": ["my-model"],
                },
            }]},
            "modelConfigRules": {
                "providerModelRules": [{"providerId": "new-provider", "modelId": "my-model", "config": {"enabled": true}}],
                "manualProviderModelRules": [],
            },
        }})
        .to_string(),
    )
    .unwrap();
    personal
}

async fn next_matching<T: Clone>(
    stream: &mut (impl Stream<Item = Arc<T>> + Unpin),
    what: &str,
    mut matches: impl FnMut(&T) -> bool,
) -> T {
    tokio::time::timeout(LIMIT, async {
        loop {
            let event = stream.next().await.expect("event stream ended");
            if matches(&event) {
                return (*event).clone();
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

struct Harness {
    _home: tempfile::TempDir,
    _workspace: tempfile::TempDir,
    path: String,
    target: WorkspaceTarget,
    mock: Mock,
    runtime: AgentRuntime,
    lifecycle: Arc<TaskLifecycle>,
    _observer: Arc<SessionsIndexObserver>,
    _syncer: tokio::task::JoinHandle<()>,
}

async fn harness() -> Harness {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().to_string_lossy().into_owned();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mock = serve_mock_model(listener);
    let personal = write_provider_config(home.path(), port);
    let builtin = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../config/provider/zcode-builtin.json");
    let home_text = home.path().to_string_lossy().into_owned();
    let settings = SettingStore::new(home.path());
    let runtime = AgentRuntime::new(
        AgentRuntimeConfig {
            base_env: BTreeMap::from([
                ("HOME".to_owned(), home_text.clone()),
                ("USERPROFILE".to_owned(), home_text.clone()),
                ("ZCODE_DATA_BASE_DIR".to_owned(), home_text),
                ("PATH".to_owned(), std::env::var("PATH").unwrap_or_default()),
                ("NO_PROXY".to_owned(), "127.0.0.1,localhost".to_owned()),
            ]),
            command_vars: runtime_vars(),
            fallback_cwd: None,
        },
        settings.clone(),
        ProviderRuntime::new(Some(builtin), personal),
    );
    let index = TasksIndex::open(home.path().join(".zcode/v2/tasks-index.sqlite"), |_| {})
        .await
        .unwrap();
    let tasks = TaskService::new(
        Arc::new(index.clone()),
        WorkspaceEvents::new(),
        runtime.clone(),
    );
    let observer = SessionsIndexObserver::start(runtime.clone());
    let syncer = syncer::start(&observer, runtime.clone(), tasks.clone());
    let context = TaskContext::new(
        TaskPaths {
            user_home: home.path().into(),
            data_base_dir: home.path().into(),
            log_dir: None,
        },
        index,
        tasks,
        runtime.clone(),
        settings,
    );
    Harness {
        _home: home,
        _workspace: workspace,
        target: WorkspaceTarget::local(path.clone()),
        path,
        mock,
        runtime,
        lifecycle: TaskLifecycle::start(context),
        _observer: observer,
        _syncer: syncer,
    }
}

impl Harness {
    async fn create(&self) -> String {
        let created = self
            .lifecycle
            .create_task(&json!({"workspacePath": self.path, "mode": "build"}))
            .await
            .unwrap();
        created["taskId"].as_str().unwrap().to_owned()
    }

    /// 发一条输入（`sendPrompt`）；带 HOLD 时等到它的模型请求挂起。
    async fn send(&self, task: &str, trace: &str, content: &str) {
        let held = self.mock.holding.load(Ordering::SeqCst);
        self.lifecycle
            .send_prompt(&json!({"taskId": task, "traceId": trace, "content": content}))
            .await
            .unwrap();
        if content.contains(HOLD) {
            self.held(held + 1).await;
        }
    }

    async fn held(&self, count: usize) {
        let holding = self.mock.holding.clone();
        tokio::time::timeout(LIMIT, async {
            while holding.load(Ordering::SeqCst) != count {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("expected {count} held model requests"));
    }

    fn params(&self, task: &str, command: &str, extra: Value) -> Value {
        let mut params = json!({"workspacePath": self.path, "taskId": task, "commandId": command});
        if let (Some(params), Value::Object(extra)) = (params.as_object_mut(), extra) {
            params.extend(extra);
        }
        params
    }

    fn enqueue_params(&self, task: &str, command: &str, trace: &str, content: &str) -> Value {
        self.params(
            task,
            command,
            json!({"traceId": trace, "type": "send_prompt", "content": content}),
        )
    }

    /// runtime 的队列：(externalId, sourceCommandId, dispatch.state)，以及 autoDrain 与运行中的输入。
    async fn queue(&self, task: &str) -> (Vec<(String, String, String)>, bool, Value) {
        let state = agent_calls::read(
            &self.runtime,
            &self.target,
            "v4/conversation/inputQueue",
            json!({"sessionId": task}),
        )
        .await
        .unwrap();
        let items = state["queue"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                let text = |key: &str| item[key].as_str().unwrap_or_default().to_owned();
                (
                    text("externalId"),
                    text("sourceCommandId"),
                    item["dispatch"]["state"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        let auto = state["queue"]["autoDrain"].as_bool().unwrap();
        (items, auto, state["activeInput"].clone())
    }

    async fn idle(&self, task: &str) {
        tokio::time::timeout(LIMIT, async {
            while !self.queue(task).await.2.is_null() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the session did not become idle");
    }
}

fn item(external: &str, source: &str) -> (String, String, String) {
    (external.into(), source.into(), "queued".into())
}

/// 一条 V4 会话订阅收到的逻辑帧（快照或增量），按 clientMode 投递。
async fn conversation_frames(h: &Harness, task: &str, mode: &str) -> UnboundedReceiver<Value> {
    let key = h.target.key();
    let hub = h.runtime.frames();
    let (sender, receiver) = unbounded_channel();
    let (subscribed, ready) = tokio::sync::oneshot::channel();
    let (route, routed) = tokio::sync::oneshot::channel::<String>();
    tokio::spawn(async move {
        // 先挂上帧流再发订阅：runtime 在 ACK 之后立即写初始快照。
        let mut frames = Box::pin(hub.subscribe_filtered(move |frame| {
            frame.kind == FrameKind::Conversation && frame.workspace_key == key
        }));
        let _ = subscribed.send(());
        let Ok(subscription) = routed.await else {
            return;
        };
        let mut assembler = Assembler::new();
        while let Some(frame) = frames.next().await {
            if frame.subscription_id != subscription {
                continue;
            }
            let Ok(wire) = serde_json::from_str::<Value>(frame.raw.get()) else {
                continue;
            };
            if let Assembled::Frame { frame, .. } = assembler.accept(&wire)
                && sender.send(frame).is_err()
            {
                return;
            }
        }
    });
    ready.await.unwrap();
    let process = h
        .runtime
        .get_client(&h.target, RuntimePolicy::StartIfNeeded)
        .await
        .unwrap();
    let ack = process
        .request_value(
            "v4/conversation/subscribe",
            &json!({"topic": format!("conversation/{task}"),
                "connectionId": format!("queue-test-{mode}"), "clientMode": mode}),
        )
        .await
        .unwrap();
    route
        .send(ack["ack"]["subscriptionId"].as_str().unwrap().to_owned())
        .unwrap();
    receiver
}

/// 帧里的队列项 externalId（快照的 queue，或增量里 state.updated 的 queue）。
fn queued_ids(frame: &Value) -> Option<Vec<String>> {
    let payload = &frame["payload"];
    let queue = if payload["kind"] == "snapshot" {
        Some(&payload["snapshot"]["queue"])
    } else {
        payload["deltas"]
            .as_array()?
            .iter()
            .rev()
            .find(|d| d["op"] == "state.updated" && d["patch"].get("queue").is_some())
            .map(|d| &d["patch"]["queue"])
    }?;
    Some(
        queue["items"]
            .as_array()?
            .iter()
            .map(|item| item["externalId"].as_str().unwrap_or_default().to_owned())
            .collect(),
    )
}

async fn frame_with_queue(frames: &mut UnboundedReceiver<Value>, expected: &[&str]) {
    tokio::time::timeout(LIMIT, async {
        loop {
            let frame = frames.recv().await.expect("conversation frames ended");
            if queued_ids(&frame).is_some_and(|ids| ids == expected) {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no frame with queue {expected:?}"));
}

/// 运行中入队 → 队列在 runtime（两种投递都看到）→ 重复入队不加项 → promote 改顺序 → cancel 移除
/// 并发事件 → 找不到的项按约定的形状回复 → run 结束后 runtime 开跑队首，终态带 traceId → 正在运行的
/// 项 promote 回 running、cancel 回 already_running → owner 错误码不变。
#[tokio::test(flavor = "multi_thread")]
async fn queued_commands_live_in_the_runtime_queue() {
    let h = harness().await;
    let task = h.create().await;
    let mut terminal = Box::pin(h.lifecycle.terminal_outcomes().subscribe());
    let mut stream = Box::pin(h.lifecycle.stream_events().subscribe());
    h.send(&task, "t1", &format!("{HOLD} first")).await;
    let mut continuous = conversation_frames(&h, &task, "desktop-continuous").await;
    let mut replayable = conversation_frames(&h, &task, "web-remote-replayable").await;

    let enqueue = |command: &str, trace: &str, content: &str, owner: Option<&str>| {
        let mut params = h.enqueue_params(&task, command, trace, content);
        if let Some(owner) = owner {
            params["ownerRunId"] = owner.into();
        }
        params
    };
    let enqueued = h
        .lifecycle
        .enqueue_command(&enqueue("c2", "t2", "second", Some("t1")))
        .await
        .unwrap();
    assert_eq!(enqueued["accepted"], true);
    let command = &enqueued["command"];
    for (field, value) in [
        ("commandId", "c2"),
        ("traceId", "t2"),
        ("status", "accepted"),
        ("type", "send_prompt"),
        ("content", "second"),
    ] {
        assert_eq!(command[field], value, "{enqueued}");
    }
    assert_eq!(command["workspaceKey"], h.target.key().as_str());
    // 重试同一个 commandId：返回同样的形状，队列里不多一项。
    let again = h
        .lifecycle
        .enqueue_command(&enqueue("c2", "t2", "second", Some("t1")))
        .await
        .unwrap();
    assert_eq!(again["command"]["commandId"], "c2");
    let third = format!("{HOLD} third");
    h.lifecycle
        .enqueue_command(&enqueue("c3", "t3", &third, Some("t1")))
        .await
        .unwrap();
    let (items, auto, active) = h.queue(&task).await;
    assert_eq!(items, vec![item("c2", "t2"), item("c3", "t3")]);
    assert!(auto);
    assert_eq!(active["inputId"], "t1");
    frame_with_queue(&mut continuous, &["c2", "c3"]).await;
    frame_with_queue(&mut replayable, &["c2", "c3"]).await;

    let promoted = h
        .lifecycle
        .promote_command(&h.params(&task, "c3", json!({"ownerRunId": "t1"})))
        .await
        .unwrap();
    assert_eq!(promoted["accepted"], true);
    assert_eq!(promoted["command"]["status"], "accepted");
    assert_eq!(promoted["command"]["traceId"], "t3");
    assert_eq!(promoted["command"]["content"], third.as_str());
    assert_eq!(
        h.queue(&task).await.0,
        vec![item("c3", "t3"), item("c2", "t2")]
    );
    // promote 只改顺序，不取消正在运行的 t1（不是 sendQueuedNow）。
    assert_eq!(h.mock.holding.load(Ordering::SeqCst), 1);
    assert_eq!(h.queue(&task).await.2["inputId"], "t1");
    frame_with_queue(&mut replayable, &["c3", "c2"]).await;

    let canceled = h
        .lifecycle
        .cancel_command(&h.params(&task, "c2", json!({"ownerRunId": "t1"})))
        .await
        .unwrap();
    assert_eq!(
        canceled,
        json!({"canceled": true, "commandId": "c2", "status": "accepted"})
    );
    let event = next_matching(&mut stream, "task_snapshot_updated", |e| e.1 == task).await;
    assert_eq!(event.2["type"], "task_snapshot_updated");
    assert_eq!(event.2["traceId"], "t2");
    assert_eq!(event.2["reason"], "task_status_changed");
    assert_eq!(h.queue(&task).await.0, vec![item("c3", "t3")]);
    frame_with_queue(&mut replayable, &["c3"]).await;
    frame_with_queue(&mut continuous, &["c3"]).await;
    let missing = h
        .lifecycle
        .cancel_command(&h.params(&task, "c2", json!({})))
        .await
        .unwrap();
    assert_eq!(
        missing,
        json!({"canceled": true, "commandId": "c2", "reason": "not_found"})
    );
    let error = h
        .lifecycle
        .promote_command(&h.params(&task, "c2", json!({"ownerRunId": "t1"})))
        .await
        .unwrap_err();
    assert_eq!(error.code_str(), Some("OWNER_COMMAND_FAILED"));
    assert_eq!(error.message, "Task command not found.");
    let error = h
        .lifecycle
        .enqueue_command(&enqueue("c4", "t4", "late", Some("old-run")))
        .await
        .unwrap_err();
    assert_eq!(error.code_str(), Some("STALE_TASK_OWNER_COMMAND"));
    assert_eq!(error.message, "Stale task owner command.");

    // run t1 结束：runtime 自己开跑队首 c3（t3），终态的 inputId 是各自的 traceId。
    h.mock.release.add_permits(1);
    let id = task.clone();
    let outcome = next_matching(&mut terminal, "t1 terminal", |e| e.0 == id).await;
    assert_eq!(outcome.1["inputId"], "t1", "{:?}", outcome.1);
    h.held(1).await;
    let (items, _, active) = h.queue(&task).await;
    assert!(items.is_empty());
    assert_eq!(active["inputId"], "t3");
    assert_eq!(active["externalId"], "c3");
    let promoted = h
        .lifecycle
        .promote_command(&h.params(&task, "c3", json!({"ownerRunId": "t3"})))
        .await
        .unwrap();
    assert_eq!(promoted["command"]["status"], "running");
    assert_eq!(promoted["command"]["traceId"], "t3");
    let running = h
        .lifecycle
        .cancel_command(&h.params(&task, "c3", json!({"ownerRunId": "t3"})))
        .await
        .unwrap();
    assert_eq!(
        running,
        json!({"canceled": false, "commandId": "c3", "reason": "already_running", "status": "running"})
    );
    h.mock.release.add_permits(1);
    let id = task.clone();
    let outcome = next_matching(&mut terminal, "t3 terminal", |e| e.0 == id).await;
    assert_eq!(outcome.1["inputId"], "t3", "{:?}", outcome.1);
    h.idle(&task).await;
    let error = h
        .lifecycle
        .enqueue_command(&enqueue("c5", "t5", "after", Some("t3")))
        .await
        .unwrap_err();
    assert_eq!(error.code_str(), Some("NO_ACTIVE_TASK_OWNER"));
    assert_eq!(error.message, "No active task owner.");
    let finished = h
        .lifecycle
        .cancel_command(&h.params(&task, "c3", json!({})))
        .await
        .unwrap();
    assert_eq!(finished["reason"], "not_found");
    h.runtime.dispose_all().await;
}

/// 空闲时入队立即开跑；run 被停止后队列暂停（held）：入队以 keepQueueAndSend 开跑、暂停的队列保持；
/// 在没有 run 时 promote 开跑该项，不需要取消任何 run。
#[tokio::test(flavor = "multi_thread")]
async fn idle_and_held_sessions() {
    let h = harness().await;
    let task = h.create().await;
    let mut terminal = Box::pin(h.lifecycle.terminal_outcomes().subscribe());
    let enqueued = h
        .lifecycle
        .enqueue_command(&h.enqueue_params(&task, "c1", "t1", "hello"))
        .await
        .unwrap();
    assert_eq!(enqueued["command"]["status"], "accepted");
    let id = task.clone();
    let outcome = next_matching(&mut terminal, "idle enqueue", |e| e.0 == id).await;
    assert_eq!(outcome.1["inputId"], "t1");
    h.idle(&task).await;

    h.send(&task, "ta", &format!("{HOLD} a")).await;
    h.lifecycle
        .enqueue_command(&h.enqueue_params(&task, "cb", "tb", "b"))
        .await
        .unwrap();
    h.lifecycle
        .stop_generation(&json!({"taskId": task, "workspacePath": h.path}))
        .await
        .unwrap();
    let id = task.clone();
    next_matching(&mut terminal, "stopped run", |e| e.0 == id).await;
    h.idle(&task).await;
    let (items, auto, _) = h.queue(&task).await;
    assert_eq!(items, vec![item("cb", "tb")]);
    assert!(!auto, "a stopped run pauses the runtime queue");

    // held：新入队的项开跑，暂停的队列保持。
    h.lifecycle
        .enqueue_command(&h.enqueue_params(&task, "cc", "tc", "c"))
        .await
        .unwrap();
    let id = task.clone();
    let outcome = next_matching(&mut terminal, "held enqueue", |e| e.0 == id).await;
    assert_eq!(outcome.1["inputId"], "tc");
    h.idle(&task).await;
    let (items, auto, _) = h.queue(&task).await;
    assert_eq!(items, vec![item("cb", "tb")]);
    assert!(!auto);

    // 没有 run 时 promote：该项立即开跑。
    let promoted = h
        .lifecycle
        .promote_command(&h.params(&task, "cb", json!({})))
        .await
        .unwrap();
    assert_eq!(promoted["command"]["traceId"], "tb");
    let id = task.clone();
    let outcome = next_matching(&mut terminal, "promoted run", |e| e.0 == id).await;
    assert_eq!(outcome.1["inputId"], "tb");
    h.idle(&task).await;
    assert!(h.queue(&task).await.0.is_empty());
    h.runtime.dispose_all().await;
}

/// 带附件的项走 legacy `session/send`：即使当前 run 由 legacy 发送开跑（持有 legacy 锁）也入队，
/// 可以按 commandId 取消。
#[tokio::test(flavor = "multi_thread")]
async fn an_item_with_an_attachment_queues_under_the_legacy_lock() {
    let h = harness().await;
    let task = h.create().await;
    let attachment = json!([{"kind": "file", "filename": "notes.txt", "mimeType": "text/plain", "textContent": "hello"}]);
    let held = h.mock.holding.load(Ordering::SeqCst);
    h.lifecycle
        .send_prompt(&json!({"taskId": task, "traceId": "t1",
            "content": format!("{HOLD} first"), "attachments": attachment}))
        .await
        .unwrap();
    h.held(held + 1).await;
    let mut params = h.enqueue_params(&task, "c2", "t2", "with a file");
    params["attachments"] = attachment;
    let enqueued = h.lifecycle.enqueue_command(&params).await.unwrap();
    assert_eq!(
        enqueued["command"]["attachments"][0]["filename"],
        "notes.txt"
    );
    assert_eq!(h.queue(&task).await.0, vec![item("c2", "t2")]);
    let canceled = h
        .lifecycle
        .cancel_command(&h.params(&task, "c2", json!({})))
        .await
        .unwrap();
    assert_eq!(canceled["canceled"], true, "{canceled}");
    assert!(h.queue(&task).await.0.is_empty());
    h.mock.release.add_permits(1);
    h.idle(&task).await;
    h.runtime.dispose_all().await;
}

/// runtime 实例重启：排队项丢弃。promote 找不到、cancel 回 not_found；同一 traceId 再入队由 runtime
/// 报告 inputDiscardedOnRestart。
#[tokio::test(flavor = "multi_thread")]
async fn a_runtime_restart_discards_queued_items() {
    let h = harness().await;
    let task = h.create().await;
    h.send(&task, "t1", &format!("{HOLD} first")).await;
    h.lifecycle
        .enqueue_command(&h.enqueue_params(&task, "c2", "t2", "second"))
        .await
        .unwrap();
    assert_eq!(h.queue(&task).await.0, vec![item("c2", "t2")]);
    h.runtime.dispose_workspace(&h.target).await;
    h.held(0).await;

    let (items, _, active) = h.queue(&task).await;
    assert!(items.is_empty() && active.is_null(), "{items:?} {active}");
    let error = h
        .lifecycle
        .promote_command(&h.params(&task, "c2", json!({})))
        .await
        .unwrap_err();
    assert_eq!(error.code_str(), Some("OWNER_COMMAND_FAILED"));
    let canceled = h
        .lifecycle
        .cancel_command(&h.params(&task, "c2", json!({})))
        .await
        .unwrap();
    assert_eq!(canceled["reason"], "not_found");
    let error = h
        .lifecycle
        .enqueue_command(&h.enqueue_params(&task, "c2", "t2", "second"))
        .await
        .unwrap_err();
    assert_eq!(
        error.code_str(),
        Some("ZCODE_V4_COMMAND_REJECTED"),
        "{error:?}"
    );
    assert!(
        error
            .message
            .contains("fault.command.inputDiscardedOnRestart"),
        "{}",
        error.message
    );
    h.runtime.dispose_all().await;
}
