//! `downloadZCodeBuiltinRelease`（packages/provider-node/src/zcode-builtin-download.ts）。
//!
//! 先读控制面 `/api/v1/client/configs` 拿 CDN 地址（只接受无凭据的 https URL），再下载 Release。
//! 总预算 20s 覆盖两次请求的正文；不跟随重定向；正文上限 10MB。错误只带阶段与原因，
//! 不把带 query 的 URL 或响应正文交给日志。
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;

use super::net::{ApiClient, ApiRequest};
use crate::providers::builtin::decode_release;

const BUDGET: Duration = Duration::from_secs(20);
const BODY_LIMIT: usize = 10_000_000;

enum Failure {
    Boundary(String),
    Schema(String),
    Invalid,
}

async fn read_json(api: &ApiClient, url: &str) -> Result<Value, Failure> {
    let response = api
        .request(ApiRequest::get(url).no_redirect())
        .await
        .map_err(|_| Failure::Invalid)?;
    if !response.ok() {
        return Err(Failure::Boundary(format!("HTTP {}", response.status)));
    }
    let mut stream = response.into_inner().bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| Failure::Invalid)?;
        body.extend_from_slice(&chunk);
        if body.len() > BODY_LIMIT {
            return Err(Failure::Boundary("body limit exceeded".into()));
        }
    }
    serde_json::from_slice(&body).map_err(|_| Failure::Invalid)
}

/// `clientConfigSchema`：返回 CDN 地址（未配置时 None）。
fn cdn_url(payload: &Value) -> Result<Option<String>, Failure> {
    if payload["code"] != 0 {
        return Err(Failure::Schema("code (invalid_value)".into()));
    }
    let configs = &payload["data"]["configs"];
    if !payload["data"].is_object() {
        return Err(Failure::Schema("data (invalid_type)".into()));
    }
    if !configs.is_object() {
        return Err(Failure::Schema("data.configs (invalid_type)".into()));
    }
    let Some(value) = configs.get("builtin_provider_config_json") else {
        return Ok(None);
    };
    let invalid = || Failure::Schema("data.configs.builtin_provider_config_json (custom)".into());
    let text = value.as_str().ok_or_else(invalid)?;
    let url = url::Url::parse(text).map_err(|_| invalid())?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return Err(invalid());
    }
    Ok(Some(text.to_owned()))
}

pub async fn download_release(
    api: &ApiClient,
    origin: &str,
    app_version: &str,
    platform: &str,
) -> Result<Option<Value>, String> {
    let at_cdn = AtomicBool::new(false);
    let stage = |at_cdn: &AtomicBool| {
        if at_cdn.load(Ordering::SeqCst) {
            "cdn"
        } else {
            "client-config"
        }
    };
    let work = async {
        let mut url = url::Url::parse(origin)
            .and_then(|base| base.join("/api/v1/client/configs"))
            .map_err(|_| Failure::Invalid)?;
        url.query_pairs_mut()
            .append_pair("app_version", app_version)
            .append_pair("platform", platform);
        let payload = read_json(api, url.as_str()).await?;
        let Some(download) = cdn_url(&payload)? else {
            return Ok(None);
        };
        at_cdn.store(true, Ordering::SeqCst);
        let release = read_json(api, &download).await?;
        decode_release(release.clone()).map_err(|_| Failure::Schema("root (custom)".into()))?;
        Ok(Some(release))
    };
    match tokio::time::timeout(BUDGET, work).await {
        Ok(Ok(release)) => Ok(release),
        Ok(Err(failure)) => {
            let reason = match failure {
                Failure::Boundary(reason) => reason,
                Failure::Schema(at) => format!("invalid schema at {at}"),
                Failure::Invalid => "invalid response".into(),
            };
            Err(format!("ZCode Built-in {}: {reason}", stage(&at_cdn)))
        }
        Err(_) => Err(format!("ZCode Built-in {}: timeout", stage(&at_cdn))),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::account::net::NetworkOptions;
    use crate::account::net::testing::serve;
    use crate::account::oauth_tests::endpoints_for;

    #[tokio::test]
    async fn reads_client_config_and_rejects_untrusted_cdn_urls() {
        let server = serve(Arc::new(|request| {
            let body = if request.target.starts_with("/api/v1/client/configs?app_version=1.2.3&platform=darwin-aarch64") {
                json!({"code": 0, "data": {"configs": {"builtin_provider_config_json": "http://cdn.invalid/x.json"}}})
            } else {
                json!({"code": 1})
            };
            (200, body.to_string())
        }))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let api = ApiClient::with_network(
            endpoints_for(&server.origin),
            dir.path().to_path_buf(),
            NetworkOptions::default(),
        );
        // 非 https 的 CDN 地址在下载前被拒绝（测试不访问任何外部地址）。
        let error = download_release(&api, &server.origin, "1.2.3", "darwin-aarch64")
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "ZCode Built-in client-config: invalid schema at data.configs.builtin_provider_config_json (custom)"
        );
        let error = download_release(&api, &server.origin, "0", "x")
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "ZCode Built-in client-config: invalid schema at code (invalid_value)"
        );
        assert_eq!(
            cdn_url(&json!({"code": 0, "data": {"configs": {}}})).ok(),
            Some(None)
        );
    }
}
