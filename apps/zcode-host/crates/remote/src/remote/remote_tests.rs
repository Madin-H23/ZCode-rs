//! connection scope 与 window-controller 远端 source 的行为（rust-host-remote.md §2.5、§2.7），
//! 远端由进程内的 [`FakeRemote`] 扮演。
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use zcode_host_control::{ClientMode, ControlRouter, ControlSink, HostToMain};
use zcode_host_kit::{CallContext, Reply, RpcError, RpcValue, ServiceChannel};

use super::agent_scope::RemoteAgentChannel;
use super::fake_remote::{FakeRemote, FakeTransport, unavailable};
use super::provisioning::ProvisioningSource;
use super::{RemoteConfig, install_with};
use crate::agent::WorkspaceTarget;
use crate::tasks::controller::Controller;
use crate::tasks::{Partitions, TaskSource, WorkspaceEvents};

fn context(connection: u64) -> CallContext {
    CallContext {
        connection,
        attachment_id: format!("a{connection}"),
        client_mode: ClientMode::DesktopContinuous,
    }
}

async fn call(
    channel: &dyn ServiceChannel,
    context: &CallContext,
    method: &str,
    params: Value,
) -> Result<Reply, RpcError> {
    channel
        .call(context, method, vec![RpcValue::Json(params)])
        .await
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

async fn next<T>(stream: &mut (impl Stream<Item = T> + Unpin)) -> T {
    tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("event within 2s")
        .expect("stream open")
}

async fn quiet<T: std::fmt::Debug>(stream: &mut (impl Stream<Item = T> + Unpin)) {
    if let Ok(Some(item)) = tokio::time::timeout(Duration::from_millis(80), stream.next()).await {
        panic!("unexpected event {item:?}");
    }
}

/// 读掉已到达的事件，直到 80 ms 内没有新事件。
async fn drain<T>(stream: &mut (impl Stream<Item = T> + Unpin)) {
    while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(80), stream.next()).await {}
}

async fn settle() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
}

fn frame(subscription: &str, seq: u64) -> Value {
    json!({"topic": "conversation/s1", "subscriptionId": subscription, "seq": seq})
}

#[tokio::test]
async fn agent_scope_stages_routes_and_cleans_up_ownership() {
    let (remote, client) = FakeRemote::start();
    remote.on("zcode-agent.subscribeConversationV4", |remote, _| {
        // 初始帧先于订阅响应到达：只有 ACK 的 subscriptionId 会被放行。
        remote.fire("zcode-agent.onDynamicConversationFrame", frame("sub-1", 1));
        remote.fire(
            "zcode-agent.onDynamicConversationFrame",
            frame("foreign", 1),
        );
        Ok(json!({"ack": {"subscriptionId": "sub-1"}}))
    });
    let channel = RemoteAgentChannel::new(client);
    let desktop = context(1);
    let target = json!({"workspacePath": "/w", "workspaceIdentity": "remote:docker:c:/w", "sessionId": "s1"});

    let early = call(
        &*channel,
        &desktop,
        "subscribeConversationV4",
        target.clone(),
    )
    .await
    .unwrap_err();
    assert_eq!(early.message, "fault.connection.handshakeRequired");
    let hello = reply_value(
        call(&*channel, &desktop, "helloConversationV4", Value::Null)
            .await
            .unwrap(),
    );
    let connection_id = hello["connectionId"].as_str().unwrap().to_owned();
    assert!(connection_id.starts_with("host-rpc-"));
    call(
        &*channel,
        &desktop,
        "initializeConversationV4",
        json!({"kind": "clientHello", "protocolVersion": 3, "clientId": "c1", "appVersion": "t",
               "capabilities": {"workflowRunDeltas": true}}),
    )
    .await
    .unwrap();

    let mut frames = channel
        .listen(
            &desktop,
            "onDynamicConversationFrame",
            RpcValue::Json(target.clone()),
        )
        .unwrap();
    settle().await;
    let mut forged = target.clone();
    forged["connectionId"] = json!("forged");
    forged["clientMode"] = json!("web-remote-replayable");
    let ack = reply_value(
        call(&*channel, &desktop, "subscribeConversationV4", forged)
            .await
            .unwrap(),
    );
    assert_eq!(ack["ack"]["subscriptionId"], "sub-1");
    assert_eq!(next(&mut frames).await, Reply::Value(frame("sub-1", 1)));
    quiet(&mut frames).await;
    let forwarded = &remote.calls("zcode-agent.subscribeConversationV4")[0];
    assert!(forwarded.get("connectionId").is_none() && forwarded.get("clientMode").is_none());
    assert_eq!(
        forwarded["__zcodeTrustedV4Connection"],
        json!({"connectionId": connection_id, "clientMode": "desktop-continuous", "workflowRunDeltas": true})
    );

    remote.fire("zcode-agent.onDynamicConversationFrame", frame("sub-1", 2));
    remote.fire(
        "zcode-agent.onDynamicConversationFrame",
        frame("foreign", 2),
    );
    assert_eq!(next(&mut frames).await, Reply::Value(frame("sub-1", 2)));
    quiet(&mut frames).await;

    // 命令的 clientId 必须与握手一致；resync 只认本连接拥有的订阅，并带精确路由。
    let mismatch = call(
        &*channel,
        &desktop,
        "sendConversationCommandV4",
        json!({"workspacePath": "/w", "envelope": {"clientId": "other"}}),
    )
    .await
    .unwrap_err();
    assert_eq!(mismatch.message, "fault.command.clientMismatch");
    let not_owned = call(
        &*channel,
        &desktop,
        "resyncConversationV4",
        json!({"workspacePath": "/w", "workspaceIdentity": "remote:docker:c:/w", "subscriptionId": "nope"}),
    )
    .await
    .unwrap_err();
    assert_eq!(not_owned.message, "fault.subscription.notOwned");
    call(
        &*channel,
        &desktop,
        "resyncConversationV4",
        json!({"workspacePath": "/w", "workspaceIdentity": "remote:docker:c:/w", "subscriptionId": "sub-1"}),
    )
    .await
    .unwrap();
    assert_eq!(
        remote.calls("zcode-agent.resyncConversationV4")[0]["__zcodeTrustedV4UnsubscribeRoute"],
        json!({"topic": "conversation/s1", "connectionId": connection_id})
    );

    // 远端 runtime unavailable：该 workspace 的所有权失效，旧帧不再放行。
    remote.fire(
        "zcode-agent.onAgentRuntimeLifecycle",
        json!({"state": "unavailable", "workspaceKey": "remote:docker:c:/w", "runtimeIdentity": {"generation": 2}}),
    );
    settle().await;
    remote.fire("zcode-agent.onDynamicConversationFrame", frame("sub-1", 3));
    quiet(&mut frames).await;

    // 再订阅一次，然后关闭连接：先发 closed 流控，再反向退订。
    call(
        &*channel,
        &desktop,
        "subscribeConversationV4",
        target.clone(),
    )
    .await
    .unwrap();
    let _ = next(&mut frames).await;
    channel.connection_closed(&desktop);
    settle().await;
    let flow = remote.calls("zcode-agent.setConnectionFlowStateV4");
    assert_eq!(flow.len(), 1);
    assert_eq!(flow[0]["state"], "closed");
    assert_eq!(
        flow[0]["__zcodeTrustedV4Connection"]["connectionId"],
        connection_id.as_str()
    );
    let unsubscribed = remote.calls("zcode-agent.unsubscribeConversationV4");
    assert_eq!(unsubscribed.len(), 1);
    assert_eq!(unsubscribed[0]["subscriptionId"], "sub-1");
    assert_eq!(
        unsubscribed[0]["__zcodeTrustedV4UnsubscribeRoute"]["connectionId"],
        connection_id.as_str()
    );
    // 其余成员原样透传。
    let other = context(2);
    remote.on("zcode-agent.getAgentRuntimeStatus", |_, params| {
        Ok(json!({"echo": params}))
    });
    let echoed = reply_value(
        call(&*channel, &other, "getAgentRuntimeStatus", json!({"x": 1}))
            .await
            .unwrap(),
    );
    assert_eq!(echoed, json!({"echo": {"x": 1}}));
}

struct Control(mpsc::UnboundedSender<Value>);

impl ControlSink for Control {
    fn send(&self, message: HostToMain) {
        let _ = self.0.send(serde_json::to_value(&message).unwrap());
    }
}

/// 本地 tasks-index：记录被问到的 workspace，远端 identity 绝不能出现。
struct LocalTasks(Mutex<Vec<WorkspaceTarget>>);

impl TaskSource for LocalTasks {
    fn partitions(
        &self,
        target: WorkspaceTarget,
    ) -> BoxFuture<'static, Result<Partitions, RpcError>> {
        self.0.lock().unwrap().push(target);
        Box::pin(async { Ok(Partitions::default()) })
    }

    fn mutate(
        &self,
        _: WorkspaceTarget,
        _: String,
        _: Value,
    ) -> BoxFuture<'static, Result<(), RpcError>> {
        Box::pin(async { Ok(()) })
    }
}

fn remote_tasks(remote: &FakeRemote, pinned: Arc<Mutex<bool>>) {
    let meta = json!({"taskId": "t1", "title": "Remote task", "workspacePath": "/w",
                      "workspaceIdentity": "remote:docker:c:/w", "traceId": "r", "createdAt": 1, "updatedAt": 5});
    let (active, pinned_list) = (pinned.clone(), pinned.clone());
    let (active_meta, pinned_meta) = (meta.clone(), meta);
    remote.on("zcode-task.listTasks", move |_, _| {
        Ok(if *active.lock().unwrap() {
            json!([])
        } else {
            json!([active_meta])
        })
    });
    remote.on("zcode-task.listPinnedTasks", move |_, _| {
        Ok(if *pinned_list.lock().unwrap() {
            json!([pinned_meta])
        } else {
            json!([])
        })
    });
    remote.on("zcode-task.listArchivedTasks", |_, _| Ok(json!([])));
    let toggle = pinned;
    remote.on("zcode-task.setTaskPinned", move |_, params| {
        *toggle.lock().unwrap() = params["pinned"] == true;
        Ok(Value::Null)
    });
    remote.on("zcode-agent.subscribeSessionsIndexV4", |_, _| {
        Err(unavailable())
    });
}

async fn expect_message(messages: &mut mpsc::UnboundedReceiver<Value>, kind: &str) -> Value {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(2), messages.recv())
            .await
            .unwrap_or_else(|_| panic!("no {kind}"))
            .unwrap();
        if message["type"] == kind {
            return message;
        }
    }
}

#[tokio::test]
async fn controller_projects_remote_sources_across_disconnect_and_reconnect() {
    let (first, first_client) = FakeRemote::start();
    let (second, second_client) = FakeRemote::start();
    remote_tasks(&first, Arc::new(Mutex::new(false)));
    remote_tasks(&second, Arc::new(Mutex::new(false)));
    // FakeTransport 从尾部交出连接：先连 first，重连拿到 second。
    let transport = FakeTransport(Mutex::new(vec![
        second.connection(second_client),
        first.connection(first_client),
    ]));
    let home = tempfile::tempdir().unwrap();
    let (sink, mut messages) = mpsc::unbounded_channel();
    let mut router = ControlRouter::default();
    let workspaces = install_with(
        RemoteConfig {
            transport: Arc::new(transport),
            settings: crate::settings::SettingStore::new(home.path()),
            control: Arc::new(Control(sink)),
            provisioning: ProvisioningSource::for_test(home.path()),
            media_range_enabled: true,
            wsl_idle_ttl: Duration::from_secs(60),
            realtime: None,
        },
        &mut router,
    );
    let runtime = crate::agent::AgentRuntime::new(
        crate::agent::AgentRuntimeConfig {
            base_env: Default::default(),
            command_vars: Default::default(),
            fallback_cwd: None,
        },
        crate::settings::SettingStore::new(home.path()),
        crate::providers::ProviderRuntime::new(None, home.path().join("pc.json")),
    );
    let local = Arc::new(LocalTasks(Mutex::new(Vec::new())));
    let controller = Controller::start(
        local.clone(),
        crate::agent::sessions_index::SessionsIndexObserver::start(runtime),
        WorkspaceEvents::new(),
    );
    workspaces.sources().attach_to(&controller);
    let service = crate::tasks::controller::service(controller.clone()).build(None);
    let ui = context(9);
    let mut frames = service
        .listen(&ui, "onDynamicControllerFrame", RpcValue::Undefined)
        .unwrap();
    call(
        &*service,
        &ui,
        "subscribeControllerV4",
        json!({"topic": "controller/tasks-index"}),
    )
    .await
    .unwrap();
    call(
        &*service,
        &ui,
        "subscribeControllerV4",
        json!({"topic": "controller/workspaces"}),
    )
    .await
    .unwrap();
    let _ = next(&mut frames).await;
    let _ = next(&mut frames).await;

    let identity = "remote:docker:c:/w";
    let connect = |request_id: &str| {
        json!({"type": "connect-remote-workspace", "requestId": request_id,
               "target": {"kind": "docker", "container": "c"}, "remoteAssets": {}, "workspacePath": "/w"})
    };
    assert!(router.dispatch("connect-remote-workspace", connect("r1")));
    let connected = expect_message(&mut messages, "remote-workspace-connected").await;
    let first_session = connected["descriptor"]["remoteSessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let query = json!({"kind": "timeline", "sortBy": "updated",
                       "workspaceScopes": [{"workspacePath": "/w", "workspaceIdentity": identity}]});
    let list = reply_value(
        call(&*service, &ui, "listTaskList", query.clone())
            .await
            .unwrap(),
    );
    assert_eq!(list["total"], 1);
    assert_eq!(list["items"][0]["remoteSessionId"], first_session.as_str());
    assert_eq!(list["items"][0]["sourceAvailability"], "online");
    assert_eq!(list["items"][0]["liveStatus"], "idle");
    let task_frame = reply_value(next(&mut frames).await);
    assert_eq!(task_frame["payload"]["deltas"][0]["op"], "task.upserted");
    assert_eq!(
        task_frame["payload"]["deltas"][0]["task"]["address"],
        json!({"remoteSessionId": first_session, "workspacePath": "/w", "workspaceIdentity": identity, "taskId": "t1"})
    );
    let workspace_frame = reply_value(next(&mut frames).await);
    assert_eq!(workspace_frame["topic"], "controller/workspaces");
    assert_eq!(
        workspace_frame["payload"]["deltas"][0]["workspace"]["sourceAvailability"],
        "online"
    );
    assert!(
        local.0.lock().unwrap().is_empty(),
        "remote scopes never read the local tasks-index"
    );

    // 列表写操作经投影写远端，随后强制重读。
    let address = json!({"remoteSessionId": first_session, "workspacePath": "/w", "workspaceIdentity": identity, "taskId": "t1"});
    let meta = reply_value(
        call(
            &*service,
            &ui,
            "mutateTask",
            json!({"address": address, "mutation": {"kind": "pin", "pinned": true}}),
        )
        .await
        .unwrap(),
    );
    assert_eq!(meta["taskId"], "t1");
    assert_eq!(first.calls("zcode-task.setTaskPinned")[0]["pinned"], true);
    let pinned = reply_value(
        call(
            &*service,
            &ui,
            "listTaskList",
            json!({"kind": "pinned", "sortBy": "updated",
             "workspaceScopes": [{"workspacePath": "/w", "workspaceIdentity": identity}]}),
        )
        .await
        .unwrap(),
    );
    assert_eq!(pinned["total"], 1);
    drain(&mut frames).await;

    // 断线：行改为 offline，写操作被拒；Main 收到 connection-closed。
    first.close(255);
    let closed = expect_message(&mut messages, "remote-workspace-closed").await;
    assert_eq!(closed["reason"], "connection-closed");
    assert_eq!(closed["exitCode"], 255);
    settle().await;
    let offline = reply_value(
        call(
            &*service,
            &ui,
            "listTaskList",
            json!({"kind": "pinned", "sortBy": "updated",
             "workspaceScopes": [{"workspacePath": "/w", "workspaceIdentity": identity}]}),
        )
        .await
        .unwrap(),
    );
    assert_eq!(offline["items"][0]["sourceAvailability"], "offline");
    let rejected = call(
        &*service,
        &ui,
        "mutateTask",
        json!({"address": address, "mutation": {"kind": "mark-read"}}),
    )
    .await
    .unwrap_err();
    assert_eq!(rejected.message, "远程 source 当前离线，禁止列表写操作");

    // 重连：新 session 原子替换离线投影，旧 session 以 disposed 收口。
    assert!(router.dispatch("connect-remote-workspace", connect("r2")));
    let replaced = expect_message(&mut messages, "remote-workspace-closed").await;
    assert_eq!(replaced["remoteSessionId"], first_session.as_str());
    assert_eq!(replaced["reason"], "disposed");
    let reconnected = expect_message(&mut messages, "remote-workspace-connected").await;
    let second_session = reconnected["descriptor"]["remoteSessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let list = reply_value(
        call(&*service, &ui, "listTaskList", query.clone())
            .await
            .unwrap(),
    );
    assert_eq!(list["total"], 1);
    assert_eq!(list["items"][0]["remoteSessionId"], second_session.as_str());
    assert_eq!(list["items"][0]["sourceAvailability"], "online");

    // 释放：行与 workspace 一起移除；之后同 identity 的查询直接跳过，仍不读本地。
    assert!(router.dispatch(
        "dispose-remote-workspace-session",
        json!({"type": "dispose-remote-workspace-session", "requestId": "d", "remoteSessionId": second_session}),
    ));
    expect_message(&mut messages, "remote-workspace-closed").await;
    let empty = reply_value(call(&*service, &ui, "listTaskList", query).await.unwrap());
    assert_eq!(empty["total"], 0);
    assert!(local.0.lock().unwrap().is_empty());
}

fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 远端 CLI / MCP 的资源事实经 Host 转给 Main（runtimeSurface remote，按环境身份分组）。
#[tokio::test]
async fn remote_resource_facts_reach_main() {
    let (remote, client) = FakeRemote::start();
    let transport = FakeTransport(Mutex::new(vec![remote.connection(client)]));
    let home = tempfile::tempdir().unwrap();
    let (sink, mut messages) = mpsc::unbounded_channel();
    let mut router = ControlRouter::default();
    let _workspaces = install_with(
        RemoteConfig {
            transport: Arc::new(transport),
            settings: crate::settings::SettingStore::new(home.path()),
            control: Arc::new(Control(sink)),
            provisioning: ProvisioningSource::for_test(home.path()),
            media_range_enabled: true,
            wsl_idle_ttl: Duration::from_secs(60),
            realtime: None,
        },
        &mut router,
    );
    assert!(router.dispatch(
        "connect-remote-workspace",
        json!({"type": "connect-remote-workspace", "requestId": "r1",
               "target": {"kind": "docker", "container": "c"}, "remoteAssets": {}, "workspacePath": "/w"}),
    ));
    expect_message(&mut messages, "remote-workspace-connected").await;
    settle().await;
    let environment_key = sha256_hex("docker:c");
    remote.fire(
        "zcode-agent.onDynamicProcessResourceSample",
        json!({"cpuPercent": 3}),
    );
    let sample = expect_message(&mut messages, "agent-resource-sample").await;
    assert_eq!(
        sample,
        json!({"type": "agent-resource-sample", "runtimeSurface": "remote",
               "environmentKey": environment_key, "sample": {"cpuPercent": 3}})
    );
    remote.fire(
        "zcode-agent.onDynamicMcpTelemetry",
        json!({"kind": "spawn"}),
    );
    assert_eq!(
        expect_message(&mut messages, "mcp-telemetry").await,
        json!({"type": "mcp-telemetry", "runtimeSurface": "remote", "event": {"kind": "spawn"}})
    );
    remote.fire(
        "zcode-agent.onDynamicMcpResourceSamples",
        json!([{"pid": 1}]),
    );
    assert_eq!(
        expect_message(&mut messages, "mcp-resource-samples").await,
        json!({"type": "mcp-resource-samples", "runtimeSurface": "remote",
               "environmentKey": environment_key, "samples": [{"pid": 1}]})
    );
    remote.fire("zcode-agent.onDynamicToolExecResource", json!({"ms": 9}));
    assert_eq!(
        expect_message(&mut messages, "tool-exec-resource").await,
        json!({"type": "tool-exec-resource", "runtimeSurface": "remote", "sample": {"ms": 9}})
    );
}

/// 远端 attachment 上的本机服务与 Node 的远端集合一致：App 用量读远端 Agent，分享导入不可用。
#[tokio::test]
async fn remote_attachments_route_usage_and_share_like_node() {
    use zcode_host_kit::{ChannelRegistry, ServiceBuilder};
    let (remote, client) = FakeRemote::start();
    remote.on("zcode-agent.getAppUsageStats", |_, params| {
        Ok(json!({"source": "remote", "params": params}))
    });
    let transport = FakeTransport(Mutex::new(vec![remote.connection(client)]));
    let home = tempfile::tempdir().unwrap();
    let (sink, mut messages) = mpsc::unbounded_channel();
    let mut router = ControlRouter::default();
    let workspaces = install_with(
        RemoteConfig {
            transport: Arc::new(transport),
            settings: crate::settings::SettingStore::new(home.path()),
            control: Arc::new(Control(sink)),
            provisioning: ProvisioningSource::for_test(home.path()),
            media_range_enabled: true,
            wsl_idle_ttl: Duration::from_secs(60),
            realtime: None,
        },
        &mut router,
    );
    assert!(router.dispatch(
        "connect-remote-workspace",
        json!({"type": "connect-remote-workspace", "requestId": "r1",
               "target": {"kind": "docker", "container": "c"}, "remoteAssets": {}, "workspacePath": "/w"}),
    ));
    let connected = expect_message(&mut messages, "remote-workspace-connected").await;
    let scope = super::RemoteScope {
        remote_session_id: connected["descriptor"]["remoteSessionId"]
            .as_str()
            .unwrap()
            .into(),
        workspace_path: "/w".into(),
        workspace_identity: "remote:docker:c:/w".into(),
    };
    let marker = |name: &'static str| {
        move |_: CallContext, _: zcode_host_kit::Args| async move {
            Ok(Reply::Value(json!({"source": "local", "method": name})))
        }
    };
    let mut local = ChannelRegistry::empty();
    local.implement(
        ServiceBuilder::new("usage-stats")
            .method("getAppUsageSnapshot", marker("getAppUsageSnapshot"))
            .method(
                "getCodingPlanUsageSnapshot",
                marker("getCodingPlanUsageSnapshot"),
            ),
    );
    local.implement(
        ServiceBuilder::new("conversation-share")
            .method("importShare", marker("importShare"))
            .method("getPreview", marker("getPreview")),
    );
    let desktop = context(1);
    let attachment = workspaces
        .attach("a1", ClientMode::DesktopContinuous, &scope, &local)
        .await
        .unwrap();
    let usage = attachment.registry.get("usage-stats").unwrap();
    let app = reply_value(
        call(
            &*usage,
            &desktop,
            "getAppUsageSnapshot",
            json!({"range": "7d"}),
        )
        .await
        .unwrap(),
    );
    assert_eq!(app, json!({"source": "remote", "params": {"range": "7d"}}));
    let plan = reply_value(
        call(&*usage, &desktop, "getCodingPlanUsageSnapshot", json!({}))
            .await
            .unwrap(),
    );
    assert_eq!(plan["source"], "local");

    let share = attachment.registry.get("conversation-share").unwrap();
    let import = call(&*share, &desktop, "importShare", json!({"shareCode": "x"}))
        .await
        .unwrap_err();
    assert_eq!(import.message, "Conversation share import is unavailable");
    assert_eq!(import.field("kind"), Some(&json!("feature_disabled")));
    let preview = reply_value(
        call(&*share, &desktop, "getPreview", json!({}))
            .await
            .unwrap(),
    );
    assert_eq!(preview["source"], "local");
    // replayable 连接交给本机实现（它先按 desktop-only 拒绝写操作）。
    let replay = CallContext {
        client_mode: ClientMode::WebRemoteReplayable,
        ..context(2)
    };
    let delegated = reply_value(
        call(&*share, &replay, "importShare", json!({}))
            .await
            .unwrap(),
    );
    assert_eq!(delegated["method"], "importShare");
}
