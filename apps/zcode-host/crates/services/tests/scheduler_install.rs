//! U4 的调度（docs/specs/rust-u4-app-host.md §2.4、§6）：经 composition root 完整安装 Host 服务，不建立
//! 任何连接、也不经 Main 转发；Host 内的 Scheduler 把到期的 automation 交给真实派发器执行并结算。
//! runtime 是进程内的嵌入式 runtime，模型是本地 mock 的 OpenAI 兼容端点；HOME 与数据目录都是临时目录。
#![expect(
    clippy::disallowed_methods,
    reason = "集成测试从进程环境取 PATH 交给嵌入式 runtime"
)]
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use zcode_host_control::{ControlRouter, ControlSink, HostToMain, InitLocal};
use zcode_host_kit::ChannelRegistry;
use zcode_host_services::tasks_index::TasksIndex;
use zcode_host_services::tasks_index::automation_repo::{AutomationRepo, NewAutomation};
use zcode_host_services::{HostEnvironment, ServiceContext, install, prepare_storage};

/// 记下 Host 发给 Main 的消息。
#[derive(Default)]
struct Recorder(Mutex<Vec<Value>>);

impl ControlSink for Recorder {
    fn send(&self, message: HostToMain) {
        if let Ok(value) = serde_json::to_value(&message) {
            self.0.lock().unwrap().push(value);
        }
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
                    json!({"role": "assistant", "content": "Done by the scheduler"}),
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

fn write_provider_config(config_dir: &Path, port: u16) {
    std::fs::create_dir_all(config_dir).unwrap();
    std::fs::write(
        config_dir.join("provider_config.json"),
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
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

async fn eventually(what: &str, mut check: impl AsyncFnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(90), async {
        while !check().await {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

#[tokio::test(flavor = "multi_thread")]
async fn due_automation_is_dispatched_and_settled_without_any_connection() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let workspace_path = workspace.path().to_string_lossy().into_owned();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve_mock_model(listener));
    let home_text = home.path().to_string_lossy().into_owned();
    let environment = HostEnvironment {
        home_dir: home.path().into(),
        data_base_dir: home.path().into(),
        user_home: home.path().into(),
        vars: BTreeMap::from([
            ("HOME".to_owned(), home_text.clone()),
            ("USERPROFILE".to_owned(), home_text.clone()),
            ("ZCODE_DATA_BASE_DIR".to_owned(), home_text),
            ("PATH".to_owned(), std::env::var("PATH").unwrap_or_default()),
            ("NO_PROXY".to_owned(), "127.0.0.1,localhost".to_owned()),
            (
                "ZCODE_AGENT_SERVER_RUNTIME".to_owned(),
                "embedded".to_owned(),
            ),
        ]),
        search_roots: Vec::new(),
    };
    write_provider_config(&environment.app_config_dir(), port);
    let storage = prepare_storage(&environment, |_| {}).await.unwrap();

    // Host 启动前已到期（在 misfire 宽限内）的循环任务；模型跟随 workspace 首选。
    let index = TasksIndex::open(
        environment.app_config_dir().join("tasks-index.sqlite"),
        |_| {},
    )
    .await
    .unwrap();
    let repo = AutomationRepo::new(index);
    let due_at = now_ms() - 1_000;
    let created = repo
        .create(
            NewAutomation {
                title: "scheduled".into(),
                cron_expr: "*/5 * * * *".into(),
                prompt: "hello from the scheduler".into(),
                workspace_path: workspace_path.clone(),
                recurring: true,
                ..NewAutomation::default()
            },
            Some(due_at),
            None,
        )
        .await
        .unwrap();
    let automation_id = created["automationId"].as_str().unwrap().to_owned();
    let run_id = format!("{automation_id}:{due_at}");

    let builtin = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../config/provider/zcode-builtin.json");
    let control = Arc::new(Recorder::default());
    let context = Arc::new(ServiceContext {
        environment,
        init: InitLocal {
            zcode_builtin_provider_config_file_path: Some(builtin.to_string_lossy().into_owned()),
            ..InitLocal::default()
        },
        control: control.clone(),
    });
    let mut registry = ChannelRegistry::empty();
    let mut router = ControlRouter::default();
    let installed = install(context, storage, &mut registry, &mut router);

    // 认领 → 派发（建会话、发 prompt）→ 结算：没有任何 attachment，也没有 Main 的 cron-run。
    eventually("dispatch settled", async || {
        repo.get_row(&automation_id, None)
            .await
            .unwrap()
            .is_some_and(|row| row.dispatch_status == "dispatched")
    })
    .await;
    let row = repo.get_row(&automation_id, None).await.unwrap().unwrap();
    assert_eq!(row.run_count, 1);
    assert_eq!(row.scheduled_run_count, 1);
    assert_eq!(row.running, 0);
    assert!(row.next_run_at.is_some_and(|next| next > due_at), "{row:?}");
    let run = repo.get_run(&run_id).await.unwrap().unwrap();
    assert_eq!(run["dispatchStatus"], "dispatched", "{run}");
    assert!(run["sessionId"].as_str().is_some_and(|id| !id.is_empty()));
    // 会话这一轮结束后由派发器回写运行结果。
    eventually("run outcome", async || {
        repo.get_run(&run_id).await.unwrap().unwrap()["outcome"] == "succeeded"
    })
    .await;

    installed.shutdown().await;
    let sent = control.0.lock().unwrap().clone();
    for kind in [
        "cron-run-result",
        "cron-scheduler-wake-request",
        "off-peak-scheduler-wake-request",
    ] {
        assert!(
            !sent.iter().any(|message| message["type"] == kind),
            "{kind} was sent to Main"
        );
    }
}
