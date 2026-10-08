//! 端点解析（packages/shared/src/zcodeEndpoint.ts、oauth/providers/configUtils.ts）。
//!
//! Main 已把 ZCODE_ENV、ZCODE_BASE_URL、ZAI_* 等解析后放进 Host 环境；Rust Host 只读环境，
//! 再叠加设置页的 `zcodeEndpointOrigin` 覆盖（每次调用重新读取设置）。
use std::collections::BTreeMap;

use url::Url;

use crate::settings::SettingStore;

pub const DEFAULT_ZCODE_ENDPOINT_ORIGIN: &str = "https://zcode.z.ai";
pub const DEFAULT_BIGMODEL_API_ORIGIN: &str = "https://bigmodel.cn";
pub const DEFAULT_ZAI_OAUTH_ORIGIN: &str = "https://chat.z.ai";
pub const DEFAULT_ZAI_BUSINESS_BASE_URL: &str = "https://api.z.ai";
pub const DEFAULT_ZAI_OAUTH_CLIENT_ID: &str = "client_P8X5CMWmlaRO9gyO-KSqtg";
const DESKTOP_OAUTH_CALLBACK_URI: &str = "zcode://oauth/callback";

/// `normalizeZCodeEndpointOrigin`：返回 URL 的 origin，只接受 http / https。
pub fn normalize_origin(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("ZCode endpoint origin is empty".to_owned());
    }
    let url = Url::parse(trimmed).map_err(|_| "Invalid URL".to_owned())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("ZCode endpoint origin must use http or https".to_owned());
    }
    Ok(url.origin().ascii_serialization())
}

/// URL 的 origin（`new URL(x).origin`）；解析失败返回 None。
pub fn origin_of(url: &str) -> Option<String> {
    Url::parse(url)
        .ok()
        .map(|url| url.origin().ascii_serialization())
}

/// `rewriteZCodeEndpointUrl`：默认 origin 的 URL 改写到当前 origin，保留路径、查询与片段。
pub fn rewrite_url(input: &str, endpoint_origin: &str) -> String {
    let Ok(parsed) = Url::parse(input) else {
        return input.to_owned();
    };
    if parsed.origin().ascii_serialization() != DEFAULT_ZCODE_ENDPOINT_ORIGIN {
        return input.to_owned();
    }
    let Ok(target_origin) = normalize_origin(endpoint_origin) else {
        return input.to_owned();
    };
    if target_origin == DEFAULT_ZCODE_ENDPOINT_ORIGIN {
        return input.to_owned();
    }
    let Ok(mut target) = Url::parse(&target_origin) else {
        return input.to_owned();
    };
    target.set_path(parsed.path());
    target.set_query(parsed.query());
    target.set_fragment(parsed.fragment());
    target.to_string()
}

fn join_path(origin: &str, path: &str) -> String {
    if path.starts_with('/') {
        format!("{origin}{path}")
    } else {
        format!("{origin}/{path}")
    }
}

/// Host 环境里的产品端点配置，加上设置页覆盖。
#[derive(Clone)]
pub struct Endpoints {
    vars: BTreeMap<String, String>,
    settings: Option<SettingStore>,
}

impl Endpoints {
    pub fn new(vars: BTreeMap<String, String>, settings: Option<SettingStore>) -> Self {
        Self { vars, settings }
    }

    /// 只读环境（不带设置覆盖），测试与无设置场景用。
    pub fn from_env(vars: BTreeMap<String, String>) -> Self {
        Self::new(vars, None)
    }

    pub fn vars(&self) -> &BTreeMap<String, String> {
        &self.vars
    }

    /// 非空（trim 后）的环境变量。
    pub fn env(&self, key: &str) -> Option<&str> {
        self.vars
            .get(key)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
    }

    /// `resolveRuntimeZCodeEnv`：只有 `test` 是测试环境。
    pub fn zcode_env(&self) -> &'static str {
        if self
            .env("ZCODE_ENV")
            .is_some_and(|value| value.eq_ignore_ascii_case("test"))
        {
            "test"
        } else {
            "production"
        }
    }

    /// 编译期 `ZCODE_ENV` 的规范化（`normalizeZCodeEnv`）：用作发布渠道头。
    pub fn release_channel(&self) -> &'static str {
        if self
            .env("ZCODE_ENV")
            .is_some_and(|value| value.eq_ignore_ascii_case("production"))
        {
            "production"
        } else {
            "test"
        }
    }

    /// App 版本：Main 下发的 `ZCODE_APP_VERSION`（Node Host 用编译期常量，值相同）。
    pub fn app_version(&self) -> String {
        self.env("ZCODE_APP_VERSION")
            .unwrap_or("0.0.0-dev")
            .to_owned()
    }

    /// `resolveRuntimeZCodeEndpointOrigin(env)`：不含设置覆盖。
    pub fn env_origin(&self) -> Result<String, String> {
        match self
            .env("ZCODE_BASE_URL")
            .or_else(|| self.env("ZCODE_ENDPOINT_ORIGIN"))
        {
            Some(origin) => normalize_origin(origin),
            None => Ok(DEFAULT_ZCODE_ENDPOINT_ORIGIN.to_owned()),
        }
    }

    /// 设置页的 `zcodeEndpointOrigin`（trim 后非空）。
    pub async fn override_origin(&self) -> Option<String> {
        let settings = self.settings.as_ref()?.get().await;
        settings["zcodeEndpointOrigin"]
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    }

    /// 当前生效的 origin：设置覆盖优先，其次环境（`resolveCurrentZCodeEndpointOrigin`）。
    pub async fn current_origin(&self) -> Result<String, String> {
        match self.override_origin().await {
            Some(origin) => normalize_origin(&origin),
            None => self.env_origin(),
        }
    }

    /// `buildRuntimeZCodeApiUrl(env, path)`：环境 origin + path（请求时再按当前 origin 改写）。
    pub fn zcode_api_url(&self, path: &str) -> String {
        let origin = self
            .env_origin()
            .unwrap_or_else(|_| DEFAULT_ZCODE_ENDPOINT_ORIGIN.to_owned());
        join_path(&origin, path)
    }

    fn origin_env(&self, key: &str, default: &str) -> String {
        self.env(key)
            .and_then(|value| normalize_origin(value).ok())
            .unwrap_or_else(|| default.to_owned())
    }

    pub fn bigmodel_api_origin(&self) -> String {
        self.origin_env("BIGMODEL_API_BASE_URL", DEFAULT_BIGMODEL_API_ORIGIN)
    }

    pub fn bigmodel_api_url(&self, path: &str) -> String {
        join_path(&self.bigmodel_api_origin(), path)
    }

    pub fn zai_oauth_origin(&self) -> String {
        self.origin_env("ZAI_OAUTH_ORIGIN", DEFAULT_ZAI_OAUTH_ORIGIN)
    }

    pub fn zai_oauth_url(&self, path: &str) -> String {
        join_path(&self.zai_oauth_origin(), path)
    }

    pub fn zai_business_base_url(&self) -> String {
        self.origin_env("ZAI_BUSINESS_BASE_URL", DEFAULT_ZAI_BUSINESS_BASE_URL)
    }

    pub fn zai_business_url(&self, path: &str) -> String {
        join_path(&self.zai_business_base_url(), path)
    }

    pub fn zai_oauth_client_id(&self) -> String {
        self.env("ZAI_OAUTH_CLIENT_ID")
            .or_else(|| self.env("ZAI_OAUTH_APP_ID"))
            .unwrap_or(DEFAULT_ZAI_OAUTH_CLIENT_ID)
            .to_owned()
    }

    /// `buildDesktopOAuthRedirectUriFromEnv`：官网中转页，再透传到 `zcode://oauth/callback`。
    pub fn desktop_oauth_redirect_uri(&self) -> String {
        let origin = self
            .env_origin()
            .unwrap_or_else(|_| DEFAULT_ZCODE_ENDPOINT_ORIGIN.to_owned());
        let mut url = Url::parse(&origin)
            .and_then(|origin| origin.join("/app/oauth/login"))
            .unwrap_or_else(|_| {
                Url::parse("https://zcode.z.ai/app/oauth/login").expect("static url")
            });
        url.query_pairs_mut()
            .append_pair("redirect", DESKTOP_OAUTH_CALLBACK_URI)
            .append_pair("app_version", &self.app_version());
        url.to_string()
    }

    /// `ZCodeEndpointUrls`（buildZCodeEndpointUrls）。
    pub fn endpoint_urls(origin: &str) -> serde_json::Value {
        serde_json::json!({
            "origin": origin,
            "apiBaseUrl": format!("{origin}/api/v1"),
            "webShareCallbackUrl": format!("{origin}/cn/share/callback"),
            "zcodePlanOpenAiBaseUrl": format!("{origin}/api/v1/zcode-plan"),
            "zcodePlanAnthropicBaseUrl": format!("{origin}/api/v1/zcode-plan/anthropic"),
            "zcodePlanBillingCurrentUrl": format!("{origin}/api/v1/zcode-plan/billing/current"),
            "zcodePlanBillingBalanceUrl": format!("{origin}/api/v1/zcode-plan/billing/balance"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoints(pairs: &[(&str, &str)]) -> Endpoints {
        Endpoints::from_env(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    #[test]
    fn env_origin_priority_and_validation() {
        assert_eq!(
            endpoints(&[]).env_origin().unwrap(),
            DEFAULT_ZCODE_ENDPOINT_ORIGIN
        );
        let env = endpoints(&[
            ("ZCODE_BASE_URL", " http://127.0.0.1:9/x/y "),
            ("ZCODE_ENDPOINT_ORIGIN", "https://other"),
        ]);
        assert_eq!(env.env_origin().unwrap(), "http://127.0.0.1:9");
        assert_eq!(
            env.zcode_api_url("/api/v1/oauth/cli/init"),
            "http://127.0.0.1:9/api/v1/oauth/cli/init"
        );
        assert!(
            endpoints(&[("ZCODE_BASE_URL", "ftp://x")])
                .env_origin()
                .is_err()
        );
        assert_eq!(
            endpoints(&[("ZCODE_ENDPOINT_ORIGIN", "https://a.example:443/")])
                .env_origin()
                .unwrap(),
            "https://a.example"
        );
    }

    #[test]
    fn rewrite_only_default_origin() {
        assert_eq!(
            rewrite_url("https://zcode.z.ai/api/v1/x?a=1#f", "http://127.0.0.1:8080"),
            "http://127.0.0.1:8080/api/v1/x?a=1#f"
        );
        assert_eq!(
            rewrite_url("https://bigmodel.cn/x", "http://127.0.0.1:8080"),
            "https://bigmodel.cn/x"
        );
        assert_eq!(
            rewrite_url("https://zcode.z.ai/x", DEFAULT_ZCODE_ENDPOINT_ORIGIN),
            "https://zcode.z.ai/x"
        );
    }

    #[test]
    fn product_urls_follow_env() {
        let env = endpoints(&[
            ("ZAI_OAUTH_APP_ID", "app"),
            ("BIGMODEL_API_BASE_URL", "http://127.0.0.1:1/"),
            ("ZCODE_APP_VERSION", "1.2.3"),
        ]);
        assert_eq!(env.zai_oauth_client_id(), "app");
        assert_eq!(env.bigmodel_api_url("login"), "http://127.0.0.1:1/login");
        assert_eq!(
            env.desktop_oauth_redirect_uri(),
            "https://zcode.z.ai/app/oauth/login?redirect=zcode%3A%2F%2Foauth%2Fcallback&app_version=1.2.3"
        );
        assert_eq!(env.zai_business_base_url(), DEFAULT_ZAI_BUSINESS_BASE_URL);
        assert_eq!(env.zcode_env(), "production");
        assert_eq!(env.release_channel(), "test");
    }
}
