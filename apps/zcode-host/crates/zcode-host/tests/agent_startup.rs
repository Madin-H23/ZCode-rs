//! 启动期的会话库准备（docs/specs/rust-host-agent.md §2.11）：拉起真实的 zcode-host，
//! 以 Rust CLI 的 `--prepare-storage` 准备 warmup workspace 的会话库，启动状态必须经过
//! `preparing_session_storage` 再进入 `ready`。runtime 缺省为嵌入式；设置
//! `ZCODE_TEST_CLI_RUST_BINARY` 时走子进程链路作差分对照。
#![expect(
    clippy::disallowed_methods,
    reason = "集成测试从进程环境取被测二进制与 PATH"
)]
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// runtime 的选择：缺省嵌入式；`ZCODE_TEST_CLI_RUST_BINARY` 指向可执行文件时走子进程链路。
fn runtime_env() -> Vec<(&'static str, String)> {
    match std::env::var_os("ZCODE_TEST_CLI_RUST_BINARY").map(PathBuf::from) {
        Some(cli) => {
            assert!(
                cli.is_file(),
                "ZCODE_TEST_CLI_RUST_BINARY is not a file: {}",
                cli.display()
            );
            vec![
                (
                    "ZCODE_AGENT_SERVER_COMMAND",
                    cli.to_string_lossy().into_owned(),
                ),
                ("ZCODE_AGENT_SERVER_RUNTIME", "zcode-cli-rust".to_owned()),
            ]
        }
        None => vec![("ZCODE_AGENT_SERVER_RUNTIME", "embedded".to_owned())],
    }
}

#[tokio::test]
async fn startup_prepares_session_storage_with_the_cli() {
    // 测试 Host 的 home 与数据根：绝不能读写开发机真实的 ~/.zcode。
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut child = zcode_proc::command(env!("CARGO_BIN_EXE_zcode-host"))
        .args(["--allowed-origin", "file://"])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("ZCODE_DESKTOP_HOME_DIR", home.path())
        .env("ZCODE_DATA_BASE_DIR", home.path())
        .envs(runtime_env())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn zcode-host");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();
    let init = json!({
        "type": "init-local",
        "databaseStartupId": "startup-agent",
        "zcodeBuiltinProviderConfigFilePath": "/x",
        "agentWarmupTargets": [{"workspacePath": workspace.path()}],
        "agentSpawnFallbackCwd": home.path(),
    });
    stdin
        .write_all(format!("{init}\n").as_bytes())
        .await
        .unwrap();
    let mut session_phases = Vec::new();
    let mut phases = Vec::new();
    let finished = tokio::time::timeout(Duration::from_secs(60), async {
        while let Ok(Some(line)) = stdout.next_line().await {
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["type"] != "database-startup-state" {
                continue;
            }
            let state = &message["state"];
            let phase = state["phase"].as_str().unwrap().to_owned();
            if phase == "preparing_session_storage" {
                session_phases.push(state["databasePhase"].as_str().unwrap_or("").to_owned());
                if state["databasePhase"] == "ready" {
                    assert_eq!(state["finalDatabase"], true, "{state}");
                }
            }
            phases.push(phase.clone());
            if phase == "failed" {
                panic!("startup failed: {state}");
            }
            if phase == "ready" {
                return;
            }
        }
    })
    .await;
    assert!(finished.is_ok(), "startup did not finish: {phases:?}");
    assert_eq!(session_phases.first().map(String::as_str), Some("checking"));
    assert!(
        session_phases.iter().any(|phase| phase == "ready"),
        "{session_phases:?}"
    );
    let session_index = phases
        .iter()
        .position(|phase| phase == "preparing_session_storage")
        .unwrap();
    let host_index = phases
        .iter()
        .position(|phase| phase == "preparing_host_storage")
        .unwrap();
    let services_index = phases
        .iter()
        .position(|phase| phase == "starting_services")
        .unwrap();
    assert!(
        host_index < session_index && session_index < services_index,
        "{phases:?}"
    );
    let _ = child.kill().await;
}
