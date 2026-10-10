//! 合并的二进制当 CLI 用（docs/specs/rust-host-cli-boundary.md §3.4、§5）：`zcode-host cli <参数>`
//! 与以 `zcode-cli-rust` 为名运行（链接或副本）时，行为与独立的 `zcode-cli-rust` 相同。
//!
//! 期望值是独立 CLI 的已知输出（版本取自 CLI workspace 的清单）。设置 `ZCODE_TEST_CLI_RUST_BINARY`
//! 指向独立 CLI 时，另外用同样的参数运行它，逐字节比较退出码、stdout 与 stderr。
#![cfg(feature = "embedded-runtime")]
#![expect(
    clippy::disallowed_methods,
    reason = "集成测试从进程环境取 PATH 与可选的独立 CLI"
)]
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const LIMIT: Duration = Duration::from_secs(60);
const MOCK_REPLY: &str = "Hello from the mock model";

/// 以 `zcode-cli-rust` 为名的合并二进制（Unix 用符号链接，其他平台用副本）。
fn cli_named_binary(dir: &Path) -> PathBuf {
    let path = dir.join(format!("zcode-cli-rust{}", std::env::consts::EXE_SUFFIX));
    #[cfg(unix)]
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_zcode-host"), &path).unwrap();
    #[cfg(not(unix))]
    std::fs::copy(env!("CARGO_BIN_EXE_zcode-host"), &path).unwrap();
    path
}

/// 让合并的二进制以 CLI 身份运行的两种方式，外加（可选的）独立 CLI 作对照。
struct Invocations {
    _dir: tempfile::TempDir,
    named: PathBuf,
    standalone: Option<PathBuf>,
}

impl Invocations {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let named = cli_named_binary(dir.path());
        let standalone = std::env::var_os("ZCODE_TEST_CLI_RUST_BINARY").map(PathBuf::from);
        Self {
            _dir: dir,
            named,
            standalone,
        }
    }

    /// `(名字, 命令)`：`zcode-host cli …`、`zcode-cli-rust …`（合并二进制）、独立 CLI（设置了时）。
    fn commands(&self, home: &Path) -> Vec<(&'static str, tokio::process::Command)> {
        let command = |program: &Path| {
            let mut command = zcode_proc::command(program);
            command
                .env_clear()
                .env("PATH", std::env::var("PATH").unwrap_or_default())
                .env("HOME", home)
                .env("USERPROFILE", home)
                .stdin(Stdio::null())
                .kill_on_drop(true);
            command
        };
        let mut subcommand = command(Path::new(env!("CARGO_BIN_EXE_zcode-host")));
        subcommand.arg("cli");
        let mut commands = vec![
            ("zcode-host cli", subcommand),
            ("zcode-cli-rust (merged)", command(&self.named)),
        ];
        if let Some(standalone) = &self.standalone {
            commands.push(("zcode-cli-rust (standalone)", command(standalone)));
        }
        commands
    }
}

/// `(退出码, stdout, stderr)`。
type Output = (Option<i32>, String, String);

async fn run(mut command: tokio::process::Command, args: &[&str]) -> Output {
    let output = tokio::time::timeout(LIMIT, command.args(args).output())
        .await
        .expect("the CLI did not exit")
        .unwrap();
    (
        output.status.code(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

/// 每种调用方式（设置了独立 CLI 时也包括它）用同样的参数各运行一次。
async fn run_all(
    invocations: &Invocations,
    home: &Path,
    args: &[&str],
) -> Vec<(&'static str, Output)> {
    let mut outputs = Vec::new();
    for (name, command) in invocations.commands(home) {
        outputs.push((name, run(command, args).await));
    }
    outputs
}

/// 所有调用方式的输出相同，返回这份输出。
fn same(outputs: Vec<(&'static str, Output)>) -> Output {
    let (first, expected) = outputs[0].clone();
    for (name, output) in &outputs[1..] {
        assert_eq!(output, &expected, "{name} differs from {first}");
    }
    expected
}

/// CLI workspace 的版本（`[workspace.package] version`），即独立 CLI 的 `--version`。
fn cli_version() -> String {
    let manifest = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../zcode-cli-rust/Cargo.toml"),
    )
    .unwrap();
    let package = &manifest[manifest.find("[workspace.package]").unwrap()..];
    let line = package
        .lines()
        .find_map(|line| line.strip_prefix("version = "))
        .unwrap();
    line.trim_matches('"').to_owned()
}

#[tokio::test]
async fn version_and_help_match_the_cli() {
    let home = tempfile::tempdir().unwrap();
    let invocations = Invocations::new();
    let version = cli_version();
    for flag in ["--version", "-v"] {
        let output = same(run_all(&invocations, home.path(), &[flag]).await);
        assert_eq!(output, (Some(0), format!("{version}\n"), String::new()));
    }
    let (code, stdout, stderr) = same(run_all(&invocations, home.path(), &["--help"]).await);
    assert_eq!((code, stderr.as_str()), (Some(0), ""));
    assert!(
        stdout.starts_with(&format!(
            "zcode-cli-rust {version}\n\nUsage:\n  zcode-cli-rust [command] [options]\n"
        )),
        "{stdout}"
    );
}

#[tokio::test]
async fn invalid_arguments_fail_like_the_cli() {
    let home = tempfile::tempdir().unwrap();
    let invocations = Invocations::new();
    // 顶层参数由 headless 解析：消息与帮助写到 stderr，退出码 1。
    let (code, stdout, stderr) = same(run_all(&invocations, home.path(), &["--bogus"]).await);
    assert_eq!((code, stdout.as_str()), (Some(1), ""));
    assert!(stderr.starts_with("Unknown option '--bogus'."), "{stderr}");
    // 子命令由 clap 解析：用法里的程序名是 CLI 的名字，退出码 2。
    assert_eq!(
        same(run_all(&invocations, home.path(), &["app-server"]).await),
        (
            Some(2),
            String::new(),
            "error: the following required arguments were not provided:\n  --stdio\n\n\
             Usage: zcode-cli-rust app-server --stdio\n\n\
             For more information, try '--help'.\n"
                .to_owned()
        )
    );
    assert_eq!(
        same(run_all(&invocations, home.path(), &["tui"]).await),
        (
            Some(2),
            String::new(),
            "zcode-cli-rust: TUI 尚未实现，请使用 app-server --stdio 或 -p\n".to_owned()
        )
    );
}

/// 静态模型配置（`--config`）：指向 `port` 上的假模型。
fn model_config(dir: &Path, port: u16) -> PathBuf {
    let path = dir.join("model.json");
    std::fs::write(
        &path,
        json!({
            "providerId": "fixture", "modelId": "core-model", "reasoningLevel": "none",
            "reasoningParameters": {"reasoning_effort": "none"},
            "baseUrl": format!("http://127.0.0.1:{port}/v1"), "requestTimeoutSeconds": 5,
        })
        .to_string(),
    )
    .unwrap();
    path
}

#[tokio::test]
async fn app_server_speaks_the_protocol_on_stdio() {
    let invocations = Invocations::new();
    let home = tempfile::tempdir().unwrap();
    let workspace = home.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    // 不会被请求：这次会话只问 runtime 的能力。
    let config = model_config(home.path(), 9);
    for (index, (name, mut command)) in invocations.commands(home.path()).into_iter().enumerate() {
        let data = home.path().join(format!("data-{index}"));
        let mut child = command
            .args(["app-server", "--stdio", "--cwd"])
            .arg(&workspace)
            .arg("--data-dir")
            .arg(&data)
            .arg("--config")
            .arg(&config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut next = async || -> Value {
            let line = tokio::time::timeout(LIMIT, lines.next_line())
                .await
                .expect("no output from the CLI")
                .unwrap()
                .expect("the CLI closed stdout");
            serde_json::from_str(&line).unwrap_or_else(|_| panic!("{name}: not JSON: {line}"))
        };
        // 存储握手：checking → ready，之后才处理请求。
        let checking = next().await;
        assert_eq!(checking["method"], "startup/storageState", "{name}");
        assert_eq!(checking["params"]["phase"], "checking", "{name}");
        let ready = next().await;
        assert_eq!(ready["params"]["phase"], "ready", "{name}");
        stdin
            .write_all(b"{\"id\":1,\"method\":\"runtime/capabilities\",\"params\":{}}\n")
            .await
            .unwrap();
        let reply = next().await;
        assert_eq!(reply["id"], 1, "{name}: {reply}");
        assert!(reply["result"].is_object(), "{name}: {reply}");
        // stdin EOF 是退出请求：干净地退出，stdout 不再有任何内容。
        drop(stdin);
        let status = tokio::time::timeout(LIMIT, child.wait())
            .await
            .expect("the CLI did not exit on stdin EOF")
            .unwrap();
        assert_eq!(status.code(), Some(0), "{name}");
        assert_eq!(lines.next_line().await.unwrap(), None, "{name}");
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .await
            .unwrap();
        assert_eq!(stderr, "", "{name}");
    }
}

/// 每个请求都回同一段 Chat Completions SSE 流。
async fn serve_mock_model(listener: TcpListener) {
    while let Ok((mut stream, _)) = listener.accept().await {
        tokio::spawn(async move {
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 8192];
            let body_start = loop {
                let Ok(read) = stream.read(&mut chunk).await else {
                    return;
                };
                if read == 0 {
                    return;
                }
                buffer.extend_from_slice(&chunk[..read]);
                if let Some(index) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8_lossy(&buffer[..body_start]).to_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while buffer.len() < body_start + length {
                match stream.read(&mut chunk).await {
                    Ok(read) if read > 0 => buffer.extend_from_slice(&chunk[..read]),
                    _ => break,
                }
            }
            let event = |delta: Value, finish: Value| json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish}]});
            let mut stop = event(json!({}), json!("stop"));
            stop["usage"] = json!({"prompt_tokens": 10, "completion_tokens": 4});
            let body = format!(
                "data: {}\n\ndata: {stop}\n\ndata: [DONE]\n\n",
                event(json!({"content": MOCK_REPLY}), Value::Null)
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        });
    }
}

#[tokio::test]
async fn headless_prompt_runs_against_a_mock_model() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let _model = zcode_async::owned(serve_mock_model(listener));
    let invocations = Invocations::new();
    let home = tempfile::tempdir().unwrap();
    let workspace = home.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let config = model_config(home.path(), port);
    let workspace = workspace.to_string_lossy().into_owned();
    let config = config.to_string_lossy().into_owned();
    for (index, (name, command)) in invocations.commands(home.path()).into_iter().enumerate() {
        let data = home.path().join(format!("data-{index}"));
        let data = data.to_string_lossy().into_owned();
        let args = [
            "-p",
            "hello",
            "--cwd",
            &workspace,
            "--data-dir",
            &data,
            "--config",
            &config,
        ];
        let output = run(command, &args).await;
        assert_eq!(
            output,
            (Some(0), format!("{MOCK_REPLY}\n"), String::new()),
            "{name}"
        );
    }
}
