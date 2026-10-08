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
        Reply::Encoded(body) => decoded(&body).into_json(),
        other => panic!("expected a JSON reply, got {other:?}"),
    }
}

/// 转发结果保持远端编码（Reply::Encoded）：测试里解码后比较。
fn decoded(body: &[u8]) -> RpcValue {
    zcode_host_rpc::Reader::new(body).value().unwrap()
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
    let Reply::Encoded(body) = bytes else {
        panic!("forwarded reads keep the remote encoding");
    };
    assert_eq!(decoded(&body), RpcValue::Bytes(b"remote".to_vec()));
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

    // 远端媒体预览代理：Range URL 由 Host 回环代理，分块读远端 file.readFileRange。
    let clip: Vec<u8> = (0..(2 * 1024 * 1024 + 123))
        .map(|index| (index % 251) as u8)
        .collect();
    std::fs::write(workspace.join("clip.mp4"), &clip).unwrap();
    let prepared = value(
        call(
            &registry,
            &desktop,
            "media-preview",
            "prepare",
            vec![json!({"path": format!("{workspace_path}/clip.mp4"), "expectedKind": "video"})],
        )
        .await
        .unwrap(),
    );
    assert_eq!(prepared["kind"], "host-range-url");
    assert_eq!(prepared["mediaType"], "video/mp4");
    assert_eq!(prepared["size"], clip.len());
    let url = prepared["url"].as_str().unwrap().to_owned();
    let (status, headers, body) = http_get(&url, "GET", Some("bytes=1048570-1048580")).await;
    assert_eq!(status, 206);
    assert!(headers.contains(&format!(
        "Content-Range: bytes 1048570-1048580/{}",
        clip.len()
    )));
    assert_eq!(body, clip[1048570..=1048580].to_vec());
    let (status, _, body) = http_get(&url, "GET", None).await;
    assert_eq!(status, 200);
    assert_eq!(body.len(), clip.len());
    assert!(body == clip, "full body matches across 1 MiB chunks");
    let (status, headers, _) = http_get(&url, "GET", Some("bytes=999999999-")).await;
    assert_eq!(status, 416);
    assert!(headers.contains(&format!("Content-Range: bytes */{}", clip.len())));
    let wrong_kind = call(
        &registry,
        &desktop,
        "media-preview",
        "prepare",
        vec![json!({"path": format!("{workspace_path}/clip.mp4"), "expectedKind": "audio"})],
    )
    .await
    .unwrap_err();
    assert!(
        wrong_kind
            .message
            .starts_with("Unsupported media preview format")
    );
    let outside = call(
        &registry,
        &desktop,
        "media-preview",
        "prepare",
        vec![json!({"path": "/etc/hosts", "expectedKind": "video"})],
    )
    .await
    .unwrap_err();
    assert_eq!(
        outside.message,
        "Media preview path is outside remote workspace"
    );
    call(
        &registry,
        &desktop,
        "media-preview",
        "release",
        vec![json!({"previewId": prepared["previewId"]})],
    )
    .await
    .unwrap();
    assert_eq!(http_get(&url, "GET", None).await.0, 404);
    let replay = remote
        .attach(
            "attachment-2",
            ClientMode::WebRemoteReplayable,
            &scope,
            &local,
        )
        .await
        .unwrap();
    assert!(
        replay.registry.get("media-preview").is_none(),
        "replayable attachments keep inline media"
    );

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

/// 最小 HTTP/1.1 客户端：返回 (状态码, 响应头文本, body)。
async fn http_get(url: &str, method: &str, range: Option<&str>) -> (u16, String, Vec<u8>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let rest = url.strip_prefix("http://").unwrap();
    let (authority, path) = rest.split_at(rest.find('/').unwrap());
    let mut stream = tokio::net::TcpStream::connect(authority).await.unwrap();
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some(range) = range {
        request.push_str(&format!("Range: {range}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let head = String::from_utf8_lossy(&response[..split]).into_owned();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, head, response[split + 4..].to_vec())
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../..")
        .canonicalize()
        .unwrap()
}

/// 产品传输 `ConnectorTransport` 的完整链路：Rust ⇄ TS 连接器（与产品相同的 `runRemoteConnector`，
/// 只是“部署 + 启动”换成本机拉起 zcode-server.cjs）⇄ 远端 server；附件物化经连接器的 backend channel。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connector_transport_relays_server_and_backend_channels() {
    use zcode_host_services::remote::transport::{
        ConnectParams, ConnectorConfig, ConnectorTransport, Transport,
    };
    let bundle = server_bundle();
    let root = repo_root();
    let script = root.join("scripts/rust-host/remote-connector-local.ts");
    let (Some(node), true, true) = (
        find_node(),
        bundle.is_file(),
        root.join("node_modules/tsx").exists(),
    ) else {
        eprintln!("skipping: needs node, tsx (pnpm install) and the server bundle");
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let remote_home = temp.path().join("remote-home");
    std::fs::create_dir_all(&remote_home).unwrap();
    let local_file = temp.path().join("note.txt");
    std::fs::write(&local_file, "attached").unwrap();
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
        (
            "ZCODE_SERVICE_AUTHORITY_MODE",
            "desktop-attached-remote".into(),
        ),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value))
    .collect();
    let transport = ConnectorTransport::new(ConnectorConfig {
        node_exec: node.clone(),
        args: vec![
            "--import".into(),
            "tsx".into(),
            script.display().to_string(),
            node.display().to_string(),
            bundle.display().to_string(),
        ],
        env,
        cwd: root.clone(),
    });
    let logs = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = logs.clone();
    let (_abort, abort) = tokio::sync::watch::channel(false);
    let connection = tokio::time::timeout(
        Duration::from_secs(60),
        transport.connect(ConnectParams {
            target: json!({"kind": "docker", "container": "local-test"}),
            remote_assets: json!({}),
            remote_runtime_network: None,
            network: json!({}),
            log: Arc::new(move |_, message| sink.lock().unwrap().push(message)),
            abort,
        }),
    )
    .await
    .expect("connected within 60s")
    .unwrap_or_else(|error| panic!("connect failed: {error}"));
    assert_eq!(connection.hello["type"], "zcode-hello");
    assert!(
        logs.lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("local test connector: target=docker")),
        "{:?}",
        logs.lock().unwrap()
    );
    let info = connection
        .client
        .call("system", "info", vec![])
        .await
        .unwrap();
    assert!(info.into_json().is_object());

    let backend = connection
        .backend
        .as_ref()
        .expect("connector backend channel");
    let local_path = local_file.display().to_string();
    let materialized = backend
        .call(
            "remote-backend",
            "materializePromptAttachments",
            vec![RpcValue::Json(json!({
                "taskId": "t1", "traceId": "trace-1",
                "content": format!("see {local_path}"),
                "attachments": [{"kind": "file", "filename": "note.txt", "localPath": local_path}],
            }))],
        )
        .await
        .unwrap()
        .into_json();
    let remote_path = materialized["attachments"][0]["localPath"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        remote_path.contains("/.zcode/tmp/prompt-attachments/trace-1/"),
        "{remote_path}"
    );
    assert_eq!(materialized["content"], format!("see {remote_path}"));
    assert_eq!(std::fs::read_to_string(&remote_path).unwrap(), "attached");

    tokio::time::timeout(Duration::from_secs(15), connection.dispose())
        .await
        .expect("connector exits after dispose");
    let closed = connection.closed().await;
    assert!(closed.exit_code.is_some() || closed.error.is_some());
}
