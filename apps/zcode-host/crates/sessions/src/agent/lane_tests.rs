//! runtime 的 lane、偏好、门禁、进程事件与通知总线（docs/specs/rust-host-agent.md §2.1、§2.7、
//! §2.8、§2.10）：用一个 shell 写的假 CLI（逐行回 `{"id":N,"result":{}}`，并把收到的方法记进文件）。
#![cfg(unix)]
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};

use super::runtime::{AgentRuntime, AgentRuntimeConfig, Lane, RuntimePolicy, WorkspaceTarget};
use crate::providers::ProviderRuntime;
use crate::settings::SettingStore;

const DIAGNOSTIC: &str = r#"[zcode-process-exception] {"version":1,"errorId":"123e4567-e89b-42d3-a456-426614174000","kind":"uncaughtException","origin":"uncaughtException","name":"Error","message":"boom token=abc123","occurredAt":5}"#;

struct Fixture {
    _dir: tempfile::TempDir,
    workspace: PathBuf,
    log: PathBuf,
    runtime: AgentRuntime,
}

fn fake_cli(dir: &Path, log: &Path) -> PathBuf {
    let script = dir.join("fake-cli.sh");
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/sh
printf '%s\n' '{DIAGNOSTIC}' >&2
printf '%s\n' 'plain stderr line' >&2
printf '%s\n' '{{"method":"process/resourceSample","params":{{"cpuPercent":1}}}}'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{{"id":\([0-9]*\),.*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/^{{"id":[0-9]*,"method":"\([^"]*\)".*/\1/p')
  printf '%s %s\n' "$method" "$line" >> "{log}"
  [ -n "$id" ] && printf '{{"id":%s,"result":{{}}}}\n' "$id"
done
"#,
            log = log.display()
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&script, permissions).unwrap();
    script
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let log = dir.path().join("calls.log");
    let cli = fake_cli(dir.path(), &log);
    let runtime = AgentRuntime::new(
        AgentRuntimeConfig {
            base_env: BTreeMap::from([("PATH".to_owned(), "/usr/bin:/bin".to_owned())]),
            command_vars: BTreeMap::from([(
                "ZCODE_AGENT_SERVER_COMMAND".to_owned(),
                cli.display().to_string(),
            )]),
            fallback_cwd: None,
        },
        SettingStore::new(dir.path()),
        ProviderRuntime::new(None, dir.path().join("provider_config.json")),
    );
    Fixture {
        _dir: dir,
        workspace,
        log,
        runtime,
    }
}

fn calls(log: &Path) -> Vec<(String, Value)> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (method, raw) = line.split_once(' ')?;
            Some((method.to_owned(), serde_json::from_str(raw).ok()?))
        })
        .collect()
}

async fn eventually(mut check: impl FnMut() -> bool) {
    for _ in 0..200 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition not reached");
}

#[tokio::test]
async fn preferences_follow_the_latest_value_and_replay_on_new_processes() {
    let fixture = fixture();
    let target = WorkspaceTarget::local(fixture.workspace.display().to_string());
    let runtime = &fixture.runtime;
    // 没有进程时只记住，不拉起 CLI。
    runtime
        .sync_app_preferences(json!({"askUserQuestionAutoResolutionEnabled": false}))
        .await
        .unwrap();
    assert!(runtime.existing(&target).await.is_none());
    let process = runtime
        .get_client(&target, RuntimePolicy::StartIfNeeded)
        .await
        .unwrap();
    let log = calls(&fixture.log);
    assert_eq!(log[0].0, "workspace/updateInteractionPreferences");
    assert_eq!(
        log[0].1["params"]["preferences"],
        json!({"askUserQuestionAutoResolutionEnabled": false})
    );
    assert_eq!(log[1].0, "workspace/updateModelIoPreferences");
    assert_eq!(
        log[1].1["params"]["preferences"],
        json!({"fullRetentionEnabled": false})
    );
    // 运行中的进程收到新值；已推版本不再重复推送。
    runtime
        .sync_app_preferences(json!({"askUserQuestionAutoResolutionEnabled": true, "modelIoFullRetentionEnabled": true}))
        .await
        .unwrap();
    runtime
        .get_client(&target, RuntimePolicy::StartIfNeeded)
        .await
        .unwrap();
    let log = calls(&fixture.log);
    assert_eq!(log.len(), 4, "{log:?}");
    assert_eq!(
        log[3].1["params"]["preferences"],
        json!({"fullRetentionEnabled": true})
    );
    process.client.close("dispose_workspace");
}

#[tokio::test]
async fn lifecycle_events_diagnostics_and_notifications_reach_the_bus() {
    let fixture = fixture();
    let target = WorkspaceTarget::local(fixture.workspace.display().to_string());
    let runtime = &fixture.runtime;
    runtime.extensions().enable_off_peak_tools();
    let mut process_events = Box::pin(runtime.extensions().process_events().subscribe());
    let mut notifications = Box::pin(runtime.extensions().notifications().subscribe());
    let mut storage = Box::pin(runtime.storage_events().subscribe());
    let process = runtime
        .get_client(&target, RuntimePolicy::StartIfNeeded)
        .await
        .unwrap();
    // 闲时工具门禁打开且是本地 workspace：交出进程前推一次策略。
    assert!(calls(&fixture.log).iter().any(|(method, line)| method
        == "workspace/updateOffPeakToolPolicy"
        && line["params"]["enabled"] == true));
    let spawned = process_events.next().await.unwrap();
    assert_eq!(spawned.message["type"], "agent-process-spawned");
    assert_eq!(spawned.message["provider"], "glm");
    assert!(spawned.message.get("lane").is_none());
    assert!(
        spawned.message["runtimeInstanceId"]
            .as_str()
            .unwrap()
            .starts_with("agent-")
    );
    let exception = process_events.next().await.unwrap();
    assert_eq!(exception.message["type"], "agent-process-exception");
    assert_eq!(
        exception.message["diagnostic"]["message"],
        "boom token=<redacted>"
    );
    let sample = notifications.next().await.unwrap();
    assert_eq!(sample.method, "process/resourceSample");
    assert_eq!(sample.lane, Lane::Chat);
    assert_eq!(
        storage.next().await.unwrap()["snapshot"]["state"],
        Value::Null
    );

    runtime.dispose_workspace(&target).await;
    let exited = tokio::time::timeout(Duration::from_secs(10), process_events.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(exited.message["type"], "agent-process-exited");
    assert_eq!(exited.message["terminationKind"], "expected");
    assert_eq!(exited.message["terminationReason"], "workspace-dispose");
    assert_eq!(exited.message["runtimeReady"], false);
    // 进程诊断行不进 tail，普通行进 tail；expected 退出不带 tail。
    assert_eq!(exited.message["stderrLineCount"], 1);
    assert!(exited.message.get("stderrTail").is_none());
    assert!(process.client.is_closed());
}

#[tokio::test]
async fn mcp_status_lane_reclaims_idle_processes_and_labels_identity() {
    let fixture = fixture();
    let target = WorkspaceTarget::local(fixture.workspace.display().to_string());
    let lane = fixture
        .runtime
        .lane(Lane::McpStatus, Some(Duration::from_millis(150)));
    let mut lifecycle = Box::pin(lane.lifecycle().subscribe());
    let mut chat_lifecycle = Box::pin(fixture.runtime.lifecycle().subscribe());
    let process = lane
        .get_client(&target, RuntimePolicy::StartIfNeeded)
        .await
        .unwrap();
    assert_eq!(process.runtime_identity()["lane"], "mcp-status");
    assert!(
        process.runtime_identity()["identity"]
            .as_str()
            .unwrap()
            .ends_with(":mcp-status")
    );
    // 控制面 lane 不回放偏好、不推门禁策略。
    assert!(calls(&fixture.log).is_empty());
    process.request_value("mcp/list", &json!({})).await.unwrap();
    assert_eq!(lifecycle.next().await.unwrap()["state"], "available");
    let unavailable = tokio::time::timeout(Duration::from_secs(5), lifecycle.next())
        .await
        .expect("idle reclaim")
        .unwrap();
    assert_eq!(unavailable["state"], "unavailable");
    eventually(|| process.client.is_closed()).await;
    // 派生 lane 的生命周期不会出现在 chat lane 的事件上。
    assert!(
        tokio::time::timeout(Duration::from_millis(100), chat_lifecycle.next())
            .await
            .is_err()
    );
    // Host 退出时连同派生 lane 一起回收。
    let again = lane
        .get_client(&target, RuntimePolicy::StartIfNeeded)
        .await
        .unwrap();
    assert_eq!(again.generation, 2);
    fixture.runtime.dispose_all().await;
    eventually(|| again.client.is_closed()).await;
    let _ = Arc::strong_count(&again);
}
