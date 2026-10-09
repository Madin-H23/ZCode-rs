//! `client-config` 与 `client-scenes` channel（packages/services/src/client-config/*、
//! client-scenes/*、packages/shared/src/clientConfig.ts、pluginStoreOrder.ts）。
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use serde_json::{Map, Value, json};
use zcode_host_kit::{Reply, RpcError, ServiceBuilder};

use super::credential::node_platform;
use super::net::{ApiClient, ApiRequest, node_arch};

const CACHE_TTL: Duration = Duration::from_secs(60 * 60);
const REQUEST_TIMEOUT_MS: u64 = 15_000;

/// `orderList`：trim 后非空的字符串数组，去重保序；任一元素非法则整体非法。
fn order_list(value: &Value) -> Option<Value> {
    let items = value.as_array()?;
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for item in items {
        let trimmed = item.as_str()?.trim();
        if trimmed.is_empty() {
            return None;
        }
        if seen.insert(trimmed.to_owned()) {
            result.push(json!(trimmed));
        }
    }
    Some(Value::Array(result))
}

/// `modeOrderSchema`：非法时整个模式回退（`.catch(undefined)`）。
fn mode_order(value: &Value) -> Option<Value> {
    let object = value.as_object()?;
    let mut parsed = Map::new();
    if let Some(categories) = object.get("categoryOrder") {
        parsed.insert("categoryOrder".into(), order_list(categories)?);
    }
    if let Some(plugins) = object.get("pluginOrder") {
        let mut map = Map::new();
        for (key, list) in plugins.as_object()? {
            map.insert(key.clone(), order_list(list)?);
        }
        parsed.insert("pluginOrder".into(), Value::Object(map));
    }
    Some(Value::Object(parsed))
}

/// `parsePluginStoreOrder`：对象之外为 null；code / work 各自独立回退。
pub fn parse_plugin_store_order(value: &Value) -> Value {
    let Some(object) = value.as_object() else {
        return Value::Null;
    };
    let mut parsed = Map::new();
    for key in ["code", "work"] {
        if let Some(mode) = object.get(key).and_then(mode_order) {
            parsed.insert(key.to_owned(), mode);
        }
    }
    Value::Object(parsed)
}

/// `parseClientConfigSnapshot`：只放开显式接入的公开字段。
pub fn parse_client_config_snapshot(payload: &Value) -> Result<Value, String> {
    let invalid = || "Invalid public client config response".to_owned();
    if payload["code"].as_f64() != Some(0.0) {
        return Err(invalid());
    }
    let data = &payload["data"];
    if !(data.is_null() || data.is_object()) {
        return Err(invalid());
    }
    let configs = &data["configs"];
    if !(configs.is_null() || configs.is_object()) {
        return Err(invalid());
    }
    Ok(json!({"pluginStoreOrder": parse_plugin_store_order(&configs["pluginStoreOrder"])}))
}

type SnapshotFuture = Shared<BoxFuture<'static, Result<Value, String>>>;

#[derive(Default)]
struct Entry {
    snapshot: Option<Value>,
    expires_at: Option<Instant>,
    pending: Option<SnapshotFuture>,
}

/// 窗口级公开配置读取，按 URL（含当前端点、版本、平台）缓存 1 小时。
#[derive(Clone)]
pub struct ClientConfig {
    api: ApiClient,
    entries: Arc<Mutex<HashMap<String, Entry>>>,
}

impl ClientConfig {
    pub fn new(api: ApiClient) -> Self {
        Self {
            api,
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn snapshot(&self, force_refresh: bool) -> Result<Value, String> {
        let endpoints = self.api.endpoints();
        let origin = endpoints.current_origin().await?;
        let mut url = url::Url::parse(&format!("{origin}/api/v1/client/configs"))
            .map_err(|error| error.to_string())?;
        url.query_pairs_mut()
            .append_pair("app_version", &endpoints.app_version())
            .append_pair("platform", &format!("{}-{}", node_platform(), node_arch()));
        let key = url.to_string();
        let future = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = entries.entry(key.clone()).or_default();
            if !force_refresh
                && let (Some(snapshot), Some(expires)) = (&entry.snapshot, entry.expires_at)
                && expires > Instant::now()
            {
                return Ok(snapshot.clone());
            }
            match &entry.pending {
                Some(pending) => pending.clone(),
                None => {
                    let this = self.clone();
                    let request_key = key.clone();
                    let future = async move {
                        let result = this.fetch(&request_key).await;
                        let mut entries = this
                            .entries
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let entry = entries.entry(request_key).or_default();
                        entry.pending = None;
                        if let Ok(snapshot) = &result {
                            entry.snapshot = Some(snapshot.clone());
                            entry.expires_at = Some(Instant::now() + CACHE_TTL);
                        }
                        result
                    }
                    .boxed()
                    .shared();
                    entry.pending = Some(future.clone());
                    future
                }
            }
        };
        future.await
    }

    async fn fetch(&self, url: &str) -> Result<Value, String> {
        let request = self.api.request(ApiRequest::get(url));
        let response = tokio::time::timeout(Duration::from_millis(REQUEST_TIMEOUT_MS), request)
            .await
            .map_err(|_| "Public client config request timed out".to_owned())?
            .map_err(|error| error.message)?;
        if !response.ok() {
            return Err(format!("Public client config HTTP {}", response.status));
        }
        // 设置里的端点可能在两次 await 之间变化（或被重定向）：不能把别处的响应缓存到这个地址。
        if response.url != url {
            return Err("Public client config request context changed".to_owned());
        }
        let body = response.text().await.map_err(|error| error.message)?;
        let payload: Value = serde_json::from_str(&body).map_err(|error| error.to_string())?;
        parse_client_config_snapshot(&payload)
    }
}

pub fn client_config_service(config: ClientConfig) -> ServiceBuilder {
    ServiceBuilder::new("client-config").method("getSnapshot", move |_, args| {
        let config = config.clone();
        async move {
            let options: Value = args.opt(0, "options")?.unwrap_or_else(|| json!({}));
            if !options.is_object()
                || !(options["forceRefresh"].is_null() || options["forceRefresh"].is_boolean())
            {
                return Err(RpcError::new(
                    "ZodError",
                    "invalid client config read options",
                ));
            }
            config
                .snapshot(options["forceRefresh"] == true)
                .await
                .map(Reply::Value)
                .map_err(|message| RpcError::new("Error", message))
        }
    })
}

pub fn client_scenes_service(api: ApiClient) -> ServiceBuilder {
    ServiceBuilder::new("client-scenes").method("list", move |_, _| {
        let api = api.clone();
        async move {
            // `ZCODE_CLIENT_SCENES_URL`：环境 origin（请求时再按设置覆盖改写）。
            let url = api.endpoints().zcode_api_url("/api/v1/client/scenes");
            api.read_json(ApiRequest::get(url))
                .await
                .map(Reply::Value)
                .map_err(|error| RpcError::new("ApiError", error.message))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_parsing_matches_zod_rules() {
        assert_eq!(
            parse_client_config_snapshot(
                &json!({"code": 0, "data": {"configs": {"pluginStoreOrder": {
                    "code": {"categoryOrder": [" a ", "a", "b"], "pluginOrder": {"x": ["p"]}},
                    "work": {"categoryOrder": [""]},
                    "extra": 1,
                }}}})
            )
            .unwrap(),
            json!({"pluginStoreOrder": {"code": {"categoryOrder": ["a", "b"], "pluginOrder": {"x": ["p"]}}}})
        );
        assert_eq!(
            parse_client_config_snapshot(&json!({"code": 0, "data": null})).unwrap(),
            json!({"pluginStoreOrder": null})
        );
        assert!(parse_client_config_snapshot(&json!({"code": 1})).is_err());
    }
}
