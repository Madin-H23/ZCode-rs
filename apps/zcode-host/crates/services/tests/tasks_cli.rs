//! zcode-task 生命周期成员对真实 Rust CLI 的集成测试（rust-host-tasks.md 第 6 节）：
//! createTask（legacy session/create）→ sendPrompt（V4 sendText）→ sessions-index 终态 →
//! onDynamicTaskTerminalOutcome / Ready → 配置读写 → 空闲时 enqueueTaskCommand → resume → close。
//! task 命令映射到 runtime 队列的其余情形见 `task_queue_cli.rs`。
//! 模型是本地 mock 的 OpenAI 兼容端点；HOME 与数据目录都是临时目录，不读写开发者的 `~/.zcode`。
//!
//! 缺省用进程内的嵌入式 runtime（docs/specs/rust-u3-single-process.md），不需要预先构建 CLI；设置
//! `ZCODE_TEST_CLI_RUST_BINARY` 时改用该可执行文件走子进程链路，作为差分对照。
#![expect(
    clippy::disallowed_methods,
    reason = "集成测试从进程环境取被测二进制与 PATH"
)]
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use zcode_host_services::agent::sessions_index::SessionsIndexObserver;
use zcode_host_services::agent::{AgentRuntime, AgentRuntimeConfig};
use zcode_host_services::providers::ProviderRuntime;
use zcode_host_services::settings::SettingStore;
use zcode_host_services::tasks::context::{TaskContext, TaskPaths};
use zcode_host_services::tasks::lifecycle::TaskLifecycle;
use zcode_host_services::tasks::service::TaskService;
use zcode_host_services::tasks::{syncer, workspace_events};
use zcode_host_services::tasks_index::TasksIndex;

/// runtime 的选择：缺省嵌入式（Host 的缺省链路）；`ZCODE_TEST_CLI_RUST_BINARY` 指向可执行文件时走子进程链路。
fn runtime_vars() -> BTreeMap<String, String> {
    match std::env::var_os("ZCODE_TEST_CLI_RUST_BINARY").map(PathBuf::from) {
        Some(cli) => {
            assert!(
                cli.is_file(),
                "ZCODE_TEST_CLI_RUST_BINARY is not a file: {}",
                cli.display()
            );
            // 缺陷修复：这里原来递归调用 runtime_vars() 自己，设置该变量时栈溢出，子进程链路的差分
            // 对照从未真正跑过；改为与 agent_cli.rs 相同的显式命令与 runtime 种类。
            BTreeMap::from([
                (
                    "ZCODE_AGENT_SERVER_COMMAND".to_owned(),
                    cli.to_string_lossy().into_owned(),
                ),
                (
                    "ZCODE_AGENT_SERVER_RUNTIME".to_owned(),
                    "zcode-cli-rust".to_owned(),
                ),
            ])
        }
        // 不设置任何选择变量：走 Host 的缺省链路（嵌入式）。
        None => BTreeMap::new(),
    }
}

/// 每个请求都回同一段 SSE 流（含标题生成等附带请求）。
async fn serve_mock_model(listener: TcpListener) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(async move {
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 8192];
            let body_start = loop {
                let Ok(read) = stream.read(&mut chunk).await else {
                    return;
                };
                if read == 0 {
                    return;
                }
                buffer.extend_from_slice(&chunk[..read]);
                if let Some(index) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8_lossy(&buffer[..body_start]).to_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while buffer.len() < body_start + length {
                let Ok(read) = stream.read(&mut chunk).await else {
                    return;
                };
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
            let chunk = |delta: Value, finish: Value| {
                json!({
                    "id": "mock", "object": "chat.completion.chunk", "created": 0, "model": "my-model",
                    "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
                })
            };
            let mut stop = chunk(json!({}), json!("stop"));
            stop["usage"] = json!({"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2});
            let body = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                chunk(
                    json!({"role": "assistant", "content": "Hello from the mock model"}),
                    Value::Null
                ),
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
    stream: &mut (impl futures_util::Stream<Item = Arc<T>> + Unpin),
    what: &str,
    mut matches: impl FnMut(&T) -> bool,
) -> T {
    tokio::time::timeout(Duration::from_secs(90), async {
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

#[tokio::test(flavor = "multi_thread")]
async fn task_lifecycle_against_rust_cli() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let workspace_path = workspace.path().to_string_lossy().into_owned();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve_mock_model(listener));
    let personal = write_provider_config(home.path(), port);
    let builtin = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../config/provider/zcode-builtin.json");
    let home_text = home.path().to_string_lossy().into_owned();
    let base_env = BTreeMap::from([
        ("HOME".to_owned(), home_text.clone()),
        ("USERPROFILE".to_owned(), home_text.clone()),
        ("ZCODE_DATA_BASE_DIR".to_owned(), home_text.clone()),
        ("PATH".to_owned(), std::env::var("PATH").unwrap_or_default()),
        ("NO_PROXY".to_owned(), "127.0.0.1,localhost".to_owned()),
    ]);
    let command_vars = runtime_vars();
    let settings = SettingStore::new(home.path());
    let runtime = AgentRuntime::new(
        AgentRuntimeConfig {
            base_env,
            command_vars,
            fallback_cwd: None,
        },
        settings.clone(),
        ProviderRuntime::new(Some(builtin), personal),
    );
    let index = TasksIndex::open(home.path().join(".zcode/v2/tasks-index.sqlite"), |_| {})
        .await
        .unwrap();
    let events = workspace_events();
    let tasks = TaskService::new(Arc::new(index.clone()), events.clone(), runtime.clone());
    let observer = SessionsIndexObserver::start(runtime.clone());
    let _syncer = syncer::start(&observer, runtime.clone(), tasks.clone());
    let context = TaskContext::new(
        TaskPaths {
            user_home: home.path().into(),
            data_base_dir: home.path().into(),
            log_dir: None,
        },
        index.clone(),
        tasks.clone(),
        runtime.clone(),
        settings,
    );
    let lifecycle = TaskLifecycle::start(context.clone());
    let key = workspace_path.clone();
    let mut workspace_events = Box::pin(events.subscribe_filtered(move |e| e.workspace_key == key));
    let mut terminal = Box::pin(lifecycle.terminal_outcomes().subscribe());
    let mut ready = Box::pin(lifecycle.ready_outcomes().subscribe());

    let initialized = lifecycle
        .initialize(&json!({"workspacePath": workspace_path}))
        .await
        .unwrap();
    assert_eq!(
        initialized,
        json!({"available": true, "version": "ZCode Protocol/1"})
    );

    // createTask：legacy session/create → 写索引 → grouped 顶层 → 配置事件 → task_created。
    let created = lifecycle
        .create_task(&json!({"workspacePath": workspace_path, "mode": "build"}))
        .await
        .unwrap();
    let task_id = created["taskId"].as_str().unwrap().to_owned();
    assert_eq!(created["provider"], "glm");
    assert!(created["initialSlashCommands"].is_array());
    let config = next_matching(&mut workspace_events, "config update", |e| {
        e.event["type"] == "workspace_config_options_update"
    })
    .await;
    assert_eq!(config.event["configOptions"][0]["id"], "model");
    let created_event = next_matching(&mut workspace_events, "task_created", |e| {
        e.event["reason"] == "task_created"
    })
    .await;
    assert_eq!(created_event.event["taskMeta"]["taskId"], task_id.as_str());
    let structure = index
        .query_grouped_task_view_structure(json!([{"workspacePath": workspace_path}]))
        .await
        .unwrap();
    assert_eq!(structure["topLevelOrders"][0]["taskId"], task_id.as_str());

    // sendPrompt（V4 sendText）→ sessions-index 终态 → terminal 带 inputId，随后 ready。
    lifecycle
        .send_prompt(&json!({"taskId": task_id, "traceId": "trace-1", "content": "hi"}))
        .await
        .unwrap();
    let id = task_id.clone();
    let outcome = next_matching(&mut terminal, "terminal outcome", |e| e.0 == id).await;
    assert_eq!(
        outcome.1,
        json!({"taskId": task_id, "inputId": "trace-1", "outcome": "succeeded"})
    );
    let id = task_id.clone();
    let settled = next_matching(&mut ready, "ready", |e| e.0 == id).await;
    assert_eq!(settled.1["reason"], "prompt_completed");

    // 配置：读 configOptions、切到 yolo 权限模式（V4 CAS）、读当前模型。
    let options = lifecycle
        .get_task_config_options(&json!({"taskId": task_id}))
        .await
        .unwrap();
    assert_eq!(options[1]["id"], "mode");
    let options = lifecycle
        .set_config_option(&json!({"taskId": task_id, "configId": "mode", "value": "yolo"}))
        .await
        .unwrap();
    assert_eq!(options[1]["currentValue"], "yolo");
    let selection = lifecycle
        .get_task_model_selection(&json!({"taskId": task_id}))
        .await
        .unwrap();
    assert_eq!(selection["modelId"], "my-model");
    let usage = lifecycle
        .get_task_token_usage(&json!({"taskId": task_id, "workspacePath": workspace_path}))
        .await
        .unwrap();
    assert!(usage.is_object(), "usage: {usage}");

    // 空闲会话入队：runtime 立即开跑，终态的 inputId 是 traceId；跑完后取消回 not_found。
    let enqueued = lifecycle
        .enqueue_command(
            &json!({"workspacePath": workspace_path, "taskId": task_id, "commandId": "c1",
            "traceId": "trace-2", "type": "send_prompt", "content": "again"}),
        )
        .await
        .unwrap();
    assert_eq!(enqueued["command"]["status"], "accepted");
    let id = task_id.clone();
    let outcome = next_matching(&mut terminal, "queued terminal", |e| e.0 == id).await;
    assert_eq!(outcome.1["inputId"], "trace-2");
    let canceled = lifecycle
        .cancel_command(
            &json!({"workspacePath": workspace_path, "taskId": task_id, "commandId": "c1"}),
        )
        .await
        .unwrap();
    assert_eq!(canceled["reason"], "not_found");

    // resume 与 close：close 写删除墓碑并广播 task_meta_changed。
    let resumed = lifecycle
        .resume_task(&json!({"taskId": task_id, "workspacePath": workspace_path}))
        .await
        .unwrap();
    assert_eq!(resumed["taskId"], task_id.as_str());
    lifecycle
        .close_task(&json!({"taskId": task_id}))
        .await
        .unwrap();
    let deleted = index
        .list_deleted_task_ids(&workspace_path, Some("glm"))
        .await
        .unwrap();
    assert_eq!(deleted, vec![task_id.clone()]);
    runtime.dispose_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_import_against_rust_cli() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let workspace_path = workspace.path().to_string_lossy().into_owned();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve_mock_model(listener));
    let personal = write_provider_config(home.path(), port);
    let builtin = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../config/provider/zcode-builtin.json");
    let home_text = home.path().to_string_lossy().into_owned();
    let runtime = AgentRuntime::new(
        AgentRuntimeConfig {
            base_env: BTreeMap::from([
                ("HOME".to_owned(), home_text.clone()),
                ("ZCODE_DATA_BASE_DIR".to_owned(), home_text.clone()),
                ("PATH".to_owned(), std::env::var("PATH").unwrap_or_default()),
                ("NO_PROXY".to_owned(), "127.0.0.1,localhost".to_owned()),
            ]),
            command_vars: runtime_vars(),
            fallback_cwd: None,
        },
        SettingStore::new(home.path()),
        ProviderRuntime::new(Some(builtin), personal),
    );
    let index = TasksIndex::open(home.path().join(".zcode/v2/tasks-index.sqlite"), |_| {})
        .await
        .unwrap();
    let events = workspace_events();
    let tasks = TaskService::new(Arc::new(index.clone()), events.clone(), runtime.clone());
    let context = TaskContext::new(
        TaskPaths {
            user_home: home.path().into(),
            data_base_dir: home.path().into(),
            log_dir: None,
        },
        index.clone(),
        tasks,
        runtime.clone(),
        SettingStore::new(home.path()),
    );
    let lifecycle = TaskLifecycle::start(context);
    // 一个普通会话、一个侧链会话、一个 worktree 下的会话；只有第一个可导入。
    let project = home.path().join(".claude/projects/-tmp-project");
    std::fs::create_dir_all(project.join("subagents")).unwrap();
    let line = |value: Value| format!("{value}\n");
    std::fs::write(
        project.join("sess-1.jsonl"),
        [
            line(json!({"type": "user", "cwd": workspace_path, "timestamp": "2026-01-01T00:00:00Z", "message": {"content": "Explain the build"}})),
            line(json!({"type": "assistant", "cwd": workspace_path, "timestamp": "2026-01-01T00:00:05Z", "message": {"model": "claude-x", "content": [{"type": "text", "text": "Sure."}]}})),
        ]
        .concat(),
    )
    .unwrap();
    std::fs::write(project.join("side.jsonl"), line(json!({"type": "user", "isSidechain": true, "cwd": workspace_path, "message": {"content": "x"}}))).unwrap();
    std::fs::write(
        project.join("subagents/sub.jsonl"),
        line(json!({"type": "user", "cwd": workspace_path, "message": {"content": "x"}})),
    )
    .unwrap();

    let candidates = lifecycle
        .scan_claude_sessions(&json!({"workspacePath": workspace_path}))
        .await;
    let candidates = candidates.as_array().unwrap();
    assert_eq!(candidates.len(), 1, "{candidates:?}");
    assert_eq!(candidates[0]["sessionId"], "sess-1");
    assert_eq!(candidates[0]["previewTitle"], "Explain the build");

    let key = workspace_path.clone();
    let mut workspace_events = Box::pin(events.subscribe_filtered(move |e| e.workspace_key == key));
    let result = lifecycle
        .import_claude_sessions(&json!({"workspacePath": workspace_path, "sessionIds": ["sess-1", " sess-1 ", "missing"]}))
        .await;
    assert_eq!(result["failed"], json!([]), "{result}");
    assert_eq!(
        result["skipped"][0]["reason"],
        "session_not_found_or_workspace_mismatch"
    );
    let task_id = result["imported"][0]["taskId"].as_str().unwrap().to_owned();
    assert!(task_id.starts_with("claude-import-"));
    let event = next_matching(&mut workspace_events, "import event", |e| {
        e.event["reason"] == "task_meta_changed"
    })
    .await;
    assert_eq!(event.event["taskMeta"]["migrationSource"], "claudeCode");
    let listed = index
        .query_task_list(json!({"kind": "timeline", "workspaceScopes": [{"workspacePath": workspace_path}], "search": "build"}), Some("glm".into()))
        .await
        .unwrap();
    assert_eq!(listed["items"][0]["taskId"], task_id.as_str());
    // CLI 里是真实会话：可以读回导入的历史。
    let resumed = lifecycle
        .resume_task(&json!({"taskId": task_id, "workspacePath": workspace_path}))
        .await
        .unwrap();
    assert_eq!(resumed["taskId"], task_id.as_str());
    runtime.dispose_all().await;
}
