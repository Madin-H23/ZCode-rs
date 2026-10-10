//! Rust Host 写出的 provider_config.json 必须是 TS 的规范形态：真实的 TS codec 能解码，
//! 重新编码后与文件逐字节相同（scripts/rust-host/provider-config-interop.ts）。没有 node_modules 时跳过。
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use zcode_host_services::providers::ProviderRuntime;
use zcode_host_services::providers::personal::PersonalWriter;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../..")
        .canonicalize()
        .unwrap()
}

fn check(root: &Path, file: &Path) -> Value {
    let output = zcode_proc::std_command("node")
        .current_dir(root)
        .args([
            "--import",
            "tsx",
            "scripts/rust-host/provider-config-interop.ts",
        ])
        .arg(file)
        .output()
        .expect("run node");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn personal_writes_are_canonical_for_typescript() {
    let root = repo_root();
    if !root.join("node_modules/.bin/tsx").exists() {
        eprintln!("skipped: node_modules/.bin/tsx missing (run pnpm install)");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let personal = dir.path().join("provider_config.json");
    let builtin = root.join("config/provider/zcode-builtin.json");
    let runtime = ProviderRuntime::new(
        &zcode_host_services::testing::spawner(),
        Some(builtin),
        personal.clone(),
    );
    let writer = PersonalWriter::new(runtime.clone());
    writer
        .create_provider(&json!({"providerName": "Mine", "initialConfig": {
            "access": {"type": "api-key", "apiKey": "sk-1"},
            "api": {"baseUrl": "https://api.example.com/v1", "type": "openai-chat-completions", "headers": {"X-A": "1"}},
        }}))
        .await
        .unwrap();
    writer
        .add_model(
            "new-provider",
            "m-smart",
            &json!({"optionSpecs": {"maxOutputTokens": {"max": 4096}}}),
            None,
        )
        .await
        .unwrap();
    let manual = json!({
        "properties": {
            "supportsNativeWebSearch": false, "contextWindow": 128000, "supportsJsonSchemaOutput": true,
            "supportsMidConversationSystem": false,
            "inputFormat": {"supportsPdf": false, "supportsImage": true, "supportsVideo": false},
        },
        "optionSpecs": {
            "maxOutputTokens": {"max": 8192},
            "reasoningLevel": {"map": "{\"thinking\": reasoningLevel == \"enabled\"}", "values": ["disabled", "enabled"]},
        },
    });
    writer
        .add_model("new-provider", "m-manual", &manual, Some(false))
        .await
        .unwrap();
    writer
        .save_provider_overlay(
            "account:zai-start-plan",
            &json!({"modelOrder": ["GLM-5.2"]}),
            Some(&json!({"providerName": "Start"})),
        )
        .await
        .unwrap();
    writer
        .create_provider(&json!({"templateId": "deepseek"}))
        .await
        .ok();
    let result = check(&root, &personal);
    assert_eq!(
        result,
        json!({"decoded": true, "canonical": true}),
        "{}",
        std::fs::read_to_string(&personal).unwrap()
    );
}
