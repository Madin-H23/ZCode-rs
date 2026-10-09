//! 嵌入入口的端到端测试（docs/specs/rust-u1-runtime-library.md §5）：runtime 只使用
//! `RuntimeOptions` 给出的环境与路径，一个 runtime 服务多个 workspace，停掉后可以在同一进程重启。
#![expect(
    clippy::disallowed_methods,
    reason = "读取测试进程自己的 PATH 与 cwd，用来证明 runtime 没有使用它们"
)]
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use zcode_cli_rust::{embed, runtime::RuntimeOptions, serve::AppServerOptions};

const LIMIT: Duration = Duration::from_secs(20);

fn sse(delta: Value, finish: Option<&str>) -> String {
    let mut out = format!("data: {}\n\n", json!({"choices": [{"delta": delta}]}));
    if let Some(reason) = finish {
        out.push_str(&format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices": [{"delta": {}, "finish_reason": reason}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 4}})
        ));
    }
    out
}

/// 让假模型一直不回复的输入；用来保持一个 run 处于运行中。
const HANG: &str = "hang until cancelled";
/// 只出现在注入环境里的故障标记（`ZCODE_RUNTIME_FAULT_MARKER`）。
const FAULT: &str = "fault-marker-6c1d";

/// Chat Completions 假模型：标题请求回标题；最后一条消息不是工具结果时调用 Write，否则回文本并结束。
fn respond(body: &str) -> String {
    if body.contains("Generate a concise title for this coding session.") {
        return sse(json!({"content": "{\"title\":\"Embedded\"}"}), Some("stop"));
    }
    let request: Value = serde_json::from_str(body).unwrap_or_default();
    let last_is_tool = request["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .is_some_and(|message| message["role"] == "tool");
    if last_is_tool {
        return sse(json!({"content": "done"}), Some("stop"));
    }
    let arguments = json!({"file_path": "result.txt", "content": "written by Rust"}).to_string();
    sse(
        json!({"tool_calls": [{"index": 0, "id": "call-write", "type": "function",
            "function": {"name": "Write", "arguments": arguments}}]}),
        Some("tool_calls"),
    )
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<String> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&bytes[..header_end]).to_ascii_lowercase();
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))?
        .trim()
        .parse()
        .ok()?;
    while bytes.len() < header_end + length {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    Some(String::from_utf8_lossy(&bytes[header_end..header_end + length]).into_owned())
}

struct Mock {
    url: String,
    /// 收到后一直不回复、对端也还没断开的模型请求数。
    hanging: Arc<AtomicUsize>,
}

async fn mock_model() -> Mock {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let hanging = Arc::new(AtomicUsize::new(0));
    let counter = hanging.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let hanging = counter.clone();
            tokio::spawn(async move {
                let Some(body) = read_request(&mut socket).await else {
                    return;
                };
                if body.contains(HANG) && !body.contains("Generate a concise title") {
                    // 不回复，等对端断开：run 被取消时 HTTP 请求随之关闭。
                    hanging.fetch_add(1, Ordering::SeqCst);
                    let mut rest = [0u8; 64];
                    while matches!(socket.read(&mut rest).await, Ok(read) if read > 0) {}
                    hanging.fetch_sub(1, Ordering::SeqCst);
                    return;
                }
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(respond(&body).as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    Mock {
        url: format!("http://{address}/v1"),
        hanging,
    }
}

struct Client {
    lines: tokio::io::Lines<BufReader<ReadHalf<tokio::io::DuplexStream>>>,
    writer: WriteHalf<tokio::io::DuplexStream>,
    next_id: u64,
}

impl Client {
    fn new(io: tokio::io::DuplexStream) -> Self {
        let (reader, writer) = tokio::io::split(io);
        Self {
            lines: BufReader::new(reader).lines(),
            writer,
            next_id: 0,
        }
    }

    async fn until(&mut self, accept: impl Fn(&Value) -> bool) -> Value {
        tokio::time::timeout(LIMIT, async {
            loop {
                let line = self
                    .lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("runtime closed its output");
                let message: Value = serde_json::from_str(&line).unwrap();
                if accept(&message) {
                    return message;
                }
            }
        })
        .await
        .expect("no matching message from the runtime")
    }

    async fn ready(&mut self) {
        self.until(|m| m["method"] == "startup/storageState" && m["params"]["phase"] == "ready")
            .await;
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let line = format!(
            "{}\n",
            json!({"id": id, "method": method, "params": params})
        );
        self.writer.write_all(line.as_bytes()).await.unwrap();
        let reply = self
            .until(|m| m["id"] == id && m.get("method").is_none())
            .await;
        assert!(reply.get("error").is_none(), "{method} failed: {reply}");
        reply["result"].clone()
    }

    async fn create_session(&mut self, workspace: &Path, text: &str) {
        let ack = self
            .request(
                "v4/command",
                json!({"commandId": format!("create-{}", self.next_id), "clientId": "embed-test",
                    "sessionId": null, "type": "createSession", "issuedAt": 1000,
                    "payload": {"workspaceId": workspace, "firstInput": {"text": text}}}),
            )
            .await;
        assert_eq!(ack["status"], "accepted", "{ack}");
    }
}

async fn wait_for_file(path: &Path) -> String {
    tokio::time::timeout(LIMIT, async {
        loop {
            if let Ok(content) = tokio::fs::read_to_string(path).await
                && !content.is_empty()
            {
                return content;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("file was not produced: {}", path.display()))
}

fn options(root: &Path, workspace: &Path, model: &Path) -> AppServerOptions {
    let text = |path: PathBuf| path.to_string_lossy().into_owned();
    AppServerOptions {
        runtime: RuntimeOptions {
            home: root.to_owned(),
            // 只出现在这份环境里的变量：进程环境中没有 ZCODE_SESSION_DB_PATH，HOME 也指向别处。
            env: vec![
                ("HOME".into(), text(root.to_owned())),
                ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
                (
                    "ZCODE_SESSION_DB_PATH".into(),
                    text(root.join("sessions.sqlite")),
                ),
                ("ZCODE_RUNTIME_FAULT_MARKER".into(), FAULT.into()),
            ],
            argv: vec!["zcode-cli-rust".into()],
            cwd: workspace.to_owned(),
            workspace_identity: None,
            // 相对的数据目录按 base_dir 解析，而不是按测试进程的 cwd。
            base_dir: root.to_owned(),
            data_dir: PathBuf::from("data"),
        },
        config: Some(model.to_owned()),
        desktop: false,
        prepare_storage: false,
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    first: PathBuf,
    second: PathBuf,
    model: PathBuf,
    mock: Mock,
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = tokio::fs::canonicalize(dir.path()).await.unwrap();
    let (first, second) = (root.join("workspace-a"), root.join("workspace-b"));
    for directory in [&first, &second, &root.join(".zcode/cli")] {
        tokio::fs::create_dir_all(directory).await.unwrap();
    }
    // 与 Node 驱动的夹具相同：yolo 模式下工具不经审批直接执行。
    tokio::fs::write(
        root.join(".zcode/cli/config.json"),
        json!({"permission": {"mode": "yolo"}}).to_string(),
    )
    .await
    .unwrap();
    let mock = mock_model().await;
    let model = root.join("model.json");
    tokio::fs::write(
        &model,
        json!({"providerId": "fixture", "modelId": "core-model", "reasoningLevel": "none",
            "reasoningParameters": {"reasoning_effort": "none"}, "baseUrl": mock.url,
            "requestTimeoutSeconds": 60})
        .to_string(),
    )
    .await
    .unwrap();
    Fixture {
        _dir: dir,
        root,
        first,
        second,
        model,
        mock,
    }
}

async fn until(what: &str, check: impl Fn() -> bool) {
    tokio::time::timeout(LIMIT, async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

#[tokio::test]
async fn embedded_runtime_uses_its_options_serves_two_workspaces_and_restarts() {
    let Fixture {
        _dir,
        root,
        first,
        second,
        model,
        ..
    } = fixture().await;
    let process_cwd = std::env::current_dir().unwrap();
    assert!(!process_cwd.join("result.txt").exists());

    let runtime = embed::spawn(options(&root, &first, &model));
    let mut client = Client::new(runtime.io);
    client.ready().await;
    // 同一个 runtime 里的两个 workspace：缺省的和请求里另给的；相对路径各自落在自己的目录下。
    client.create_session(&first, "write").await;
    client.create_session(&second, "write").await;
    for workspace in [&first, &second] {
        assert_eq!(
            wait_for_file(&workspace.join("result.txt")).await,
            "written by Rust"
        );
    }
    assert!(!process_cwd.join("result.txt").exists());
    assert!(
        root.join("sessions.sqlite").exists(),
        "the injected environment names the database"
    );
    assert!(
        root.join("data").is_dir(),
        "the data dir resolves against base_dir"
    );

    // 关闭写端等价于 stdin EOF：runtime 收口后任务正常结束。
    client.writer.shutdown().await.unwrap();
    tokio::time::timeout(LIMIT, runtime.task)
        .await
        .expect("the runtime did not stop after EOF")
        .unwrap()
        .unwrap();

    // 同一进程内再启动一个 runtime：进程级状态不会让它卡住。
    let again = embed::spawn(options(&root, &first, &model));
    let mut client = Client::new(again.io);
    client.ready().await;
    client.request("runtime/capabilities", json!({})).await;
    client.writer.shutdown().await.unwrap();
    tokio::time::timeout(LIMIT, again.task)
        .await
        .expect("the restarted runtime did not stop after EOF")
        .unwrap()
        .unwrap();
}

/// Engine 循环 panic（调试构建的故障注入）：runtime 任务以错误结束，但先收口——运行中的 run 被
/// 取消，它的模型请求随之关闭；之后同一进程可以再启动一个 runtime。
#[tokio::test]
async fn an_engine_panic_cancels_its_runs_before_the_runtime_ends() {
    let Fixture {
        _dir,
        root,
        first,
        model,
        mock,
        ..
    } = fixture().await;
    let runtime = embed::spawn(options(&root, &first, &model));
    let mut client = Client::new(runtime.io);
    client.ready().await;
    client.create_session(&first, HANG).await;
    let hanging = mock.hanging.clone();
    until("the model request of the run", || {
        hanging.load(Ordering::SeqCst) == 1
    })
    .await;

    let line = format!(
        "{}\n",
        json!({"id": 99, "method": "runtime/capabilities", "params": {"note": FAULT}})
    );
    client.writer.write_all(line.as_bytes()).await.unwrap();
    let ended = tokio::time::timeout(LIMIT, runtime.task)
        .await
        .expect("the runtime did not end after the engine panicked")
        .expect("the panic must not escape the runtime task");
    let error = format!(
        "{:#}",
        ended.expect_err("an engine panic is a runtime failure")
    );
    assert!(error.contains("fault injection"), "{error}");
    until("the run's model request to be closed", || {
        hanging.load(Ordering::SeqCst) == 0
    })
    .await;

    let again = embed::spawn(options(&root, &first, &model));
    let mut client = Client::new(again.io);
    client.ready().await;
    client.request("runtime/capabilities", json!({})).await;
    client.writer.shutdown().await.unwrap();
    tokio::time::timeout(LIMIT, again.task)
        .await
        .expect("the restarted runtime did not stop after EOF")
        .unwrap()
        .unwrap();
}
