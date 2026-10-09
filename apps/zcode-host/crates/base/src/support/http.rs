//! 外部集成（bots、conversation-share、闲时任务服务端）共用的出站 HTTP 客户端。
//!
//! 与 CLI 相同：reqwest + rustls（ring）。不读取 shell 的 `HTTP(S)_PROXY`；不自动跟随重定向
//! 由调用方按需打开。测试只连 127.0.0.1 上的 mock 服务。
use std::time::Duration;

use anyhow::Context as _;

/// 构建客户端；`timeout` 为整个请求的上限。
pub fn client(timeout: Duration) -> anyhow::Result<reqwest::Client> {
    // rustls-no-provider 不自动选择算法；与 CLI 一致统一使用 ring（重复安装是无害的空操作）。
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(15))
        .timeout(timeout)
        .build()
        .context("build http client")
}

pub const DEFAULT_ZCODE_ENDPOINT_ORIGIN: &str = "https://zcode.z.ai";

/// `resolveRuntimeZCodeEndpointOrigin`：`ZCODE_BASE_URL` 或 `ZCODE_ENDPOINT_ORIGIN` 的 origin，
/// 缺省为正式服务。非法值与 TS 一样报错（`normalizeZCodeEndpointOrigin` 抛错）。
pub fn zcode_api_origin(
    vars: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<String> {
    let configured = ["ZCODE_BASE_URL", "ZCODE_ENDPOINT_ORIGIN"]
        .iter()
        .find_map(|key| {
            vars.get(*key)
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
        });
    match configured {
        None => Ok(DEFAULT_ZCODE_ENDPOINT_ORIGIN.to_owned()),
        Some(value) => endpoint_origin(value)
            .ok_or_else(|| anyhow::anyhow!("ZCode endpoint origin must use http or https")),
    }
}

/// `normalizeZCodeEndpointOrigin`：http(s) URL 的 origin。
pub fn endpoint_origin(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let url = url::Url::parse(trimmed).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.origin().ascii_serialization())
}
