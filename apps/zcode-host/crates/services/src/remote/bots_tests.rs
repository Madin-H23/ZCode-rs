//! Bot 远端运行时（rust-host-remote.md §2.9）：Main 往返、按 session 解析连接、初始化与偏好推送、
//! realtime 镜像与远端 attachment 的 bots channel。远端由进程内的 [`FakeRemote`] 扮演。
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;
use zcode_host_control::{ClientMode, ControlRouter, ControlSink, HostToMain};
use zcode_host_kit::{CallContext, ChannelRegistry, Reply, RpcValue};

use super::fake_remote::{FakeRemote, FakeTransport};
use super::provisioning::ProvisioningSource;
use super::transport::Connection;
use super::{RemoteConfig, RemoteScope, RemoteWorkspaces, install_with};
use crate::agent::WorkspaceTarget;
use crate::bots::RemoteBots;
use crate::main_bridge::MainBridge;
use crate::tasks::realtime::TaskRealtimeBridge;

const IDENTITY: &str = "remote:docker:c:/w";

struct Control(mpsc::UnboundedSender<Value>);

impl ControlSink for Control {
    fn send(&self, message: HostToMain) {
        let _ = self.0.send(serde_json::to_value(&message).unwrap());
    }
}

struct Setup {
    workspaces: Arc<RemoteWorkspaces>,
    router: ControlRouter,
    messages: mpsc::UnboundedReceiver<Value>,
}

fn setup(home: &std::path::Path, connections: Vec<Connection>, realtime: bool) -> Setup {
    let (sink, messages) = mpsc::unbounded_channel();
    let control: Arc<dyn ControlSink> = Arc::new(Control(sink));
    let mut router = ControlRouter::default();
    let realtime = realtime.then(|| {
        let bridge =
            TaskRealtimeBridge::for_host_init(Some("h1"), None, MainBridge::new(control.clone()))
                .unwrap();
        bridge.register(&mut router);
        bridge
    });
    let workspaces = install_with(
        RemoteConfig {
            transport: Arc::new(FakeTransport(Mutex::new(connections))),
            settings: crate::settings::SettingStore::new(home),
            control,
            provisioning: ProvisioningSource::for_test(home),
            media_range_enabled: true,
            wsl_idle_ttl: Duration::from_secs(60),
            realtime,
        },
        &mut router,
    );
    Setup {
        workspaces,
        router,
        messages,
    }
}

impl Setup {
    async fn expect(&mut self, kind: &str) -> Value {
        loop {
            let message = tokio::time::timeout(Duration::from_secs(2), self.messages.recv())
                .await
                .unwrap_or_else(|_| panic!("no {kind} within 2s"))
                .unwrap();
            if message["type"] == kind {
                return message;
            }
        }
    }

    fn reply(&self, kind: &str, request: &Value, fields: Value) {
        let mut message = json!({"type": kind, "requestId": request["requestId"]});
        for (key, value) in fields.as_object().unwrap() {
            message[key] = value.clone();
        }
        assert!(self.router.dispatch(kind, message));
    }

    async fn connect(&mut self, request_id: &str) -> RemoteScope {
        assert!(self.router.dispatch(
            "connect-remote-workspace",
            json!({"type": "connect-remote-workspace", "requestId": request_id,
                   "target": {"kind": "docker", "container": "c"}, "remoteAssets": {},
                   "workspacePath": "/w"}),
        ));
        let connected = self.expect("remote-workspace-connected").await;
        assert_eq!(connected["descriptor"]["workspaceIdentity"], IDENTITY);
        RemoteScope {
            remote_session_id: connected["descriptor"]["remoteSessionId"]
                .as_str()
                .unwrap()
                .into(),
            workspace_path: "/w".into(),
            workspace_identity: IDENTITY.into(),
        }
    }
}

fn write_settings(home: &std::path::Path, settings: Value) {
    let dir = home.join(".zcode/v2");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("setting.json"), settings.to_string()).unwrap();
}

fn remote_target(path: &str, identity: &str) -> WorkspaceTarget {
    WorkspaceTarget {
        workspace_path: path.into(),
        workspace_identity: Some(identity.into()),
        remote_session_id: None,
    }
}

fn reply_value(reply: Reply) -> Value {
    match reply {
        Reply::Value(value) => value,
        Reply::Undefined => Value::Null,
        Reply::Encoded(body) => zcode_host_rpc::Reader::new(&body)
            .value()
            .unwrap()
            .into_json(),
        other => panic!("unexpected reply {other:?}"),
    }
}

fn same_port(
    left: &Arc<dyn crate::bots::TaskPort>,
    right: &Arc<dyn crate::bots::TaskPort>,
) -> bool {
    std::ptr::addr_eq(Arc::as_ptr(left), Arc::as_ptr(right))
}

#[tokio::test]
async fn bot_runtime_asks_main_for_status_and_reconnect() {
    let home = tempfile::tempdir().unwrap();
    write_settings(
        home.path(),
        json!({"lastWorkspaceSession": [
            {"kind": "local", "workspacePath": "/srv"},
            // UI 规范化过路径：按 identity 命中。
            {"kind": "remote", "workspacePath": "/real/srv", "workspaceIdentity": "remote:ssh:u@h:2222:/srv",
             "target": {"kind": "ssh", "host": "h", "port": 2222, "username": "u", "sshConfigAlias": "alias",
                        "assetInstallMode": "remote-download", "passwordCredentialKey": "ssh-pw",
                        "privateKeyPassphraseCredentialKey": "ssh-missing"},
             "lastOpenedAt": 1, "lastConnectionStatus": "connected"},
            {"kind": "remote", "workspacePath": "/w", "workspaceIdentity": IDENTITY,
             "target": {"kind": "docker", "container": "c"},
             "lastOpenedAt": 1, "lastConnectionStatus": "connected"},
        ]}),
    );
    let mut setup = setup(home.path(), Vec::new(), false);
    setup
        .workspaces
        .provisioning
        .credentials
        .save("ssh-pw", "s3cret")
        .await
        .unwrap();
    let runtime = setup.workspaces.bots_runtime();
    assert!(runtime.available());
    const STATUS: &str = "bot-remote-workspace-connection-status-request";
    const STATUS_RESULT: &str = "bot-remote-workspace-connection-status-result";

    // SSH：凭据键换成明文密码，展示用字段不进 Main 消息；没有存的口令不写。
    let ssh = remote_target("/srv", "remote:ssh:u@h:2222:/srv");
    let pending = tokio::spawn(runtime.is_connected(ssh.clone()));
    let request = setup.expect(STATUS).await;
    assert!(
        request["requestId"]
            .as_str()
            .unwrap()
            .starts_with("bot-status-")
    );
    assert_eq!(request["workspacePath"], "/srv");
    assert_eq!(request["workspaceIdentity"], "remote:ssh:u@h:2222:/srv");
    assert_eq!(
        request["target"],
        json!({"kind": "ssh", "host": "h", "port": 2222, "username": "u", "password": "s3cret"})
    );
    setup.reply(
        STATUS_RESULT,
        &request,
        json!({"ok": true, "connected": true}),
    );
    assert!(pending.await.unwrap());
    // Main 查询失败：用本地标记。
    let pending = tokio::spawn(runtime.is_connected(ssh.clone()));
    let request = setup.expect(STATUS).await;
    setup.reply(STATUS_RESULT, &request, json!({"ok": false, "error": "x"}));
    assert!(pending.await.unwrap());
    // Main 说未连接：清掉标记，之后查询失败也视为未连接。
    let pending = tokio::spawn(runtime.is_connected(ssh.clone()));
    let request = setup.expect(STATUS).await;
    setup.reply(
        STATUS_RESULT,
        &request,
        json!({"ok": true, "connected": false}),
    );
    assert!(!pending.await.unwrap());
    let pending = tokio::spawn(runtime.is_connected(ssh.clone()));
    let request = setup.expect(STATUS).await;
    setup.reply(STATUS_RESULT, &request, json!({"ok": false}));
    assert!(!pending.await.unwrap());

    // 没有连接信息：不问 Main。
    let unknown = remote_target("/x", "remote:docker:other:/x");
    assert!(!runtime.is_connected(unknown.clone()).await);
    assert_eq!(
        runtime.ensure_connected(unknown).await,
        Err("未找到该远端 workspace 的连接信息。".to_owned())
    );

    // 重连：Docker target 原样；失败带 Main 的错误，成功后登记本地标记。
    const RECONNECT: &str = "bot-remote-workspace-reconnect-request";
    const RECONNECT_RESULT: &str = "bot-remote-workspace-reconnect-result";
    let docker = remote_target("/w", IDENTITY);
    let pending = tokio::spawn(runtime.ensure_connected(docker.clone()));
    let request = setup.expect(RECONNECT).await;
    assert!(
        request["requestId"]
            .as_str()
            .unwrap()
            .starts_with("bot-reconnect-")
    );
    assert_eq!(
        request["target"],
        json!({"kind": "docker", "container": "c"})
    );
    assert_eq!(request["workspaceIdentity"], IDENTITY);
    setup.reply(
        RECONNECT_RESULT,
        &request,
        json!({"ok": false, "error": "boom"}),
    );
    assert_eq!(pending.await.unwrap(), Err("boom".to_owned()));
    let pending = tokio::spawn(runtime.ensure_connected(docker.clone()));
    let request = setup.expect(RECONNECT).await;
    setup.reply(
        RECONNECT_RESULT,
        &request,
        json!({"ok": true, "sessionId": "s1"}),
    );
    assert_eq!(pending.await.unwrap(), Ok(()));
    let pending = tokio::spawn(runtime.is_connected(docker));
    let request = setup.expect(STATUS).await;
    setup.reply(STATUS_RESULT, &request, json!({"ok": false}));
    assert!(
        pending.await.unwrap(),
        "reconnect marked the workspace connected"
    );
}

fn serve_runtime(remote: &FakeRemote, model: &'static str) {
    remote.on("model-selection.getView", move |_, _| {
        Ok(json!({"preferredSelection": {"providerId": "p", "modelId": model}}))
    });
    remote.on("zcode-task.createTask", |_, params| {
        Ok(
            json!({"taskId": "t1", "workspacePath": params["workspacePath"],
                  "workspaceIdentity": params["workspaceIdentity"], "traceId": "r"}),
        )
    });
    remote.on("zcode-task.listTasks", |_, _| Ok(json!([{"taskId": "t1"}])));
}

#[tokio::test]
async fn bot_runtime_uses_the_online_session_and_initializes_per_connection() {
    let home = tempfile::tempdir().unwrap();
    write_settings(
        home.path(),
        json!({"askUserQuestionAutoResolutionEnabled": false}),
    );
    let (first, first_client) = FakeRemote::start();
    let (second, second_client) = FakeRemote::start();
    serve_runtime(&first, "m1");
    serve_runtime(&second, "m2");
    // FakeTransport 从尾部交出连接：先 first，重连拿到 second。
    let mut setup = setup(
        home.path(),
        vec![
            second.connection(second_client),
            first.connection(first_client),
        ],
        false,
    );
    let runtime = setup.workspaces.bots_runtime();
    let target = remote_target("/w", IDENTITY);
    assert!(runtime.tasks(&target).is_none(), "no session yet");
    setup.connect("r1").await;

    // 只按 (path, identity) 精确匹配在线 session，绝不按路径单独匹配。
    assert!(
        runtime
            .tasks(&WorkspaceTarget::local("/w".to_owned()))
            .is_none()
    );
    assert!(
        runtime
            .tasks(&remote_target("/w", "remote:docker:c:/other"))
            .is_none()
    );
    assert!(runtime.tasks(&remote_target("/other", IDENTITY)).is_none());
    let port = runtime.tasks(&target).unwrap();
    assert!(first.calls("model-selection.getView").is_empty(), "lazy");

    // 首次使用：先 getView，再推本机设置的偏好；之后不再初始化。
    let view = port.model_view(None).await;
    assert_eq!(view["preferredSelection"]["modelId"], "m1");
    assert_eq!(first.calls("model-selection.getView").len(), 2);
    assert_eq!(
        first.calls("zcode-agent.syncAppRuntimePreferences"),
        vec![
            json!({"askUserQuestionAutoResolutionEnabled": false, "modelIoFullRetentionEnabled": false})
        ]
    );
    let task = port
        .create(
            target.clone(),
            json!({"providerId": "p", "modelId": "m1"}),
            Some("yolo".into()),
        )
        .await
        .unwrap();
    assert_eq!(task, "t1");
    assert_eq!(
        first.calls("zcode-task.createTask"),
        vec![json!({"workspacePath": "/w", "workspaceIdentity": IDENTITY,
                    "modelSelection": {"providerId": "p", "modelId": "m1"},
                    "mode": "yolo", "v4Create": true})]
    );
    // 建会话的结果经 wrapper 登记 task meta（session route 同 Renderer 路径）。
    let route = setup.expect("session-route-announce").await;
    assert_eq!(route["route"]["sessionId"], "t1");
    assert!(same_port(&port, &runtime.tasks(&target).unwrap()));
    assert_eq!(
        port.list(IDENTITY.into()).await,
        vec![json!({"taskId": "t1"})]
    );
    assert_eq!(
        first.calls("zcode-task.listTasks"),
        vec![json!({"workspacePath": "/w", "workspaceIdentity": IDENTITY})]
    );
    assert_eq!(first.calls("model-selection.getView").len(), 2);

    // 偏好变更只推给已就绪的远端运行时。
    runtime
        .sync_app_runtime_preferences(json!({"askUserQuestionAutoResolutionEnabled": true}))
        .await;
    assert_eq!(
        first.calls("zcode-agent.syncAppRuntimePreferences").last(),
        Some(&json!({"askUserQuestionAutoResolutionEnabled": true}))
    );

    // 断线后不可用；重连是新连接：换新端口并重新初始化（带最新偏好）。
    first.close(-1);
    setup.expect("remote-workspace-closed").await;
    assert!(runtime.tasks(&target).is_none());
    setup.connect("r2").await;
    let fresh = runtime.tasks(&target).unwrap();
    assert!(!same_port(&port, &fresh));
    assert_eq!(
        fresh.model_view(None).await["preferredSelection"]["modelId"],
        "m2"
    );
    assert_eq!(
        second.calls("zcode-agent.syncAppRuntimePreferences"),
        vec![json!({"askUserQuestionAutoResolutionEnabled": true})]
    );
}

#[tokio::test]
async fn remote_send_prompt_holds_a_realtime_lease_and_mirrors_the_stream() {
    let home = tempfile::tempdir().unwrap();
    let (remote, client) = FakeRemote::start();
    remote.on("zcode-task.createTask", |_, _| {
        Ok(json!({"taskId": "t1", "workspacePath": "/w", "workspaceIdentity": IDENTITY, "traceId": "r"}))
    });
    // 远端在 ACK 之前推流事件（同一条连接按序到达）。
    remote.on("zcode-task.sendPrompt", |remote, _| {
        remote.fire(
            "zcode-task.onDynamicStreamEvent",
            json!({"type": "agent_message_chunk", "text": "hi"}),
        );
        remote.fire(
            "zcode-task.onDynamicStreamEvent",
            json!({"type": "task_snapshot_updated"}),
        );
        Ok(Value::Null)
    });
    let mut setup = setup(home.path(), vec![remote.connection(client)], true);
    let scope = setup.connect("r1").await;
    let attachment = setup
        .workspaces
        .attach(
            "a1",
            ClientMode::DesktopContinuous,
            &scope,
            &ChannelRegistry::empty(),
        )
        .await
        .unwrap();
    let tasks = attachment.registry.get("zcode-task").unwrap();
    let ui = CallContext {
        connection: 1,
        attachment_id: "a1".into(),
        client_mode: ClientMode::DesktopContinuous,
    };
    tasks
        .call(
            &ui,
            "createTask",
            vec![RpcValue::Json(
                json!({"workspacePath": "/w", "workspaceIdentity": IDENTITY}),
            )],
        )
        .await
        .unwrap();
    let send = |trace: &str| {
        let call = tasks.call(
            &ui,
            "sendPrompt",
            vec![RpcValue::Json(
                json!({"taskId": "t1", "traceId": trace, "content": "hello"}),
            )],
        );
        tokio::spawn(call)
    };

    let pending = send("tr1");
    let acquire = setup.expect("task-run-lease-acquire").await;
    let request = &acquire["request"];
    assert_eq!(request["workspacePath"], "/w");
    assert_eq!(request["workspaceIdentity"], IDENTITY);
    assert_eq!(request["workspaceKey"], IDENTITY);
    assert_eq!(request["taskId"], "t1");
    assert_eq!(request["runId"], "tr1");
    assert_eq!(request["traceId"], "tr1");
    assert!(
        remote.calls("zcode-task.sendPrompt").is_empty(),
        "waits for the lease"
    );
    assert!(setup.router.dispatch(
        "task-run-lease-result",
        json!({"type": "task-run-lease-result",
               "result": {"leaseRequestId": request["leaseRequestId"], "acquired": true}}),
    ));
    let mut ops = Vec::new();
    loop {
        let message = tokio::time::timeout(Duration::from_secs(2), setup.messages.recv())
            .await
            .unwrap()
            .unwrap();
        match message["type"].as_str() {
            Some("task-stream-op-publish") => ops.push(message["op"].clone()),
            Some("task-run-lease-release") => {
                assert_eq!(message["target"]["taskId"], "t1");
                break;
            }
            _ => {}
        }
    }
    assert!(pending.await.unwrap().is_ok());
    assert_eq!(ops.len(), 2, "{ops:?}");
    assert_eq!(ops[0]["kind"], "user_message");
    assert_eq!(ops[0]["messageId"], "user-tr1");
    assert_eq!(ops[0]["content"], "hello");
    assert!(ops[0]["timestamp"].is_u64());
    assert!(ops[0].get("attachments").is_none());
    assert_eq!(
        ops[1],
        json!({"kind": "stream_event", "event": {"type": "agent_message_chunk", "text": "hi"}})
    );

    // 没拿到 lease：直接发送，不镜像。
    let pending = send("tr2");
    let acquire = setup.expect("task-run-lease-acquire").await;
    assert!(setup.router.dispatch(
        "task-run-lease-result",
        json!({"type": "task-run-lease-result",
               "result": {"leaseRequestId": acquire["request"]["leaseRequestId"], "acquired": false}}),
    ));
    assert!(pending.await.unwrap().is_ok());
    assert_eq!(remote.calls("zcode-task.sendPrompt").len(), 2);
    while let Ok(message) = setup.messages.try_recv() {
        assert_ne!(message["type"], "task-stream-op-publish", "{message}");
        assert_ne!(message["type"], "task-run-lease-release", "{message}");
    }
}

#[tokio::test]
async fn remote_attachment_bots_channel_runs_on_the_connection() {
    let home = tempfile::tempdir().unwrap();
    let (remote, client) = FakeRemote::start();
    remote.on("zcode-task.listTasks", |_, _| Ok(json!([{"taskId": "t9"}])));
    let mut setup = setup(home.path(), vec![remote.connection(client)], false);
    let scope = setup.connect("r1").await;
    let local = crate::bots::test_support::bots_with(
        home.path(),
        "http://127.0.0.1:9",
        crate::bots::test_support::FakeTasks::new(),
    );
    setup.workspaces.bind_bots(local.clone());
    let mut locals = ChannelRegistry::empty();
    locals.implement(crate::bots::service::channel(local));
    let attachment = setup
        .workspaces
        .attach("a1", ClientMode::DesktopContinuous, &scope, &locals)
        .await
        .unwrap();
    let channel = attachment.registry.get("bots").unwrap();
    assert!(!Arc::ptr_eq(&channel, &locals.get("bots").unwrap()));
    let ui = CallContext {
        connection: 1,
        attachment_id: "a1".into(),
        client_mode: ClientMode::DesktopContinuous,
    };
    // 配置与本地实例同一份存储。
    let config = reply_value(channel.call(&ui, "getConfig", Vec::new()).await.unwrap());
    let local_config = reply_value(
        locals
            .get("bots")
            .unwrap()
            .call(&ui, "getConfig", Vec::new())
            .await
            .unwrap(),
    );
    assert_eq!(config, local_config);
    // 每条连接一个实例：任务能力是这条连接的远端 zcode-task，不起通道运行时。
    let handle = setup.workspaces.registry().resolve_scoped(&scope).unwrap();
    let bots = handle.bots.get().unwrap().clone();
    assert!(!bots.background);
    assert_eq!(
        bots.tasks.list("/w".into()).await,
        vec![json!({"taskId": "t9"})]
    );
    assert_eq!(
        remote.calls("zcode-task.listTasks"),
        vec![json!({"workspacePath": "/w"})]
    );
    setup
        .workspaces
        .attach("a2", ClientMode::WebRemoteReplayable, &scope, &locals)
        .await
        .unwrap();
    assert!(Arc::ptr_eq(handle.bots.get().unwrap(), &bots));
}
