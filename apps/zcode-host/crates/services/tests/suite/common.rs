//! 合并前在几个测试文件里逐字复制的辅助代码，现在共用一份。
#![expect(
    clippy::disallowed_methods,
    reason = "集成测试从进程环境取被测二进制与 PATH"
)]
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use zcode_host_control::{ControlSink, HostToMain};

/// 把 Host 发给 Main 的控制消息以 JSON 交给测试。
pub(super) struct Control(pub(super) mpsc::UnboundedSender<Value>);

impl ControlSink for Control {
    fn send(&self, message: HostToMain) {
        let _ = self.0.send(serde_json::to_value(&message).unwrap());
    }
}

/// 丢弃全部控制消息。
pub(super) struct NullSink;

impl ControlSink for NullSink {
    fn send(&self, _message: HostToMain) {}
}

/// runtime 的选择：缺省嵌入式（Host 的缺省链路）；`ZCODE_TEST_CLI_RUST_BINARY` 指向可执行文件时走子进程链路。
pub(super) fn runtime_vars() -> BTreeMap<String, String> {
    match std::env::var_os("ZCODE_TEST_CLI_RUST_BINARY").map(PathBuf::from) {
        Some(cli) => {
            assert!(
                cli.is_file(),
                "ZCODE_TEST_CLI_RUST_BINARY is not a file: {}",
                cli.display()
            );
            // 缺陷修复：这里原来递归调用 runtime_vars() 自己，设置该变量时栈溢出，子进程链路的差分
            // 对照从未真正跑过；改为与 agent_cli.rs 相同的显式命令与 runtime 种类。
            BTreeMap::from([
                (
                    "ZCODE_AGENT_SERVER_COMMAND".to_owned(),
                    cli.to_string_lossy().into_owned(),
                ),
                (
                    "ZCODE_AGENT_SERVER_RUNTIME".to_owned(),
                    "zcode-cli-rust".to_owned(),
                ),
            ])
        }
        // 不设置任何选择变量：走 Host 的缺省链路（嵌入式）。
        None => BTreeMap::new(),
    }
}

/// 个人 provider 配置：一个指向本地 mock 的 OpenAI 兼容 provider。返回配置文件路径。
pub(super) fn write_provider_config(home: &Path, port: u16) -> PathBuf {
    let config_dir = home.join(".zcode/v2");
    std::fs::create_dir_all(&config_dir).unwrap();
    let personal = config_dir.join("provider_config.json");
    std::fs::write(
        &personal,
        json!({"schemaVersion": 1, "config": {
            "providerOrder": ["new-provider"],
            "providerConfigRules": {"providerRules": [{
                "providerId": "new-provider", "providerName": "Mock",
                "config": {
                    "group": "standard-personal",
                    "access": {"type": "api-key", "apiKey": "sk-test"},
                    "api": {"type": "openai-chat-completions", "baseUrl": format!("http://127.0.0.1:{port}/v1")},
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
    personal
}

/// 一段 Chat Completions 流式 chunk。
pub(super) fn chunk(delta: Value, finish: Value) -> Value {
    json!({
        "id": "mock", "object": "chat.completion.chunk", "created": 0, "model": "my-model",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    })
}

pub(super) fn jwt(exp: i64) -> String {
    let encode =
        |value: Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string());
    format!(
        "{}.{}.sig",
        encode(json!({"alg": "none"})),
        encode(json!({"exp": exp}))
    )
}

pub(super) fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

pub(super) fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../..")
        .canonicalize()
        .unwrap()
}

pub(super) fn find_node() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("node"))
            .find(|candidate| candidate.is_file())
    })
}

/// 本仓库构建的远端 server（`pnpm --filter @zcode/server build:remote`）。
pub(super) fn server_bundle() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../packages/server/dist/remote/zcode-server.cjs")
}
