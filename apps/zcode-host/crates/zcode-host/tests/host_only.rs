//! “只有 Host”形态（docs/specs/rust-host-cli-boundary.md §3.3、§3.4、§3.5）：
//! `cargo build -p zcode-host --no-default-features` 不链接任何 CLI 代码。这个构建没有 `cli` 入口；
//! 不给 `ZCODE_AGENT_SERVER_COMMAND` 时启动 runtime（这里是启动期的会话库准备）失败，并说明要设置
//! 哪些变量。只在关掉 feature `embedded-runtime` 时编译（`pnpm test:rust-host` 的“只有 Host”一步）。
#![cfg(not(feature = "embedded-runtime"))]
#![expect(clippy::disallowed_methods, reason = "集成测试从进程环境取 PATH")]
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const LIMIT: Duration = Duration::from_secs(60);
const NO_RUNTIME: &str = "this zcode-host build does not include the in-process runtime; set \
                          ZCODE_AGENT_SERVER_COMMAND to a zcode-cli-rust executable and \
                          ZCODE_AGENT_SERVER_RUNTIME=zcode-cli-rust";

fn host(program: &Path, home: &Path) -> tokio::process::Command {
    let mut command = zcode_proc::command(program);
    command
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("ZCODE_DESKTOP_HOME_DIR", home)
        .env("ZCODE_DATA_BASE_DIR", home)
        .kill_on_drop(true);
    command
}

#[tokio::test]
async fn the_cli_entry_is_not_included() {
    let home = tempfile::tempdir().unwrap();
    let named = home
        .path()
        .join(format!("zcode-cli-rust{}", std::env::consts::EXE_SUFFIX));
    #[cfg(unix)]
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_zcode-host"), &named).unwrap();
    #[cfg(not(unix))]
    std::fs::copy(env!("CARGO_BIN_EXE_zcode-host"), &named).unwrap();
    let mut subcommand = host(Path::new(env!("CARGO_BIN_EXE_zcode-host")), home.path());
    subcommand.args(["cli", "--version"]);
    let mut by_name = host(&named, home.path());
    by_name.arg("--version");
    for mut command in [subcommand, by_name] {
        let output = command.output().await.unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "zcode-host: this build does not include the CLI (built without the embedded-runtime \
             feature); use a zcode-cli-rust executable\n"
        );
    }
}

#[tokio::test]
async fn a_runtime_start_without_a_command_fails() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut child = host(Path::new(env!("CARGO_BIN_EXE_zcode-host")), home.path())
        .args(["--allowed-origin", "file://"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();
    let init = json!({
        "type": "init-local",
        "databaseStartupId": "startup-host-only",
        "zcodeBuiltinProviderConfigFilePath": "/x",
        "agentWarmupTargets": [{"workspacePath": workspace.path()}],
        "agentSpawnFallbackCwd": home.path(),
    });
    stdin
        .write_all(format!("{init}\n").as_bytes())
        .await
        .unwrap();
    let mut warned = false;
    let failed = tokio::time::timeout(LIMIT, async {
        while let Some(line) = stdout.next_line().await.unwrap() {
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["type"] == "log" && message["level"] == "warn" {
                warned |= message["message"].as_str().unwrap().contains(NO_RUNTIME);
            }
            if message["type"] == "database-startup-state" {
                let state = &message["state"];
                assert_ne!(state["phase"], "ready", "{state}");
                if state["phase"] == "failed" {
                    return state.clone();
                }
            }
        }
        panic!("the Host closed its output before startup ended");
    })
    .await
    .expect("startup did not end");
    assert_eq!(
        failed["failedPhase"], "preparing_session_storage",
        "{failed}"
    );
    assert_eq!(failed["errorCode"], "unsupported_runtime", "{failed}");
    assert!(
        warned,
        "the Host did not say why the runtime is unavailable"
    );
}
