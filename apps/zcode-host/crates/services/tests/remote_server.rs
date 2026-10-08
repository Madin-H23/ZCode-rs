//! 远端 workspace 端到端（docs/specs/rust-host-remote.md §7）：拉起本仓库构建的真实
//! `packages/server/dist/remote/zcode-server.cjs`（临时 HOME，经 `ProcessTransport` 直连 stdio），
//! 走 Main 的控制消息完成连接、Provider Provisioning、scoped attachment 的转发与 zcode-agent scope，
//! 再释放。所有出站 HTTP 被指向 127.0.0.1 上已关闭的端口，不访问任何真实服务。
//!
//! 构建产物：`pnpm --filter @zcode/server build:remote`。没有产物或找不到 node 时跳过并说明原因。
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;
use zcode_host_control::{ClientMode, ControlRouter, ControlSink, HostToMain};
use zcode_host_kit::{CallContext, ChannelRegistry, Reply, RpcValue};
use zcode_host_services::remote::provisioning::{CredentialCipher, ProvisioningSource};
use zcode_host_services::remote::transport::ProcessTransport;
use zcode_host_services::remote::{RemoteConfig, RemoteScope, install_with};
use zcode_host_services::settings::SettingStore;

struct Control(mpsc::UnboundedSender<Value>);

impl ControlSink for Control {
    fn send(&self, message: HostToMain) {
        let _ = self.0.send(serde_json::to_value(&message).unwrap());
    }
}

fn find_node() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("node"))
            .find(|candidate| candidate.is_file())
    })
}

fn server_bundle() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../packages/server/dist/remote/zcode-server.cjs")
}

async fn next_of(
    messages: &mut mpsc::UnboundedReceiver<Value>,
    kind: &str,
    logs: &Arc<Mutex<Vec<Value>>>,
) -> Value {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(30), messages.recv())
            .await
            .unwrap_or_else(|_| panic!("no {kind} within 30s; logs: {:?}", logs.lock().unwrap()))
            .expect("control channel open");
        if message["type"] == kind {
            return message;
        }
        logs.lock().unwrap().push(message);
    }
}

fn context(connection: u64, mode: ClientMode) -> CallContext {
    CallContext {
        connection,
        attachment_id: format!("attachment-{connection}"),
        client_mode: mode,
    }
}

async fn call(
    registry: &ChannelRegistry,
    context: &CallContext,
    channel: &str,
    method: &str,
    args: Vec<Value>,
) -> Result<Reply, zcode_host_kit::RpcError> {
    registry
        .get(channel)
        .unwrap_or_else(|| panic!("channel {channel} registered"))
        .call(
            context,
            method,
            args.into_iter().map(RpcValue::Json).collect(),
        )
        .await
}

fn value(reply: Reply) -> Value {
    match reply {
        Reply::Value(value) => value,
        other => panic!("expected a JSON reply, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_workspace_against_a_real_zcode_server() {
    let bundle = server_bundle();
    let (Some(node), true) = (find_node(), bundle.is_file()) else {
        eprintln!(
            "skipping: needs node on PATH and {} (pnpm --filter @zcode/server build:remote)",
            bundle.display()
        );
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let remote_home = temp.path().join("remote-home");
    let local_home = temp.path().join("local-home");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&remote_home).unwrap();
    std::fs::create_dir_all(local_home.join(".zcode/v2")).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("hello.txt"), "hello remote\n").unwrap();
    let workspace_path = workspace.canonicalize().unwrap().display().to_string();

    let dead = "http://127.0.0.1:9";
    let env: BTreeMap<String, String> = [
        ("HOME", remote_home.display().to_string()),
        (
            "PATH",
            format!("/usr/bin:/bin:{}", node.parent().unwrap().display()),
        ),
        ("NODE_USE_ENV_PROXY", "1".into()),
        ("HTTP_PROXY", dead.into()),
        ("HTTPS_PROXY", dead.into()),
        ("ZCODE_BASE_URL", dead.into()),
        ("ZCODE_ENDPOINT_ORIGIN", dead.into()),
        ("ZAI_OAUTH_ORIGIN", dead.into()),
        ("ZAI_BUSINESS_BASE_URL", dead.into()),
        ("ZCODE_TELEMETRY_REPORT_ENDPOINT", String::new()),
        (
            "ZCODE_SERVICE_AUTHORITY_MODE",
            "desktop-attached-remote".into(),
        ),
        (
            "ZCODE_SERVER_RUNTIME_ROOT",
            remote_home.join(".zcode/server").display().to_string(),
        ),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value))
    .collect();
    let transport = ProcessTransport {
        program: node,
        args: vec![bundle.display().to_string()],
        env,
        cwd: remote_home.clone(),
    };
    std::fs::write(
        local_home.join(".zcode/v2/credentials.json"),
        json!({"zcodejwttoken": "local-token"}).to_string(),
    )
    .unwrap();
    let (sink, mut messages) = mpsc::unbounded_channel();
    let mut router = ControlRouter::default();
    let remote = install_with(
        RemoteConfig {
            transport: Arc::new(transport),
            settings: SettingStore::new(&local_home),
            control: Arc::new(Control(sink)),
            provisioning: ProvisioningSource {
                personal_config_file: local_home.join(".zcode/v2/provider_config.json"),
                credential_file: local_home.join(".zcode/v2/credentials.json"),
                cipher: CredentialCipher::new(Some("test-secret"), &local_home, None),
            },
            media_range_enabled: true,
            wsl_idle_ttl: Duration::from_secs(60),
        },
        &mut router,
    );
    let logs = Arc::new(Mutex::new(Vec::new()));

    // Main：connect-remote-workspace → remote-workspace-connected{descriptor}
    let target = json!({"kind": "docker", "container": "local-test"});
    let identity = format!("remote:docker:local-test:{workspace_path}");
    assert!(router.dispatch(
        "connect-remote-workspace",
        json!({"type": "connect-remote-workspace", "requestId": "r1", "target": target,
               "remoteAssets": {}, "workspacePath": workspace_path}),
    ));
    let connected = next_of(&mut messages, "remote-workspace-connected", &logs).await;
    assert_eq!(connected["requestId"], "r1");
    let descriptor = &connected["descriptor"];
    assert_eq!(descriptor["workspaceIdentity"], identity);
    assert_eq!(descriptor["generation"], 1);
    let session_id = descriptor["remoteSessionId"].as_str().unwrap().to_owned();
    assert!(
        logs.lock()
            .unwrap()
            .iter()
            .any(|log| log["type"] == "remote-workspace-connection-log" && log["requestId"] == "r1"),
        "connection log reached Main"
    );

    // Main：environment-online 的 Provider Provisioning（Remote Workspace 发布屏障）。
    assert!(router.dispatch(
        "provider-provisioning-execute",
        json!({"type": "provider-provisioning-execute", "requestId": "p1",
               "environmentKey": "docker:local-test", "remoteSessionId": session_id,
               "trigger": "environment-online"}),
    ));
    let provisioned = next_of(
        &mut messages,
        "provider-provisioning-execution-result",
        &logs,
    )
    .await;
    assert!(
        matches!(
            provisioned["status"].as_str(),
            Some("applied" | "already-applied")
        ),
        "{provisioned}"
    );
    assert!(router.dispatch(
        "provider-provisioning-execute",
        json!({"type": "provider-provisioning-execute", "requestId": "p2",
               "environmentKey": "docker:other", "remoteSessionId": session_id,
               "trigger": "credential"}),
    ));
    let stale = next_of(
        &mut messages,
        "provider-provisioning-execution-result",
        &logs,
    )
    .await;
    assert_eq!(stale["status"], "failed");
    assert_eq!(stale["error"], "Remote Environment registration 已失效");

    // scoped attachment：scope 不匹配被拒；匹配时转发到远端。
    let scope = RemoteScope {
        remote_session_id: session_id.clone(),
        workspace_path: workspace_path.clone(),
        workspace_identity: identity.clone(),
    };
    let mut wrong = scope.clone();
    wrong.workspace_identity = workspace_path.clone();
    assert!(remote.validate_scope(&wrong).is_err());
    remote.validate_scope(&scope).unwrap();
    let local = ChannelRegistry::empty();
    let attachment = remote
        .attach(
            "attachment-1",
            ClientMode::DesktopContinuous,
            &scope,
            &local,
        )
        .await
        .unwrap();
    let registry = attachment.registry.clone();
    let desktop = context(1, ClientMode::DesktopContinuous);

    let info = value(
        call(&registry, &desktop, "system", "info", vec![])
            .await
            .unwrap(),
    );
    assert!(info.is_object(), "{info}");
    let text = value(
        call(
            &registry,
            &desktop,
            "file",
            "readTextFile",
            vec![json!({"path": format!("{workspace_path}/hello.txt")})],
        )
        .await
        .unwrap(),
    );
    assert_eq!(text["content"], "hello remote\n", "{text}");
    let bytes = call(
        &registry,
        &desktop,
        "file",
        "readFileRange",
        vec![json!({"path": format!("{workspace_path}/hello.txt"), "offset": 6, "length": 6})],
    )
    .await
    .unwrap();
    assert_eq!(bytes, Reply::Bytes(b"remote".to_vec()));
    let missing = call(
        &registry,
        &desktop,
        "file",
        "stat",
        vec![json!({"path": format!("{workspace_path}/missing.txt")})],
    )
    .await;
    assert!(missing.is_err() || value(missing.unwrap())["exists"] == false);
    assert!(
        registry.get("automation").is_none(),
        "unregistered channels stay unknown"
    );

    // zcode-agent：握手前拒绝，握手后经可信 carrier 转发；existing-only 不拉起远端 CLI。
    let target = json!({"workspacePath": workspace_path, "workspaceIdentity": identity});
    let early = call(
        &registry,
        &desktop,
        "zcode-agent",
        "subscribeSessionsIndexV4",
        vec![target.clone()],
    )
    .await
    .unwrap_err();
    assert_eq!(early.message, "fault.connection.handshakeRequired");
    let hello = value(
        call(
            &registry,
            &desktop,
            "zcode-agent",
            "helloConversationV4",
            vec![],
        )
        .await
        .unwrap(),
    );
    assert_eq!(hello["clientMode"], "desktop-continuous");
    assert!(
        hello["connectionId"]
            .as_str()
            .unwrap()
            .starts_with("host-rpc-")
    );
    call(
        &registry,
        &desktop,
        "zcode-agent",
        "initializeConversationV4",
        vec![json!({"kind": "clientHello", "protocolVersion": 3, "clientId": "c1", "appVersion": "test"})],
    )
    .await
    .unwrap();
    let mut subscribe = target.clone();
    subscribe["runtimePolicy"] = json!("existing-only");
    subscribe["connectionId"] = json!("forged");
    let dormant = call(
        &registry,
        &desktop,
        "zcode-agent",
        "subscribeSessionsIndexV4",
        vec![subscribe],
    )
    .await
    .unwrap_err();
    assert!(
        dormant.code_str() == Some("ZCODE_AGENT_RUNTIME_UNAVAILABLE")
            || dormant.message.contains("unavailable")
            || dormant.message.contains("Unavailable"),
        "{dormant:?}"
    );
    let resync = call(
        &registry,
        &desktop,
        "zcode-agent",
        "resyncSessionsIndexV4",
        vec![json!({"workspacePath": workspace_path, "workspaceIdentity": identity, "subscriptionId": "x"})],
    )
    .await
    .unwrap_err();
    assert_eq!(resync.message, "fault.subscription.notOwned");

    // Main：dispose-remote-workspace-session → attachment 断开、remote-workspace-closed{disposed}
    let mut closed = attachment.closed.clone();
    assert!(router.dispatch(
        "dispose-remote-workspace-session",
        json!({"type": "dispose-remote-workspace-session", "requestId": "d1", "remoteSessionId": session_id}),
    ));
    let disposed = next_of(&mut messages, "remote-workspace-closed", &logs).await;
    assert_eq!(disposed["reason"], "disposed");
    assert_eq!(disposed["remoteSessionId"], session_id.as_str());
    tokio::time::timeout(Duration::from_secs(5), closed.wait_for(|closed| *closed))
        .await
        .expect("attachment closed")
        .unwrap();
    assert!(remote.validate_scope(&scope).is_err());
    assert!(remote.attachment_ids().is_empty());
    remote.shutdown().await;
}
