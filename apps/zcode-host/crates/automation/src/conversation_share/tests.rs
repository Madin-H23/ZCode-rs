//! conversation-share 测试：只连 127.0.0.1 上的 mock 分享服务。
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::net::TcpListener;

use super::client::{ShareClient, TokenProvider};
use super::import::{ImportInput, Importer, format_shared_context, sanitize_file_segment};
use super::integrity::{sha256_hex, sha256_json};
use crate::agent::AgentRuntime;
use crate::off_peak::gateway::{read_request, write_json};

fn envelope(data: Value) -> Value {
    json!({"code": 0, "msg": "ok", "data": data})
}

fn no_token() -> TokenProvider {
    Arc::new(|| Box::pin(async { None }))
}

const ARTIFACT: &[u8] = b"# hello\n";

fn continuation(origin: &str, tamper: bool) -> Value {
    let rows = json!([
        {"kind": "turnHeader", "rowId": 1, "productTurnId": "t1"},
        {"kind": "userInput", "rowId": 2, "productTurnId": "t1", "text": "hi"},
        {"kind": "assistantText", "rowId": 3, "productTurnId": "t1", "text": "hello", "extra": 1.5},
        {"kind": "futureKind", "rowId": 4},
    ]);
    let artifacts = json!([{
        "artifact_id": "a1",
        "logical_artifact_key": "k1",
        "producer_product_turn_id": "t1",
        "artifact_version": 1,
        "state": "current",
        "ref": "zcode-artifact://share/a1",
        "artifact_type": "md",
        "display_name": "notes.md",
        "extension": "md",
        "mime_type": "text/markdown",
        "size_bytes": ARTIFACT.len(),
        "sha256": sha256_hex(ARTIFACT),
    }]);
    let projection = sha256_json(&rows).unwrap();
    let artifact_set = sha256_json(&artifacts).unwrap();
    let mut artifacts = artifacts;
    artifacts[0]["download_url"] = json!(format!("{origin}/download/a1"));
    artifacts[0]["download_url_expires_at"] = json!(1);
    if tamper {
        artifacts[0]["display_name"] = json!("evil.md");
    }
    json!({
        "schema_version": 1,
        "import_grant_id": "g1",
        "import_grant_expires_at": 1,
        "share": {"share_id": "share-1", "title": "Demo", "access_mode": "public_importable",
                  "created_at": 1, "expires_at": 2},
        "rows": rows,
        "artifacts": artifacts,
        "integrity": {"projection_sha256": projection, "artifact_set_sha256": artifact_set},
    })
}

/// mock 分享服务：preview / continuation / 下载；`tamper` 让 continuation 的清单被改动。
async fn mock_server() -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let base = origin.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                continue;
            };
            let base = base.clone();
            tokio::spawn(async move {
                let Ok(Some(request)) = read_request(&mut stream).await else {
                    return;
                };
                let path = request.path.as_str();
                if path == "/download/a1" {
                    use tokio::io::AsyncWriteExt;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        ARTIFACT.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(ARTIFACT).await;
                    return;
                }
                let (status, body) = match path {
                    "/api/v1/shares/good/continuation" => {
                        (200, envelope(continuation(&base, false)))
                    }
                    "/api/v1/shares/bad/continuation" => (200, envelope(continuation(&base, true))),
                    "/api/v1/shares/good/preview" | "/api/v1/shares/newer/preview" => {
                        let mut data = continuation(&base, false);
                        if path.contains("newer") {
                            data["schema_version"] = json!(9);
                        }
                        let object = data.as_object_mut().unwrap();
                        object.remove("import_grant_id");
                        object.remove("import_grant_expires_at");
                        for artifact in data["artifacts"].as_array_mut().unwrap() {
                            artifact["url"] = artifact["download_url"].clone();
                            artifact["url_expires_at"] = json!(1);
                        }
                        (200, envelope(data))
                    }
                    _ => (404, json!({"code": 3211, "msg": "share not found"})),
                };
                let _ = write_json(
                    &mut stream,
                    status,
                    &body,
                    &[("x-request-id", "req-1".into())],
                )
                .await;
            });
        }
    });
    origin
}

#[tokio::test]
async fn client_gates_auth_and_verifies_public_payloads() {
    let origin = mock_server().await;
    let client = ShareClient::new(format!("{origin}/api/v1"), no_token()).unwrap();
    let auth = client.capabilities().await.unwrap_err().to_rpc().to_json();
    assert_eq!(auth["kind"], "authentication_required");
    assert_eq!(auth["status"], 401);

    let preview = client.preview("good").await.unwrap();
    assert_eq!(preview["unsupportedRowCount"], 1);
    assert_eq!(preview["rows"].as_array().unwrap().len(), 3);
    let newer = client.preview("newer").await.unwrap_err();
    assert_eq!(newer.kind, "unsupported_schema_version");
    let missing = client.preview("gone").await.unwrap_err().to_rpc().to_json();
    assert_eq!(missing["kind"], "not_found");
    assert_eq!(missing["code"], 3211);
    assert_eq!(missing["status"], 404);
    assert_eq!(missing["details"]["requestId"], "req-1");

    let continuation = client.continuation("good", "req").await.unwrap();
    assert_eq!(continuation["rawRows"].as_array().unwrap().len(), 4);
    let tampered = client.continuation("bad", "req").await.unwrap_err();
    assert_eq!(tampered.kind, "invalid_contract");
    assert_eq!(
        tampered.message,
        "Conversation share integrity check failed"
    );
}

#[tokio::test]
async fn import_installs_artifacts_then_cleans_up_when_the_session_cannot_be_created() {
    let origin = mock_server().await;
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(ShareClient::new(format!("{origin}/api/v1"), no_token()).unwrap());
    // 没有可用模型：createSession 走写门禁失败，导入必须回滚本次目录。
    let agent = AgentRuntime::new(
        crate::agent::AgentRuntimeConfig {
            base_env: Default::default(),
            command_vars: Default::default(),
            fallback_cwd: None,
        },
        crate::settings::SettingStore::new(dir.path()),
        crate::providers::ProviderRuntime::new(None, dir.path().join("p.json")),
    );
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let importer = Importer::new(
        agent,
        None,
        client,
        dir.path().join("default"),
        "https://zcode.example/cn/share".into(),
    );
    let mut progress = Box::pin(importer.progress().subscribe());
    let input = ImportInput {
        share_code: "good".into(),
        client_request_id: "req".into(),
        target_workspace_path: Some(workspace.to_string_lossy().into_owned()),
        ..ImportInput::default()
    };
    let error = importer
        .import(input, "op-1".into())
        .await
        .unwrap_err()
        .to_rpc();
    assert!(error.message.contains("模型"), "{}", error.message);
    use futures_util::StreamExt;
    let first = progress.next().await.unwrap();
    assert_eq!(
        first,
        json!({"operationId": "op-1", "phase": "downloading", "completedArtifacts": 0, "totalArtifacts": 1})
    );
    assert!(!workspace.join(".zcode-share").join("share-1").exists());
    assert!(
        importer
            .imported_conversation(&workspace.to_string_lossy(), "x")
            .await
            .is_null()
    );
}

#[test]
fn shared_context_markdown_matches_the_formatter() {
    let installed = vec![json!({
        "artifactId": "a1", "workspaceRelativePath": ".zcode-share/s/shared-artifacts/n.md",
        "displayName": "n.md", "mimeType": "text/markdown", "sha256": "ab",
    })];
    let rows = vec![
        json!({"kind": "userInput", "text": "hi", "attachments": [{"ref": "zcode-artifact://share/a1"}]}),
        json!({"kind": "toolCall", "toolName": "Bash", "inputText": "ls ```", "output": {"text": "ok"}}),
        json!({"kind": "artifact", "artifactVersionId": "a1"}),
        json!({"kind": "newKind"}),
    ];
    let (markdown, sha, unsupported) = format_shared_context("T", &rows, &installed).unwrap();
    assert!(markdown.starts_with("# Shared conversation: T\n\n## User\n\nhi\n\n### Attachments\n\n- n.md\n  - Path: .zcode-share/s/shared-artifacts/n.md"));
    assert!(markdown.contains("````\nls ```\n````"));
    assert_eq!(sha, sha256_hex(markdown.as_bytes()));
    assert_eq!(unsupported, ["newKind"]);
    assert_eq!(sanitize_file_segment("a/b:c"), "a-b-c");
}
