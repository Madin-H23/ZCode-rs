//! 合并前在几个测试文件里逐字复制的辅助代码，现在共用一份。
#![expect(clippy::disallowed_methods, reason = "集成测试从进程环境取被测二进制")]
use std::path::PathBuf;

use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// 调度器对执行中闲时任务数的上报：与请求无关、随时可能到达，读“下一条控制消息”时跳过。
pub(super) const UNSOLICITED_REPORT: &str = "off-peak-active-count";
/// 测试经 `--allowed-origin` 交给 Host 的 Renderer 源。
pub(super) const DEV_ORIGIN: &str = "http://localhost:5173";

pub(super) type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// runtime 的选择：缺省嵌入式（Host 的缺省链路）；`ZCODE_TEST_CLI_RUST_BINARY` 指向可执行文件时走子进程链路。
pub(super) fn runtime_env() -> Vec<(&'static str, String)> {
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
        // 不设置任何选择变量：走 Host 的缺省链路（嵌入式）。
        None => Vec::new(),
    }
}

/// 个人 provider 配置：一个指向本地 mock 的 OpenAI 兼容 provider。返回配置文件路径。
/// 用它的模块（`agent_cli`、`restart_settlement`）只在合并形态编译。
#[cfg(feature = "embedded-runtime")]
pub(super) fn provider_config(home: &std::path::Path, port: u16) -> PathBuf {
    let config_dir = home.join(".zcode/v2");
    std::fs::create_dir_all(&config_dir).unwrap();
    let personal = config_dir.join("provider_config.json");
    std::fs::write(
        &personal,
        serde_json::json!({"schemaVersion": 1, "config": {
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
