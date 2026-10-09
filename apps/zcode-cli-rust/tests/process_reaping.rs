//! 工具进程在 runtime 所在进程被强杀后的回收（docs/specs/rust-tool-process-reaping.md 4）：真实的
//! `zcode-cli-rust app-server --stdio` 对着本地假模型运行 Bash，测试只向自己启动、记下 pid 的进程发信号。
#![cfg(unix)]
#![expect(
    clippy::disallowed_methods,
    reason = "测试启动的 runtime 沿用测试进程的 PATH"
)]
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};

const LIMIT: Duration = Duration::from_secs(30);
/// 安静、很久不结束的命令：bash 等待它的后台 `sleep 300`（bash 是根进程，sleep 在它的进程组里）。
const QUIET: &str = "echo $$ > bash.pid; sleep 300 & echo $! > sleep.pid; wait";
/// 正常结束的命令；运行期间先留下标记文件，测试据此确认账本里确实有它的记录。
const QUICK: &str = "touch started; sleep 1; echo quick";
const TITLE: &str = "Generate a concise title for this coding session.";

fn sse(delta: Value, finish: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": delta}]}),
        json!({"choices": [{"delta": {}, "finish_reason": finish}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 4}})
    )
}

/// Chat Completions 假模型：标题请求回标题；最后一条消息是工具结果时回文本（并计数），否则让
/// 模型调用 Bash，命令取自首条用户输入。
fn respond(body: &str, results: &AtomicUsize) -> String {
    if body.contains(TITLE) {
        return sse(json!({"content": "{\"title\":\"Reaping\"}"}), "stop");
    }
    let request: Value = serde_json::from_str(body).unwrap_or_default();
    let messages = request["messages"].as_array().cloned().unwrap_or_default();
    if messages.last().is_some_and(|m| m["role"] == "tool") {
        results.fetch_add(1, Ordering::SeqCst);
        return sse(json!({"content": "done"}), "stop");
    }
    let command = if body.contains("quiet") { QUIET } else { QUICK };
    let arguments = json!({"command": command, "description": "test command"});
    sse(
        json!({"tool_calls": [{"index": 0, "id": "call-bash", "type": "function",
            "function": {"name": "Bash", "arguments": arguments.to_string()}}]}),
        "tool_calls",
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

/// 启动假模型；返回地址与收到的工具结果数。
async fn mock_model() -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let results = Arc::new(AtomicUsize::new(0));
    let counter = results.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let results = counter.clone();
            tokio::spawn(async move {
                let Some(body) = read_request(&mut socket).await else {
                    return;
                };
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(respond(&body, &results).as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (format!("http://{address}/v1"), results)
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
    model: PathBuf,
    results: Arc<AtomicUsize>,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = tokio::fs::canonicalize(dir.path()).await.unwrap();
        let workspace = root.join("workspace");
        for directory in [&workspace, &root.join(".zcode/cli")] {
            tokio::fs::create_dir_all(directory).await.unwrap();
        }
        // yolo：工具不经审批直接执行（与 Node 驱动的夹具相同）。
        let config = json!({"permission": {"mode": "yolo"}}).to_string();
        tokio::fs::write(root.join(".zcode/cli/config.json"), config)
            .await
            .unwrap();
        let (url, results) = mock_model().await;
        let model = root.join("model.json");
        let model_config = json!({"providerId": "fixture", "modelId": "core-model",
            "reasoningLevel": "none", "reasoningParameters": {"reasoning_effort": "none"},
            "baseUrl": url, "requestTimeoutSeconds": 60});
        tokio::fs::write(&model, model_config.to_string())
            .await
            .unwrap();
        Self {
            _dir: dir,
            root,
            workspace,
            model,
            results,
        }
    }

    /// 账本目录：`<数据目录>/tool-results/processes`。
    fn ledger(&self) -> PathBuf {
        self.root.join("data/tool-results/processes")
    }

    /// 启动 runtime 进程（每次都用同一个数据目录）。
    async fn start(&self) -> Runtime {
        let text = |path: &Path| path.to_string_lossy().into_owned();
        let mut child = zcode_proc::command(env!("CARGO_BIN_EXE_zcode-cli-rust"))
            .args(["app-server", "--stdio", "--cwd", &text(&self.workspace)])
            .args(["--data-dir", &text(&self.root.join("data"))])
            .args(["--config", &text(&self.model)])
            .env_clear()
            .env("HOME", &self.root)
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("ZCODE_SESSION_DB_PATH", self.root.join("sessions.sqlite"))
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut runtime = Runtime {
            input: child.stdin.take().unwrap(),
            lines: BufReader::new(child.stdout.take().unwrap()).lines(),
            child,
            next_id: 0,
        };
        runtime
            .until(|m| m["method"] == "startup/storageState" && m["params"]["phase"] == "ready")
            .await;
        runtime
    }
}

struct Runtime {
    child: Child,
    input: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: u64,
}

impl Runtime {
    async fn until(&mut self, accept: impl Fn(&Value) -> bool) -> Value {
        tokio::time::timeout(LIMIT, async {
            loop {
                let line = self
                    .lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("runtime closed");
                let message: Value = serde_json::from_str(&line).unwrap();
                if accept(&message) {
                    return message;
                }
            }
        })
        .await
        .expect("no matching message from the runtime")
    }

    async fn create_session(&mut self, workspace: &Path, text: &str) {
        self.next_id += 1;
        let id = self.next_id;
        let line = json!({"id": id, "method": "v4/command", "params": {
            "commandId": format!("create-{id}"), "clientId": "reaping-test", "sessionId": null,
            "type": "createSession", "issuedAt": 1000,
            "payload": {"workspaceId": workspace, "firstInput": {"text": text}}}});
        self.input
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
        let reply = self
            .until(|m| m["id"] == id && m.get("method").is_none())
            .await;
        assert_eq!(reply["result"]["status"], "accepted", "{reply}");
    }

    /// stdin EOF：runtime 收口后退出。
    async fn stop(mut self) {
        self.input.shutdown().await.unwrap();
        drop(self.input);
        let status = tokio::time::timeout(LIMIT, self.child.wait()).await;
        assert!(status.expect("the runtime did not stop").unwrap().success());
    }
}

/// 进程存在且不是僵尸进程。
fn running(pid: i32) -> bool {
    let stat = zcode_proc::std_command("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    let stat = String::from_utf8_lossy(&stat.stdout);
    !stat.trim().is_empty() && !stat.trim().starts_with('Z')
}

/// 测试失败时也结束自己记下 pid 的进程，不遗留 300 秒的 sleep。
struct Reap(Vec<i32>);

impl Drop for Reap {
    fn drop(&mut self) {
        for pid in &self.0 {
            // SAFETY: 只向本测试启动的 Bash 命令里、记下 pid 的进程发信号。
            unsafe {
                libc::kill(*pid, libc::SIGKILL);
            }
        }
    }
}

async fn until(what: &str, check: impl Fn() -> bool) {
    tokio::time::timeout(LIMIT, async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

async fn pid_in(path: PathBuf) -> i32 {
    until(&format!("{}", path.display()), || {
        std::fs::read_to_string(&path).is_ok_and(|text| text.ends_with('\n'))
    })
    .await;
    std::fs::read_to_string(&path)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn records(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    entries
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect()
}

#[tokio::test]
async fn a_killed_runtime_leaves_its_bash_command_until_the_next_start_reaps_it() {
    let fixture = Fixture::new().await;
    let mut runtime = fixture.start().await;
    runtime.create_session(&fixture.workspace, "quiet").await;
    let bash = pid_in(fixture.workspace.join("bash.pid")).await;
    let sleep = pid_in(fixture.workspace.join("sleep.pid")).await;
    let _reap = Reap(vec![bash, sleep]);
    let ledger = records(&fixture.ledger());
    assert!(
        ledger.len() == 1 && ledger[0].starts_with(&format!("bash-{bash}-")),
        "{ledger:?}"
    );

    // runtime 所在的进程被强杀：没有任何收口。
    runtime.child.start_kill().unwrap();
    runtime.child.wait().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    // 这就是缺陷：Bash 命令自成会话，父进程退出后被过继，继续运行。
    assert_eq!((running(bash), running(sleep)), (true, true));

    // 同一数据目录再次启动：清扫结束整棵树并删除记录。
    let again = fixture.start().await;
    until("the left-over Bash command to be reaped", || {
        !running(bash) && !running(sleep)
    })
    .await;
    until("the ledger to be empty", || {
        records(&fixture.ledger()).is_empty()
    })
    .await;
    again.stop().await;
    assert_eq!(records(&fixture.ledger()), Vec::<String>::new());
}

#[tokio::test]
async fn a_normal_run_with_tool_calls_leaves_the_ledger_empty() {
    let fixture = Fixture::new().await;
    let mut runtime = fixture.start().await;
    runtime.create_session(&fixture.workspace, "quick").await;
    // 命令运行期间账本里有它的记录。
    until("the command to start", || {
        fixture.workspace.join("started").exists()
    })
    .await;
    let ledger = records(&fixture.ledger());
    assert!(
        ledger.len() == 1 && ledger[0].starts_with("bash-"),
        "{ledger:?}"
    );
    until("the tool result to reach the model", || {
        fixture.results.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(records(&fixture.ledger()), Vec::<String>::new());
    runtime.stop().await;
    assert_eq!(records(&fixture.ledger()), Vec::<String>::new());
}
