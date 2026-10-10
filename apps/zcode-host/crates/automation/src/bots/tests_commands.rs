//! 选择命令、问答与流式卡片退避的测试（经 `handle_inbound`，回复直接取返回值）。
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::feishu::{Actor, Inbound};
use super::interaction::selection_token;
use super::test_support::{
    Answer, FakeTasks, MockServer, Script, bots_with, bound_bot, eventually,
};

fn inbound(text: &str) -> Inbound {
    Inbound {
        bot_id: "tg-1".into(),
        text: text.into(),
        actor: Actor {
            provider: "telegram".into(),
            bot_id: "tg-1".into(),
            provider_user_id: "42".into(),
            chat_type: "private".into(),
            chat_id: Some("42".into()),
            ..Actor::default()
        },
        ..Inbound::default()
    }
}

async fn ask(bots: &Arc<super::service::Bots>, text: &str) -> Vec<Value> {
    bots.handle_inbound(&inbound(text)).await.unwrap()
}

fn labels(reply: &[Value]) -> Vec<String> {
    reply[0]["selection"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|option| option["label"].as_str().unwrap().to_owned())
        .collect()
}

async fn setup(tasks: Arc<FakeTasks>) -> (tempfile::TempDir, Arc<super::service::Bots>) {
    let dir = tempfile::tempdir().unwrap();
    // 没有 token 的 Telegram Bot：出站发送是空操作，回复从返回值读取。
    let bots = bots_with(dir.path(), "http://127.0.0.1:9", tasks);
    bots.save_bot(&json!({"bot": bound_bot("tg-1", "telegram", "42", json!({}))}))
        .await
        .unwrap();
    (dir, bots)
}

#[tokio::test]
async fn draft_selection_commands_follow_node() {
    let tasks = FakeTasks::new();
    let (_dir, bots) = setup(tasks.clone()).await;
    let reply = ask(&bots, "/model").await;
    assert_eq!(reply[0]["text"], "当前模型 Z.ai/GLM-5\n选择模型供应商");
    assert_eq!(labels(&reply), ["Z.ai · 当前", "Mock"]);
    assert_eq!(reply[0]["selection"]["cancelLabel"], "取消");
    let reply = ask(&bots, "/model provider 2").await;
    assert_eq!(reply[0]["text"], "当前模型 custom:zai:GLM-5\n选择模型");
    assert_eq!(
        reply[0]["selection"]["options"],
        json!([{"id": "custom:mock:m1", "label": "m1"}])
    );
    let reply = ask(&bots, "/model model 1").await;
    assert!(
        reply[0]["text"].as_str().unwrap().contains("模型: Mock/m1"),
        "{}",
        reply[0]
    );
    let state = bots.repo.read_state().await.unwrap();
    assert_eq!(
        state["bots"]["tg-1"]["draftOptions"],
        json!({"provider": "glm", "modelSelection": {"providerId": "mock", "modelId": "m1", "options": {"reasoningLevel": "on"}}, "mode": "yolo"})
    );
    let reply = ask(&bots, "/think").await;
    assert_eq!(reply[0]["text"], "当前思考级别 on\n选择思考级别");
    assert_eq!(labels(&reply), ["off", "on · 当前"]);
    ask(&bots, "/think 1").await;
    let state = bots.repo.read_state().await.unwrap();
    assert_eq!(
        state["bots"]["tg-1"]["draftOptions"]["modelSelection"]["options"]["reasoningLevel"],
        "off"
    );
    assert_eq!(
        ask(&bots, "/mode plan").await[0]["text"],
        "机器人已锁定 **yolo** 运行模式，无法切换。"
    );
    let reply = ask(&bots, "/reply").await;
    assert_eq!(labels(&reply), ["标准回复 · 当前", "完整回复", "摘要回复"]);
    ask(&bots, "/reply 3").await;
    assert_eq!(
        bots.repo.read_config().await.unwrap()["bots"][0]["replyMode"],
        "summary_changes"
    );
    assert_eq!(
        ask(&bots, "/task").await[0]["text"],
        "当前 workspace 没有历史任务。"
    );
    let reply = ask(&bots, "/workspace").await;
    assert_eq!(labels(&reply), ["ws-bot · 当前", "ws-other"]);
    let reply = ask(&bots, "/workspace ws-other").await;
    assert!(
        reply[0]["text"]
            .as_str()
            .unwrap()
            .starts_with("工作区: ws-other"),
        "{}",
        reply[0]
    );
    // 切换 workspace 进入新草稿，原草稿模型不保留。
    let state = bots.repo.read_state().await.unwrap();
    assert_eq!(
        state["bots"]["tg-1"]["draftOptions"],
        json!({"provider": "glm", "mode": "yolo"})
    );
    // 没有待选项时取消回未知命令 0；待选项被普通文本清掉。
    assert_eq!(ask(&bots, "/cancel").await[0]["text"], "未知命令：**/0**");
    ask(&bots, "/reply").await;
    let reply = ask(&bots, "/cancel").await;
    assert!(
        reply[0]["text"]
            .as_str()
            .unwrap()
            .starts_with("工作区: ws-other")
    );
    assert_eq!(ask(&bots, "/nope").await[0]["text"], "未知命令：**/nope**");
}

#[tokio::test]
async fn task_commands_inherit_and_guard_running_tasks() {
    let tasks = FakeTasks::new();
    tasks.script.lock().unwrap().delay = Duration::from_millis(300);
    let (_dir, bots) = setup(tasks.clone()).await;
    assert!(ask(&bots, "hello").await.is_empty());
    // 运行中：切换上下文的命令都回 taskRunning。
    assert_eq!(
        ask(&bots, "/model").await[0]["text"],
        "当前任务正在运行，稍后再试，或使用 **/停止** 停止当前任务。"
    );
    assert_eq!(
        ask(&bots, "/new").await[0]["text"],
        ask(&bots, "/task").await[0]["text"]
    );
    assert_eq!(
        ask(&bots, "hello again").await[0]["text"],
        "当前任务正在运行，稍后再试，或使用 **/停止** 停止当前任务。"
    );
    eventually("task finished", || {
        bots.running.lock().unwrap().is_empty().then_some(())
    })
    .await;
    let status = ask(&bots, "/status").await[0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(status.contains("模型: Z.ai/GLM-5$high"), "{status}");
    assert!(status.contains("任务: Task 1 (task-1)"), "{status}");
    assert!(status.contains("状态: ✅ 已完成"), "{status}");
    let reply = ask(&bots, "/task").await;
    assert_eq!(reply[0]["text"], "当前任务 Task 1 (task-1)\n选择任务");
    assert_eq!(labels(&reply), ["Task 1 · 当前"]);
    assert_eq!(
        reply[0]["selection"]["options"][0]["description"],
        "completed"
    );
    // /think 在任务态读会话 configOptions，选中后 V4 switchModelConfig。
    let reply = ask(&bots, "/think").await;
    assert_eq!(labels(&reply), ["Low", "High · 当前"]);
    ask(&bots, "/think low").await;
    assert_eq!(
        tasks.thought_levels.lock().unwrap().clone(),
        vec![("task-1".to_owned(), "low".to_owned())]
    );
    // 任务态 /model 两级选择后 setModel（补成目标模型最高档）。
    ask(&bots, "/model provider zai").await;
    ask(&bots, "/model model GLM-4.7").await;
    assert_eq!(
        tasks.set_models.lock().unwrap().clone(),
        vec![(
            "task-1".to_owned(),
            json!({"providerId": "zai", "modelId": "GLM-4.7", "options": {"reasoningLevel": "medium"}})
        )]
    );
    // /new 继承当前任务的模型与档位（去掉 `$level` 后缀），模式硬锁 yolo。
    let reply = ask(&bots, "/new").await;
    assert!(reply[0]["text"].as_str().unwrap().contains("任务: 草稿"));
    let state = bots.repo.read_state().await.unwrap();
    assert_eq!(
        state["bots"]["tg-1"]["draftOptions"],
        json!({"provider": "glm", "modelSelection": {"providerId": "zai", "modelId": "GLM-5", "options": {"reasoningLevel": "high"}}, "mode": "yolo"})
    );
    ask(&bots, "/task 1").await;
    let state = bots.repo.read_state().await.unwrap();
    assert_eq!(state["bots"]["tg-1"]["activeTaskId"], "task-1");
    // 续用任务：以会话原选择解析本次输入。
    assert!(ask(&bots, "continue").await.is_empty());
    let sent = eventually("continue sent", || {
        let sent = tasks.sent.lock().unwrap().clone();
        (sent.len() == 2).then_some(sent)
    })
    .await;
    assert_eq!(sent.last().unwrap().0, "task-1");
    assert_eq!(sent.last().unwrap().1.text, "continue");
    // /stop 停当前任务并回状态。
    let reply = ask(&bots, "/stop").await;
    assert!(
        reply[0]["text"]
            .as_str()
            .unwrap()
            .starts_with("工作区: ws-bot")
    );
    assert_eq!(
        tasks.stopped.lock().unwrap().clone(),
        vec!["task-1".to_owned()]
    );
}

#[tokio::test]
async fn elicitation_buttons_advance_and_submit() {
    let tasks = FakeTasks::new();
    *tasks.script.lock().unwrap() = Script {
        chunks: vec!["done".into()],
        interaction: Some(
            json!({"interactionId": "ask-1", "kind": "userInput", "anchorRowId": null, "createdAt": 1,
            "payload": {"kind": "userInput", "prompt": "Q", "freeText": true, "traceId": "run-1", "questions": [
                {"question": "Size?", "header": "Size", "options": [{"value": "s", "label": "S"}, {"value": "l", "label": "L"}]},
                {"question": "Extras?", "header": "Extras", "multiSelect": true,
                 "options": [{"value": "x", "label": "X"}, {"value": "y", "label": "Y"}]}]}}),
        ),
        ..Script::default()
    };
    let (_dir, bots) = setup(tasks.clone()).await;
    ask(&bots, "go").await;
    let pending = eventually("pending elicitation", || {
        let state = std::fs::read_to_string(bots.repo.dir().join("bot-state.v3.json")).ok()?;
        let state: Value = serde_json::from_str(&state).ok()?;
        state["bots"]["tg-1"]["pendingElicitation"]
            .as_object()
            .cloned()
            .map(Value::Object)
    })
    .await;
    assert_eq!(pending["requestId"], "ask-1");
    assert_eq!(pending["runId"], "run-1");
    assert_eq!(pending["actorKey"], "tg-1::telegram::42");
    let token = selection_token(&pending);
    // 不带本轮 token 的按钮过期。
    assert_eq!(
        ask(&bots, "/elicitation 2").await[0]["text"],
        "问答请求已过期，请在 zcode UI 中处理。"
    );
    let reply = ask(&bots, &format!("/elicitation {token} 2")).await;
    assert_eq!(reply[0]["elicitation"]["currentQuestionIndex"], 1);
    assert_eq!(reply[0]["elicitation"]["answers"], json!({"0": ["l"]}));
    assert_eq!(labels(&reply), ["[ ] X", "[ ] Y", "完成"]);
    let mut second = pending.clone();
    second["currentQuestionIndex"] = json!(1);
    let token = selection_token(&second);
    let reply = ask(&bots, &format!("/elicitation {token} 1")).await;
    assert_eq!(labels(&reply), ["[x] X", "[ ] Y", "完成"]);
    let reply = ask(&bots, &format!("/elicitation {token} __submit__")).await;
    assert_eq!(reply[0]["text"], "已提交问答响应。");
    assert_eq!(reply[0]["elicitation"]["status"], "completed");
    let resolved = tasks.resolved.lock().unwrap().clone();
    assert_eq!(
        resolved,
        vec![(
            "task-1".to_owned(),
            "ask-1".to_owned(),
            json!({"action": "accept", "content": {
            "answers": {"Size?": "l", "Extras?": "x"}, "answer_0": "l", "answer_1": ["x"]}})
        )]
    );
    eventually("task finished", || {
        bots.running.lock().unwrap().is_empty().then_some(())
    })
    .await;
}

#[tokio::test]
async fn streaming_card_failures_back_off_then_open_the_circuit() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    server.on(|request| {
        Some(match (request.method.as_str(), request.path.as_str()) {
            (_, "/open-apis/auth/v3/tenant_access_token/internal") => {
                Answer::json(200, json!({"code": 0, "tenant_access_token": "t-1"}))
            }
            ("POST", "/open-apis/im/v1/messages") => {
                Answer::json(200, json!({"code": 0, "data": {"message_id": "om_1"}}))
            }
            ("PATCH", _) => Answer::json(400, json!({"code": 230001, "msg": "bad card"})),
            _ => Answer::json(200, json!({"code": 0})),
        })
    });
    let tasks = FakeTasks::new();
    tasks.script.lock().unwrap().chunks = (0..20).map(|index| format!("chunk {index} ")).collect();
    let bots = bots_with(dir.path(), &server.origin, tasks.clone());
    *bots.card_interval_ms.lock().unwrap() = 0;
    *bots.card_backoff_ms.lock().unwrap() = 1;
    bots.save_bot(&json!({"bot": bound_bot("fs-1", "feishu", "ou_user", json!({"feishuAppId": "cli_0123456789abcdef"})),
        "credentialValue": "app-secret"}))
        .await
        .unwrap();
    let message = Inbound {
        bot_id: "fs-1".into(),
        text: "go".into(),
        actor: Actor {
            provider: "feishu".into(),
            bot_id: "fs-1".into(),
            provider_user_id: "ou_user".into(),
            chat_type: "private".into(),
            ..Actor::default()
        },
        ..Inbound::default()
    };
    bots.handle_inbound(&message).await.unwrap();
    eventually("task finished", || {
        bots.running.lock().unwrap().is_empty().then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let patches = server
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.method == "PATCH")
        .count();
    assert_eq!(
        patches, 3,
        "three failures open the circuit; later updates (including the final one) are skipped"
    );
    let status = bots.runtime_status("fs-1").unwrap();
    assert!(
        status["deliveryError"]
            .as_str()
            .unwrap()
            .contains("code=230001"),
        "{status:?}"
    );
}

/// `/new` 继承配置、任务态 `/model`、`/think` 读选项时 Bot 自己发的 `session/resume` 与 Node
/// `buildSessionResumeParams` 一样带工具面参数（Off-Peak 只给本地 workspace，动态工作流不分）。
/// 建会话、首条消息前的 resume、setModel 收口与设思考档位走 automation driver，已带同一组参数。
#[tokio::test]
async fn bot_initiated_resume_carries_session_tool_flags() {
    use crate::agent::{AgentRuntime, AgentRuntimeConfig, WorkspaceTarget};

    let dir = tempfile::tempdir().unwrap();
    let runtime = AgentRuntime::new(
        AgentRuntimeConfig {
            base_env: Default::default(),
            command_vars: Default::default(),
            fallback_cwd: None,
        },
        crate::settings::SettingStore::new(&zcode_host_base::testing::spawner(), dir.path()),
        crate::providers::ProviderRuntime::new(None, dir.path().join("provider_config.json")),
    );
    let local = WorkspaceTarget::local("/tmp/ws-bot".to_owned());
    // 门禁都关时不写字段。
    assert_eq!(
        super::resume_params(&runtime, &local, "t1").await,
        json!({"sessionId": "t1", "workspace": local.workspace_ref()})
    );
    runtime.extensions().enable_off_peak_tools();
    runtime
        .extensions()
        .set_dynamic_workflow_gate(Arc::new(|| Box::pin(async { Ok(true) })));
    assert_eq!(
        super::resume_params(&runtime, &local, "t1").await,
        json!({"sessionId": "t1", "workspace": local.workspace_ref(),
               "offPeakToolEnabled": true, "dynamicWorkflowEnabled": true})
    );
    let remote = WorkspaceTarget {
        workspace_path: "/repo".into(),
        workspace_identity: Some("remote:ssh:h:22:u:/repo".into()),
        remote_session_id: None,
    };
    assert_eq!(
        super::resume_params(&runtime, &remote, "t2").await,
        json!({"sessionId": "t2", "workspace": remote.workspace_ref(),
               "dynamicWorkflowEnabled": true})
    );
}

/// 可编排的远端运行时：`ensure_connected` 等闸门放行后才成功，并把上下文标成已连接。
struct ScriptedRemote {
    tasks: Arc<FakeTasks>,
    connected: Arc<std::sync::atomic::AtomicBool>,
    ensures: Arc<std::sync::atomic::AtomicUsize>,
    gate: Arc<tokio::sync::Semaphore>,
}

impl super::RemoteBots for ScriptedRemote {
    fn available(&self) -> bool {
        true
    }

    fn is_connected(
        &self,
        _: crate::agent::WorkspaceTarget,
    ) -> futures_util::future::BoxFuture<'static, bool> {
        let connected = self.connected.load(std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move { connected })
    }

    fn ensure_connected(
        &self,
        _: crate::agent::WorkspaceTarget,
    ) -> futures_util::future::BoxFuture<'static, Result<(), String>> {
        let (connected, ensures, gate) = (
            self.connected.clone(),
            self.ensures.clone(),
            self.gate.clone(),
        );
        Box::pin(async move {
            ensures.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _permit = gate.acquire().await.unwrap();
            connected.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
    }

    fn tasks(&self, _: &crate::agent::WorkspaceTarget) -> Option<Arc<dyn super::TaskPort>> {
        Some(self.tasks.clone())
    }
}

fn reconnect_message(message_id: &str) -> Inbound {
    let mut message = inbound("/reconnect");
    message.actor.provider_message_id = Some(message_id.into());
    message
}

#[tokio::test]
async fn reconnect_is_idempotent_per_delivery_and_per_chat_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join(".zcode/v2");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("setting.json"),
        json!({"lastWorkspaceSession": [{"kind": "remote", "workspacePath": "/srv/app",
            "workspaceIdentity": "remote:docker:c1:/srv/app",
            "target": {"kind": "docker", "container": "c1"},
            "lastOpenedAt": 1, "lastConnectionStatus": "connected"}]})
        .to_string(),
    )
    .unwrap();
    let remote = Arc::new(ScriptedRemote {
        tasks: FakeTasks::new(),
        connected: Arc::default(),
        ensures: Arc::default(),
        gate: Arc::new(tokio::sync::Semaphore::new(0)),
    });
    let bots = super::test_support::bots_with_remote(
        dir.path(),
        "http://127.0.0.1:9",
        remote.tasks.clone(),
        remote.clone(),
        false,
    );
    bots.save_bot(&json!({"bot": bound_bot("tg-1", "telegram", "42", json!({}))}))
        .await
        .unwrap();
    let ensures = || remote.ensures.load(std::sync::atomic::Ordering::SeqCst);

    // 第一条 /reconnect 卡在重连里；同一聊天的后续消息按聊天串行排在它后面。
    let spawn = |message_id: &'static str| {
        let bots = bots.clone();
        tokio::spawn(async move { bots.handle_inbound(&reconnect_message(message_id)).await })
    };
    // 被取消的重连（例如 Host 退出时丢弃了入站处理）不能留下永远“在途”的登记。
    let cancelled = spawn("m0");
    eventually("reconnect started", || (ensures() == 1).then_some(())).await;
    cancelled.abort();
    let _ = cancelled.await;
    let first = spawn("m1");
    eventually("reconnect restarted", || (ensures() == 2).then_some(())).await;
    let replayed = spawn("m1");
    let again = spawn("m2");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!replayed.is_finished() && !again.is_finished());
    remote.gate.add_permits(1);
    let replies = first.await.unwrap().unwrap();
    assert!(
        replies[0]["text"]
            .as_str()
            .unwrap()
            .starts_with("工作区: app\n"),
        "status after reconnect: {replies:?}"
    );
    // 同一条 provider 消息重投：不执行、不回复。
    assert!(replayed.await.unwrap().unwrap().is_empty());
    // 同一聊天与 workspace 刚重连完（3 s 内）：不执行、不回复。
    assert!(again.await.unwrap().unwrap().is_empty());
    assert_eq!(ensures(), 2, "one real reconnect after the cancelled one");
}
