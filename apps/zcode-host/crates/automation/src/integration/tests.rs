//! integration 模块的单元测试（docs/specs/rust-host-integration.md §6）。
use std::collections::BTreeMap;

use serde_json::json;

use super::*;

#[test]
fn session_create_message_matches_the_host_schema() {
    let vars = BTreeMap::from([
        ("TZ".to_owned(), "UTC".to_owned()),
        ("LANG".to_owned(), "zh_CN.UTF-8".to_owned()),
    ]);
    let message = session_create_message(
        &vars,
        &SessionCreate {
            session_id: "s1",
            message_id: "run-1",
            source: CreateSource::AutomationScheduled,
            workspace_identity: None,
        },
    );
    assert_eq!(
        message,
        json!({
            "type": "session-create-telemetry",
            "event": {
                "elementName": "session_create",
                "eventRegion": "app",
                "eventType": "result",
                "talkId": "s1",
                "messageId": "run-1",
                "context": {"clientTimezone": "UTC", "clientLanguage": "zh-CN", "screenResolution": ""},
                "eventExtraDetail": {
                    "create_source": "automation_scheduled",
                    "client_kind": "desktop",
                    "workspace_kind": "local",
                    "remote_kind": "",
                },
            },
        })
    );
    let remote = session_create_message(
        &vars,
        &SessionCreate {
            session_id: "s2",
            message_id: "t",
            source: CreateSource::AutomationIdle,
            workspace_identity: Some("remote:ssh:host:22:user:/repo"),
        },
    );
    assert_eq!(
        remote["event"]["eventExtraDetail"]["create_source"],
        "automation_idle"
    );
    assert_eq!(
        remote["event"]["eventExtraDetail"]["workspace_kind"],
        "remote"
    );
    assert_eq!(remote["event"]["eventExtraDetail"]["remote_kind"], "ssh");
    // 有 identity 但不是远端格式：resolveWorkspaceTelemetryDetail 仍记 remote，种类为空。
    let odd = session_create_message(
        &vars,
        &SessionCreate {
            session_id: "s3",
            message_id: "t",
            source: CreateSource::AutomationIdle,
            workspace_identity: Some("local-id"),
        },
    );
    assert_eq!(odd["event"]["eventExtraDetail"]["workspace_kind"], "remote");
    assert_eq!(odd["event"]["eventExtraDetail"]["remote_kind"], "");
}

#[cfg(unix)]
mod dynamic_workflow {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use serde_json::Value;
    use zcode_host_control::{ControlSink, HostToMain, InitLocal};

    use crate::account::AccountStack;
    use crate::account::coding_plan::CodingPlanSubscription;
    use crate::agent::{AgentRuntime, AgentRuntimeConfig, RuntimePolicy, WorkspaceTarget};
    use crate::providers::ProviderRuntime;
    use crate::settings::SettingStore;
    use crate::{HostEnvironment, ServiceContext};

    struct NullSink;

    impl ControlSink for NullSink {
        fn send(&self, _message: HostToMain) {}
    }

    /// 逐行回 `{"id":N,"result":{}}` 并把收到的请求记进文件的假 CLI。
    fn fake_cli(dir: &Path, log: &Path) -> PathBuf {
        let script = dir.join("fake-cli.sh");
        std::fs::write(
            &script,
            format!(
                r#"#!/bin/sh
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

    /// 本地覆盖（`ZCODE_DYNAMIC_WORKFLOW_MODE`）决定门禁，不访问任何控制面。账号栈用自己的
    /// provider runtime：接入账号 overlay 后的读取要等首份账号快照（这里不启动账号栈）。
    fn plan(home: &Path, mode: &str) -> CodingPlanSubscription {
        let vars = BTreeMap::from([
            ("HOME".to_owned(), home.display().to_string()),
            ("ZCODE_DYNAMIC_WORKFLOW_MODE".to_owned(), mode.to_owned()),
            ("ZCODE_BASE_URL".to_owned(), "http://127.0.0.1:9".to_owned()),
            (
                "ZCODE_CREDENTIAL_SECRET".to_owned(),
                "integration".to_owned(),
            ),
        ]);
        let settings = SettingStore::new(&zcode_host_base::testing::spawner(), home);
        let providers = ProviderRuntime::new(
            &zcode_host_base::testing::spawner(),
            None,
            home.join("provider_config.json"),
        );
        let context = ServiceContext {
            environment: HostEnvironment::capture(vars).unwrap(),
            init: InitLocal::default(),
            control: Arc::new(NullSink),
        };
        let account = AccountStack::new(&context, &settings, &providers);
        CodingPlanSubscription::new(
            account.api.clone(),
            account.account_api.clone(),
            account.credentials,
            providers,
        )
    }

    async fn runtime_with_gate(dir: &Path, mode: &str) -> (AgentRuntime, PathBuf, WorkspaceTarget) {
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let log = dir.join("calls.log");
        let cli = fake_cli(dir, &log);
        let plan = plan(&dir.join("account"), mode);
        let settings = SettingStore::new(&zcode_host_base::testing::spawner(), dir);
        // 写入路径要求 provider 就绪：一个个人 API Key provider（不会发出请求）。
        let personal = dir.join("provider_config.json");
        std::fs::write(
            &personal,
            serde_json::json!({"schemaVersion": 1, "config": {
                "providerOrder": ["new-provider"],
                "providerConfigRules": {"providerRules": [{
                    "providerId": "new-provider", "providerName": "Mock",
                    "config": {
                        "group": "standard-personal",
                        "access": {"type": "api-key", "apiKey": "sk-test"},
                        "api": {"type": "openai-chat-completions", "baseUrl": "http://127.0.0.1:9/v1"},
                        "personalModelIds": ["my-model"], "modelOrder": ["my-model"],
                    },
                }]},
                "modelConfigRules": {
                    "providerModelRules": [{"providerId": "new-provider", "modelId": "my-model", "config": {"enabled": true}}],
                    "manualProviderModelRules": [],
                },
            }})
            .to_string(),
        )
        .unwrap();
        let builtin = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../../config/provider/zcode-builtin.json");
        let providers = ProviderRuntime::new(
            &zcode_host_base::testing::spawner(),
            Some(builtin),
            personal,
        );
        let runtime = AgentRuntime::new(
            AgentRuntimeConfig {
                base_env: BTreeMap::from([("PATH".to_owned(), "/usr/bin:/bin".to_owned())]),
                command_vars: BTreeMap::from([(
                    "ZCODE_AGENT_SERVER_COMMAND".to_owned(),
                    cli.display().to_string(),
                )]),
                fallback_cwd: None,
            },
            settings,
            providers,
        );
        runtime
            .extensions()
            .set_dynamic_workflow_gate(super::super::dynamic_workflow_gate(plan));
        (
            runtime,
            log,
            WorkspaceTarget::local(workspace.display().to_string()),
        )
    }

    #[tokio::test]
    async fn coding_plan_gate_reaches_policy_push_and_host_session_requests() {
        let dir = tempfile::tempdir().unwrap();
        let (runtime, log, target) = runtime_with_gate(dir.path(), "alwaysOn").await;
        let process = runtime
            .get_client(&target, RuntimePolicy::StartIfNeeded)
            .await
            .unwrap();
        let methods: Vec<String> = calls(&log).into_iter().map(|(method, _)| method).collect();
        assert!(
            methods.contains(&"workspace/updateDynamicWorkflowPolicy".to_owned()),
            "{methods:?}"
        );
        assert!(!methods.contains(&"workspace/updateOffPeakToolPolicy".to_owned()));
        // tasks 区域的 resumeTask 与 Node resumeSession 一样带工具面参数。
        crate::tasks::agent_calls::resume_session(&runtime, &target, "s1", None, None)
            .await
            .unwrap();
        let resume = calls(&log)
            .into_iter()
            .rfind(|(method, _)| method == "session/resume")
            .unwrap()
            .1;
        assert_eq!(resume["params"]["dynamicWorkflowEnabled"], true);
        assert!(resume["params"].get("offPeakToolEnabled").is_none());
        runtime.extensions().enable_off_peak_tools();
        let flags = runtime.extensions().session_tool_flags(&target).await;
        assert_eq!(
            Value::Object(flags),
            serde_json::json!({"offPeakToolEnabled": true, "dynamicWorkflowEnabled": true})
        );
        // 远端 identity 不带 Off-Peak，动态工作流照常（Node 不按 workspace 裁剪）。
        let remote = WorkspaceTarget {
            workspace_path: "/repo".into(),
            workspace_identity: Some("remote:ssh:h:22:u:/repo".into()),
            remote_session_id: None,
        };
        assert_eq!(
            Value::Object(runtime.extensions().session_tool_flags(&remote).await),
            serde_json::json!({"dynamicWorkflowEnabled": true})
        );
        process.client.close("dispose_workspace");
    }

    #[tokio::test]
    async fn disabled_gate_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (runtime, log, target) = runtime_with_gate(dir.path(), "disabled").await;
        let process = runtime
            .get_client(&target, RuntimePolicy::StartIfNeeded)
            .await
            .unwrap();
        crate::tasks::agent_calls::resume_session(&runtime, &target, "s1", None, None)
            .await
            .unwrap();
        let log = calls(&log);
        assert!(
            log.iter()
                .all(|(method, _)| method != "workspace/updateDynamicWorkflowPolicy")
        );
        let resume = log
            .iter()
            .rfind(|(method, _)| method == "session/resume")
            .unwrap();
        assert!(resume.1["params"].get("dynamicWorkflowEnabled").is_none());
        process.client.close("dispose_workspace");
    }
}
