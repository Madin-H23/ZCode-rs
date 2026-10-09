use std::sync::Mutex as StdMutex;

use zcode_host_control::{ControlSink, HostToMain};

use super::*;
use crate::agent::WorkspaceTarget;

#[derive(Default)]
struct Sink(StdMutex<Vec<Value>>);

impl ControlSink for Sink {
    fn send(&self, message: HostToMain) {
        if let HostToMain::Raw(value) = message {
            self.0.lock().unwrap().push(value);
        }
    }
}

impl Sink {
    fn take(&self) -> Vec<Value> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
}

fn bridge(timeout: Duration) -> (BrowserBridge, Arc<Sink>) {
    let sink = Arc::new(Sink::default());
    let bridge = BrowserBridge::with_timeout(Some(MainBridge::new(sink.clone())), timeout);
    (bridge, sink)
}

fn input(request_id: &str, command: Value) -> ExecuteInput {
    ExecuteInput {
        request_id: Some(request_id.into()),
        session_id: "s1".into(),
        workspace_key: Some("/w".into()),
        workspace_path: Some("/w".into()),
        command,
        ..ExecuteInput::default()
    }
}

async fn wait_for_post(sink: &Sink) -> Value {
    for _ in 0..200 {
        if let Some(message) = sink.take().into_iter().next() {
            return message;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("no message posted to Main");
}

#[tokio::test]
async fn executes_through_main_and_correlates_results() {
    let (bridge, sink) = bridge(Duration::from_secs(5));
    let descriptor = bridge.descriptor();
    assert!(descriptor["id"].as_str().unwrap().starts_with("iab:"));
    assert_eq!(descriptor["metadata"]["provider"], "zcode-desktop-iab");
    let running = {
        let bridge = bridge.clone();
        tokio::spawn(async move { bridge.execute(input("r1", json!({"method": "list"}))).await })
    };
    let request = wait_for_post(&sink).await;
    assert_eq!(request["type"], EXECUTE_REQUEST);
    assert_eq!(request["requestId"], "r1");
    assert_eq!(request["browserId"], descriptor["id"]);
    assert_eq!(request["clientMode"], "desktop-continuous");
    assert_eq!(request["sessionContext"], "live");
    assert!(request.get("turnId").is_none(), "absent fields are omitted");
    // 同一 requestId 在途时拒绝。
    let duplicate = bridge.execute(input("r1", json!({"method": "list"}))).await;
    assert_eq!(duplicate["error"]["code"], "duplicate_request_id");
    assert_eq!(duplicate["error"]["sideEffect"], "none");
    bridge.handle_result(
        &json!({"type": EXECUTE_RESULT, "requestId": "r1", "result": {"ok": true, "tabs": []}}),
    );
    assert_eq!(running.await.unwrap(), json!({"ok": true, "tabs": []}));
    // 迟到 / 未知结果直接丢弃。
    bridge.handle_result(&json!({"requestId": "r1", "result": {"ok": true}}));
}

#[tokio::test]
async fn rejects_foreign_or_stale_backends_without_posting() {
    let (bridge, sink) = bridge(Duration::from_secs(5));
    let mut foreign = input("r", json!({"method": "list"}));
    foreign.browser_id = Some("iab:other".into());
    let result = bridge.execute(foreign).await;
    assert_eq!(result["error"]["code"], "backend_unavailable");
    let mut stale = input("r", json!({"method": "list"}));
    stale.browser_generation = Some(1);
    let result = bridge.execute(stale).await;
    assert!(
        result["error"]["message"]
            .as_str()
            .unwrap()
            .contains("is stale")
    );
    assert!(sink.take().is_empty());
}

#[tokio::test]
async fn timeout_sends_cancel_and_marks_side_effects() {
    let (bridge, sink) = bridge(Duration::from_millis(50));
    let result = bridge
        .execute(input(
            "click-1",
            json!({"method": "click", "selector": "a"}),
        ))
        .await;
    assert_eq!(result["error"]["code"], "timeout");
    assert_eq!(result["error"]["sideEffect"], "uncertain");
    let messages = sink.take();
    assert_eq!(messages.len(), 2);
    assert_eq!(
        messages[1]["command"],
        json!({"method": "cancelRequest", "requestId": "click-1"})
    );
    assert_ne!(messages[1]["requestId"], "click-1");
    let result = bridge
        .execute(input("list-1", json!({"method": "list"})))
        .await;
    assert_eq!(result["error"]["sideEffect"], "none");
    // 等待类命令的预算 = 命令时长 + 2 s。
    assert_eq!(
        bridge.request_timeout(&json!({"method": "playwrightWaitForTimeout", "timeoutMs": 40_000})),
        Duration::from_secs(42)
    );
    assert_eq!(
        bridge.request_timeout(&json!({"method": "playwright", "action": {"name": "goto"}})),
        Duration::from_millis(50) + TRANSPORT_GRACE
    );
}

#[tokio::test]
async fn recording_is_copied_into_the_workspace() {
    let (bridge, sink) = bridge(Duration::from_secs(5));
    let workspace = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let recorded = temp.path().join("main.webm");
    std::fs::write(&recorded, b"webm").unwrap();
    let mut request = input(
        "rec",
        json!({"method": "recordingStatus", "outputPath": "out/demo.webm"}),
    );
    request.workspace_path = Some(workspace.path().display().to_string());
    let running = {
        let bridge = bridge.clone();
        tokio::spawn(async move { bridge.execute(request).await })
    };
    wait_for_post(&sink).await;
    let artifact = json!({"path": recorded.display().to_string(), "mime": "video/webm"});
    bridge.handle_result(&json!({"requestId": "rec", "result": {"ok": true, "recording": {"status": "completed", "artifact": artifact}}}));
    let result = running.await.unwrap();
    let target = workspace.path().join("out/demo.webm");
    assert_eq!(
        result["recording"]["artifact"]["path"],
        json!(target.display().to_string())
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"webm");

    // 越界路径拒绝；不带 outputPath 时去掉 Main 的临时 artifact。
    let mut escape = input(
        "rec2",
        json!({"method": "recordingStatus", "outputPath": "../x.webm"}),
    );
    escape.workspace_path = Some(workspace.path().display().to_string());
    let running = {
        let bridge = bridge.clone();
        tokio::spawn(async move { bridge.execute(escape).await })
    };
    wait_for_post(&sink).await;
    bridge.handle_result(&json!({"requestId": "rec2", "result": {"ok": true, "recording": {"status": "completed", "artifact": artifact}}}));
    assert_eq!(running.await.unwrap()["error"]["code"], "execution_error");
    let running = {
        let bridge = bridge.clone();
        tokio::spawn(async move {
            bridge
                .execute(input("rec3", json!({"method": "recordingStop"})))
                .await
        })
    };
    wait_for_post(&sink).await;
    bridge.handle_result(&json!({"requestId": "rec3", "result": {"ok": true, "recording": {"status": "completed", "artifact": artifact}}}));
    assert_eq!(
        running.await.unwrap()["recording"],
        json!({"status": "completed"})
    );
}

#[tokio::test]
async fn dispose_settles_pending_as_uncertain() {
    let (bridge, sink) = bridge(Duration::from_secs(5));
    let running = {
        let bridge = bridge.clone();
        tokio::spawn(async move { bridge.execute(input("r", json!({"method": "list"}))).await })
    };
    wait_for_post(&sink).await;
    bridge.dispose();
    let result = running.await.unwrap();
    assert_eq!(result["error"]["code"], "backend_unavailable");
    assert_eq!(result["error"]["sideEffect"], "uncertain");
}

#[tokio::test]
async fn reverse_requests_validate_params() {
    let (bridge, _sink) = bridge(Duration::from_secs(5));
    let context = |params: Value| ReverseContext {
        target: WorkspaceTarget::local("/w"),
        workspace_key: "/w".into(),
        generation: 1,
        method: "interaction/browserList".into(),
        params,
    };
    let valid = json!({"requestId": "r", "sessionId": "s", "workspaceKey": "/w", "workspacePath": "/w", "clientMode": "desktop-continuous", "sessionContext": "live"});
    match bridge.reverse_list(&context(valid.clone())) {
        ReverseReply::Result(result) => assert_eq!(result["browsers"].as_array().unwrap().len(), 1),
        other => panic!("{other:?}"),
    }
    let mut extra = valid.clone();
    extra["x"] = json!(1);
    assert!(matches!(
        bridge.reverse_list(&context(extra)),
        ReverseReply::Error { code: -32602, .. }
    ));
    let mut blank = valid;
    blank["sessionId"] = json!("  ");
    assert!(matches!(
        bridge.reverse_list(&context(blank)),
        ReverseReply::Error { code: -32602, .. }
    ));
    let reply = bridge
        .reverse_execute(context(json!({"requestId": "r", "sessionId": "s"})))
        .await;
    assert!(matches!(reply, ReverseReply::Error { code: -32602, .. }));
}

#[test]
fn ambient_urls_are_sanitized() {
    assert_eq!(
        sanitize_ambient_url("https://user:pw@example.test/a?b=1").as_deref(),
        Some("https://example.test/a?b=1")
    );
    assert_eq!(
        sanitize_ambient_url("about:blank").as_deref(),
        Some("about:blank")
    );
    assert_eq!(sanitize_ambient_url("file:///etc/passwd"), None);
    assert_eq!(sanitize_ambient_url("not a url"), None);
    let long = format!("https://example.test/{}", "a".repeat(5000));
    assert_eq!(sanitize_ambient_url(&long), None);
}
