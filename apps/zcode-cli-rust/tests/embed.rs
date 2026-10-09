//! 嵌入入口的端到端测试（docs/specs/rust-u1-runtime-library.md §5）：runtime 只使用
//! `RuntimeOptions` 给出的环境与路径，一个 runtime 服务多个 workspace，停掉后可以在同一进程重启；
//! 实例结束后（EOF、Engine panic、被嵌入方中止）它的任务都已结束（docs/specs/rust-u2-task-scope.md §5）。
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
/// 让模型先启动一个后台 Bash（记下 pid 后 exec sleep）的输入。
const BACKGROUND: &str = "start a background job";
/// 由这条输入生成的标题请求一直不回复（标题在首轮成功结束后才请求）。
const HANG_TITLE: &str = "keep the title pending";
const TITLE: &str = "Generate a concise title for this coding session.";
/// 只出现在注入环境里的故障标记（`ZCODE_RUNTIME_FAULT_MARKER`）。
const FAULT: &str = "fault-marker-6c1d";

/// 请求的消息与本轮新增的部分（最后一条 assistant 消息之后的消息，序列化成文本）。
fn request(body: &str) -> (Vec<Value>, String) {
    let request: Value = serde_json::from_str(body).unwrap_or_default();
    let messages = request["messages"].as_array().cloned().unwrap_or_default();
    let start = messages
        .iter()
        .rposition(|message| message["role"] == "assistant")
        .map_or(0, |at| at + 1);
    let pending = Value::from(messages[start..].to_vec()).to_string();
    (messages, pending)
}

fn last_is_tool(messages: &[Value]) -> bool {
    messages
        .last()
        .is_some_and(|message| message["role"] == "tool")
}

/// 收到后不回复、只等对端断开的请求。
fn hangs(body: &str) -> bool {
    if body.contains(TITLE) {
        return body.contains(HANG_TITLE);
    }
    let (messages, pending) = request(body);
    !last_is_tool(&messages) && pending.contains(HANG)
}

/// Chat Completions 假模型：标题请求回标题；最后一条消息是工具结果时回文本并结束，否则调用
/// 后台 Bash（本轮输入含 BACKGROUND）或 Write。
fn respond(body: &str) -> String {
    if body.contains(TITLE) {
        return sse(json!({"content": "{\"title\":\"Embedded\"}"}), Some("stop"));
    }
    let (messages, pending) = request(body);
    if last_is_tool(&messages) {
        return sse(json!({"content": "done"}), Some("stop"));
    }
    let (name, arguments) = if pending.contains(BACKGROUND) {
        let command = "echo $$ > background.pid; exec sleep 60";
        let arguments = json!({"command": command, "run_in_background": true,
            "description": "background job"});
        ("Bash", arguments)
    } else {
        let arguments = json!({"file_path": "result.txt", "content": "written by Rust"});
        ("Write", arguments)
    };
    sse(
        json!({"tool_calls": [{"index": 0, "id": "call-tool", "type": "function",
            "function": {"name": name, "arguments": arguments.to_string()}}]}),
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
                if hangs(&body) {
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

    /// Creates a session with `text` as its first input; returns its id.
    async fn create_session(&mut self, workspace: &Path, text: &str) -> String {
        let ack = self
            .request(
                "v4/command",
                json!({"commandId": format!("create-{}", self.next_id), "clientId": "embed-test",
                    "sessionId": null, "type": "createSession", "issuedAt": 1000,
                    "payload": {"workspaceId": workspace, "firstInput": {"text": text}}}),
            )
            .await;
        assert_eq!(ack["status"], "accepted", "{ack}");
        ack["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("no session id: {ack}"))
            .to_owned()
    }

    async fn send_text(&mut self, session: &str, text: &str) {
        let ack = self
            .request(
                "v4/command",
                json!({"commandId": format!("send-{}", self.next_id), "clientId": "embed-test",
                    "sessionId": session, "type": "sendText", "issuedAt": 2000,
                    "payload": {"text": text}}),
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

/// 实例结束之后，它的任务很快全部结束：异步任务随作用域释放被中止，阻塞任务（会话存储的工作
/// 线程）在通道关闭后自行退出。
async fn no_task_left(tasks: &embed::Tasks) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !tasks.running().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("tasks of the ended instance: {:?}", tasks.running()));
}

/// 实例结束后，runtime 那一端的管道已释放：嵌入方读到输出结尾，写入失败，而不是被一个残留的
/// 读任务收下。
async fn pipe_closed(client: &mut Client) {
    tokio::time::timeout(LIMIT, async {
        while matches!(client.lines.next_line().await, Ok(Some(_))) {}
        // 不带换行：残留的读任务只会把它攒进当前行，不会因为投递失败而自行退出。
        while client.writer.write_all(b"x").await.is_ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a task of the ended instance still holds its end of the pipe");
}

/// 进程已经结束（或只剩等待回收的僵尸进程）。
#[cfg(unix)]
fn gone(pid: i32) -> bool {
    let stat = zcode_proc::std_command("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    let stat = String::from_utf8_lossy(&stat.stdout);
    stat.trim().is_empty() || stat.trim().starts_with('Z')
}

/// 测试失败时也回收后台进程，不遗留 60 秒的 sleep。
struct Reap(Option<i32>);

impl Drop for Reap {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0 {
            // SAFETY: 只向本测试启动、记下 pid 的进程发信号。
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        #[cfg(not(unix))]
        let _ = self.0;
    }
}

/// 一个忙碌的会话：首轮（Unix 上启动一个后台 Bash）结束后，标题请求未回复；第二轮的 run 停在
/// 模型请求上。
async fn busy_session(client: &mut Client, workspace: &Path, hanging: &AtomicUsize) -> Reap {
    let first = if cfg!(unix) {
        format!("{BACKGROUND}; {HANG_TITLE}")
    } else {
        format!("write; {HANG_TITLE}")
    };
    let session = client.create_session(workspace, &first).await;
    let background = if cfg!(unix) {
        let pid = wait_for_file(&workspace.join("background.pid")).await;
        Some(pid.trim().parse().expect("background pid"))
    } else {
        None
    };
    let reap = Reap(background);
    until("the title request", || hanging.load(Ordering::SeqCst) == 1).await;
    client.send_text(&session, HANG).await;
    until("the second run's model request", || {
        hanging.load(Ordering::SeqCst) == 2
    })
    .await;
    reap
}

/// 实例结束后：模型请求都已关闭，后台进程已结束，实例中没有任务在运行。
async fn nothing_left(hanging: &AtomicUsize, background: &Reap, tasks: &embed::Tasks) {
    until("the model requests to be closed", || {
        hanging.load(Ordering::SeqCst) == 0
    })
    .await;
    #[cfg(unix)]
    if let Some(pid) = background.0 {
        until("the background job to end", || gone(pid)).await;
    }
    #[cfg(not(unix))]
    let _ = background;
    no_task_left(tasks).await;
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
    let tasks = runtime.tasks.clone();
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
    no_task_left(&tasks).await;

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

/// EOF 时会话仍在忙：收口取消 run、标题请求与后台任务，实例结束后什么都不剩。
#[tokio::test]
async fn eof_while_busy_leaves_nothing_of_the_instance() {
    let Fixture {
        _dir,
        root,
        first,
        model,
        mock,
        ..
    } = fixture().await;
    let runtime = embed::spawn(options(&root, &first, &model));
    let tasks = runtime.tasks.clone();
    let mut client = Client::new(runtime.io);
    client.ready().await;
    let background = busy_session(&mut client, &first, &mock.hanging).await;
    client.writer.shutdown().await.unwrap();
    tokio::time::timeout(LIMIT, runtime.task)
        .await
        .expect("the runtime did not stop after EOF")
        .unwrap()
        .unwrap();
    nothing_left(&mock.hanging, &background, &tasks).await;
}

/// Engine 循环 panic（调试构建的故障注入）：runtime 任务以错误结束，但先收口——运行中的 run、
/// 标题请求与后台任务都被取消，模型请求随之关闭；实例的 stdio 读任务随实例结束，不再占着管道
/// （原来它一直留到嵌入方关闭写端）。之后同一进程可以再启动一个 runtime。
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
    let tasks = runtime.tasks.clone();
    let mut client = Client::new(runtime.io);
    client.ready().await;
    let background = busy_session(&mut client, &first, &mock.hanging).await;

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
    pipe_closed(&mut client).await;
    nothing_left(&mock.hanging, &background, &tasks).await;

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

/// 嵌入方中止实例任务（Host 的收口超时后就这样做）：实例的作用域随之释放，run、标题请求、
/// 后台 Bash 与 stdio 读任务都被中止——模型请求关闭，后台进程被结束，管道被释放。原来它们是
/// 分离的 spawn：Engine 在实例之外继续收口，读任务一直占着管道（嵌入方的写入被它收下）。
#[tokio::test]
async fn aborting_the_instance_aborts_its_tasks() {
    let Fixture {
        _dir,
        root,
        first,
        model,
        mock,
        ..
    } = fixture().await;
    let runtime = embed::spawn(options(&root, &first, &model));
    let tasks = runtime.tasks.clone();
    let mut client = Client::new(runtime.io);
    client.ready().await;
    let background = busy_session(&mut client, &first, &mock.hanging).await;

    runtime.task.abort();
    let ended = tokio::time::timeout(LIMIT, runtime.task)
        .await
        .expect("the aborted runtime task did not end");
    assert!(ended.is_err_and(|error| error.is_cancelled()));
    pipe_closed(&mut client).await;
    nothing_left(&mock.hanging, &background, &tasks).await;
}
