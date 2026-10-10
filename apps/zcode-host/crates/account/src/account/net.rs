//! Host 的网络客户端（packages/services/src/providers/api/nodeApiClient.ts、nodeApiNetwork.ts、
//! apiJson.ts、sourceHeaders.ts）。
//!
//! - 端点：请求 URL 的 origin 为默认 `https://zcode.z.ai` 时改写到当前 origin（设置覆盖优先）；
//! - 发往当前 origin 的请求先放 ZCode 来源头，再叠加调用方的头；每个请求补 `x-request-id`；
//! - 代理、No Proxy、CA 只来自设置页，首次请求时读取一次（Node `optionsPromise` 同样只读一次，
//!   读取失败不缓存），不读 shell 的 `HTTP(S)_PROXY`；
//! - 响应 401 时交给登录态处理器判断是否为当前登录凭据（`isCurrentOAuthCredentialRequest`）。
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde_json::Value;
use url::Url;
use zcode_host_kit::log;

use super::endpoint::{DEFAULT_ZCODE_ENDPOINT_ORIGIN, Endpoints, normalize_origin, rewrite_url};
use crate::settings::SettingStore;

const REQUEST_ID_HEADER: &str = "x-request-id";
const DIAGNOSTIC_RESPONSE_HEADERS: [&str; 3] = ["x-request-id", "x-trace-id", "x-span-id"];
/// undici fetch 的默认重定向上限。
const MAX_REDIRECTS: usize = 20;

/// `ApiError`（packages/shared/src/api.ts）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub message: String,
    pub url: String,
    pub method: String,
    pub status: Option<u16>,
    pub response_headers: Option<BTreeMap<String, String>>,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    pub fn new(message: impl Into<String>, url: &str, method: &str) -> Self {
        Self {
            message: message.into(),
            url: url.to_owned(),
            method: method.to_owned(),
            status: None,
            response_headers: None,
        }
    }
}

/// 设置页的网络选项（`HostApiNetworkOptions`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkOptions {
    pub http_proxy: Option<String>,
    pub no_proxy: Option<String>,
    pub ca_cert_path: Option<String>,
}

impl NetworkOptions {
    fn from_settings(settings: &Value) -> Self {
        let text = |key: &str| settings[key].as_str().map(str::to_owned);
        Self {
            http_proxy: text("httpProxy"),
            no_proxy: text("httpProxyNoProxy"),
            // Node 用 `options.caCertPath ?? ""` 作 key、按真值判断是否加载：空串等于未配置。
            ca_cert_path: text("httpProxyCaCertPath").filter(|path| !path.is_empty()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyRoute {
    Direct { no_proxy_matched: bool },
    Proxy(String),
    Invalid(&'static str),
}

/// `normalizeProxyUrl`：无 scheme 补 `http://`；只接受带 host 的 http / https。
fn normalize_proxy_url(value: &str) -> Option<String> {
    let has_scheme = value.split_once("://").is_some_and(|(scheme, _)| {
        let mut chars = scheme.chars();
        chars
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '.' | '-'))
    });
    let candidate = if has_scheme {
        value.to_owned()
    } else {
        format!("http://{value}")
    };
    let url = Url::parse(&candidate).ok()?;
    if url.host_str().is_none_or(str::is_empty) || !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    Some(url.to_string())
}

/// `matchesNoProxy`。
fn matches_no_proxy(url: &Url, value: Option<&str>) -> bool {
    let host = url.host_str().unwrap_or("").to_lowercase();
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port = url
        .port()
        .map(|port| port.to_string())
        .unwrap_or_else(|| if url.scheme() == "https" { "443" } else { "80" }.to_owned());
    value
        .unwrap_or("")
        .split(|c: char| c == ',' || c.is_whitespace())
        .any(|raw| {
            let rule = raw.trim().to_lowercase();
            if rule.is_empty() {
                return false;
            }
            if rule == "*" {
                return true;
            }
            let without_scheme = match rule.split_once("://") {
                Some((scheme, rest))
                    if scheme
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_lowercase())
                        && scheme.chars().all(|c| {
                            c.is_ascii_lowercase()
                                || c.is_ascii_digit()
                                || matches!(c, '+' | '.' | '-')
                        }) =>
                {
                    rest.to_owned()
                }
                _ => rule.clone(),
            };
            let mut parts = without_scheme.split(':');
            let rule_host = parts.next().unwrap_or("");
            let rule_port = parts.next();
            let normalized = rule_host.strip_prefix('*').unwrap_or(rule_host);
            let normalized = normalized.strip_prefix('.').unwrap_or(normalized);
            if normalized.is_empty() {
                return false;
            }
            let host_matches = host == normalized || host.ends_with(&format!(".{normalized}"));
            host_matches
                && rule_port.is_none_or(|rule_port| rule_port.is_empty() || rule_port == port)
        })
}

/// `resolveHostProxyForUrl`。
pub fn resolve_proxy(request_url: &str, options: &NetworkOptions) -> ProxyRoute {
    let Ok(url) = Url::parse(request_url) else {
        return ProxyRoute::Direct {
            no_proxy_matched: false,
        };
    };
    if !matches!(url.scheme(), "http" | "https") {
        return ProxyRoute::Direct {
            no_proxy_matched: false,
        };
    }
    if matches_no_proxy(&url, options.no_proxy.as_deref()) {
        return ProxyRoute::Direct {
            no_proxy_matched: true,
        };
    }
    let Some(configured) = options
        .http_proxy
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return ProxyRoute::Direct {
            no_proxy_matched: false,
        };
    };
    match normalize_proxy_url(configured) {
        Some(proxy) => ProxyRoute::Proxy(proxy),
        None => ProxyRoute::Invalid("Configured Host proxy URL is invalid"),
    }
}

/// Node `normalizePrintableHeaderValue`。
pub fn printable(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty() && v.bytes().all(|b| (0x20..=0x7e).contains(&b)))
        .map(str::to_owned)
}

/// ICU 默认 locale（LC_ALL → LC_MESSAGES → LANG），同 CLI net crate。
pub fn client_language(vars: &BTreeMap<String, String>) -> String {
    let Some(raw) = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .find_map(|key| vars.get(key))
    else {
        return "en-US".into();
    };
    let (locale, modifier) = match raw.split_once('@') {
        Some((locale, modifier)) => (locale, Some(modifier)),
        None => (raw.as_str(), None),
    };
    let locale = locale.split('.').next().unwrap_or("");
    match locale {
        "C" | "POSIX" => return "en-US".into(),
        "" => return "und".into(),
        _ => {}
    }
    let mut tag = locale.replace('_', "-");
    if let Some(modifier) = modifier.filter(|m| !m.is_empty()) {
        tag.push('-');
        tag.push_str(modifier);
    }
    tag
}

/// `Intl.DateTimeFormat().resolvedOptions().timeZone`。
pub fn client_timezone(vars: &BTreeMap<String, String>) -> Option<String> {
    match vars.get("TZ") {
        None => iana_time_zone::get_timezone().ok(),
        Some(tz) => {
            let tz = tz.strip_prefix(':').unwrap_or(tz);
            if tz.is_empty() {
                return Some("Etc/Unknown".into());
            }
            let known = tz.eq_ignore_ascii_case("UTC")
                || (!tz.contains("..")
                    && std::path::Path::new("/usr/share/zoneinfo")
                        .join(tz)
                        .is_file());
            known.then(|| tz.to_owned())
        }
    }
}

/// `process.arch`。
pub fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        "powerpc64" => "ppc64",
        other => other,
    }
}

/// `os.version()`：POSIX 为 `uname -v`。
#[cfg(unix)]
pub fn os_version() -> Option<String> {
    #[allow(unsafe_code)]
    // SAFETY: uname 只写入调用方提供的结构体；失败时不读取其内容。
    let name = unsafe {
        let mut name: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut name) != 0 {
            return None;
        }
        name
    };
    let bytes: Vec<u8> = name
        .version
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8(bytes).ok()
}

/// Windows 的 `os.version()` 来自注册表产品名，Rust Host 不读注册表，省略该头。
#[cfg(not(unix))]
pub fn os_version() -> Option<String> {
    None
}

/// ZCode 来源头（`buildZCodeSourceHeaders` + `buildZCodeSourceHeadersFromContext`）。
#[derive(Clone)]
pub struct SourceHeaders {
    endpoints: Endpoints,
    telemetry_state_file: PathBuf,
    device_mid: Arc<Mutex<Option<String>>>,
}

impl SourceHeaders {
    pub fn new(endpoints: Endpoints, app_config_dir: PathBuf) -> Self {
        Self {
            endpoints,
            telemetry_state_file: app_config_dir.join("telemetry-state.json"),
            device_mid: Arc::new(Mutex::new(None)),
        }
    }

    /// 只复用已存在的 deviceMid（生命周期归 desktop / telemetry），读到后缓存。
    fn device_mid(&self) -> Option<String> {
        let mut cached = self
            .device_mid
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cached.is_none() {
            *cached = std::fs::read_to_string(&self.telemetry_state_file)
                .ok()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .and_then(|value| printable(value["deviceMid"].as_str()));
        }
        cached.clone()
    }

    /// 有序的头列表；`HTTP-Referer` 为默认 origin，调用方按需改写。
    pub fn build(&self) -> Vec<(String, String)> {
        let vars = self.endpoints.vars();
        let version = printable(Some(&self.endpoints.app_version()));
        let platform = super::credential::node_platform();
        let arch = node_arch();
        let mut headers = vec![
            (
                "User-Agent".to_owned(),
                format!("ZCode/{}", version.as_deref().unwrap_or("unknown")),
            ),
            (
                "HTTP-Referer".to_owned(),
                DEFAULT_ZCODE_ENDPOINT_ORIGIN.to_owned(),
            ),
            ("X-Title".to_owned(), "Z Code@electron".to_owned()),
        ];
        if let Some(version) = &version {
            headers.push(("X-ZCode-App-Version".to_owned(), version.clone()));
        }
        headers.push(("X-Platform".to_owned(), format!("{platform}-{arch}")));
        headers.push((
            "X-Release-Channel".to_owned(),
            self.endpoints.release_channel().to_owned(),
        ));
        headers.push((
            "X-Client-Language".to_owned(),
            printable(Some(&client_language(vars))).unwrap_or_else(|| "unknown".into()),
        ));
        headers.push((
            "X-Client-Timezone".to_owned(),
            printable(client_timezone(vars).as_deref()).unwrap_or_else(|| "unknown".into()),
        ));
        let category = match platform {
            "darwin" => "macos",
            "win32" => "windows",
            _ => "linux",
        };
        headers.push(("X-Os-Category".to_owned(), category.to_owned()));
        if let Some(version) = printable(os_version().as_deref()) {
            headers.push(("X-Os-Version".to_owned(), version));
        }
        if let Some(mid) = self.device_mid() {
            headers.push(("X-Device-Mid".to_owned(), mid));
        }
        headers
    }
}

/// 名字大小写不敏感、后写覆盖先写的头列表（WHATWG `Headers.set`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeaderList(Vec<(String, String)>);

impl HeaderList {
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        let value = value.into();
        if let Some(entry) = self
            .0
            .iter_mut()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(&name))
        {
            entry.1 = value;
        } else {
            self.0.push((name, value));
        }
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    /// 排序后的小写键名（只用于日志，不记录值）。
    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.0.iter().map(|(name, _)| name.to_lowercase()).collect();
        keys.sort();
        keys
    }
}

impl<K: Into<String>, V: Into<String>> FromIterator<(K, V)> for HeaderList {
    fn from_iter<T: IntoIterator<Item = (K, V)>>(iter: T) -> Self {
        let mut list = HeaderList::default();
        for (name, value) in iter {
            list.set(name, value);
        }
        list
    }
}

pub enum ApiBody {
    Empty,
    Bytes(Vec<u8>),
    /// 流式请求体（上传进度等）。
    Stream(reqwest::Body),
}

/// 一次请求（`ApiRequestInit`）。
pub struct ApiRequest {
    pub method: String,
    pub url: String,
    pub headers: HeaderList,
    pub body: ApiBody,
    pub timeout_ms: Option<u64>,
    /// `redirect: "error"`：不跟随重定向（3xx 原样返回，调用方按非 2xx 处理）。
    pub follow_redirects: bool,
}

impl ApiRequest {
    pub fn new(method: &str, url: impl Into<String>) -> Self {
        Self {
            method: method.to_uppercase(),
            url: url.into(),
            headers: HeaderList::default(),
            body: ApiBody::Empty,
            timeout_ms: None,
            follow_redirects: true,
        }
    }

    pub fn get(url: impl Into<String>) -> Self {
        Self::new("GET", url)
    }

    pub fn post(url: impl Into<String>) -> Self {
        Self::new("POST", url)
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.set(name, value);
        self
    }

    pub fn bearer(self, token: &str) -> Self {
        self.header("Authorization", format!("Bearer {token}"))
    }

    /// JSON 请求体：`JSON.stringify(value)` 与 `Content-Type: application/json`。
    pub fn json(mut self, value: &Value) -> Self {
        self.headers.set("Content-Type", "application/json");
        self.body = ApiBody::Bytes(value.to_string().into_bytes());
        self
    }

    pub fn body(mut self, body: ApiBody) -> Self {
        self.body = body;
        self
    }

    pub fn timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }

    pub fn no_redirect(mut self) -> Self {
        self.follow_redirects = false;
        self
    }
}

pub struct ApiResponse {
    pub status: u16,
    pub url: String,
    pub method: String,
    pub headers: HeaderList,
    response: reqwest::Response,
}

impl ApiResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)
    }

    pub async fn bytes(self) -> Result<Vec<u8>, ApiError> {
        let (url, method) = (self.url.clone(), self.method.clone());
        self.response
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|error| ApiError::new(error_message(&error), &url, &method))
    }

    pub async fn text(self) -> Result<String, ApiError> {
        let (url, method) = (self.url.clone(), self.method.clone());
        let bytes = self.bytes().await;
        bytes
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .map_err(|error| ApiError::new(error.message, &url, &method))
    }

    pub fn into_inner(self) -> reqwest::Response {
        self.response
    }

    fn diagnostic_headers(&self) -> Option<BTreeMap<String, String>> {
        let headers: BTreeMap<String, String> = DIAGNOSTIC_RESPONSE_HEADERS
            .iter()
            .filter_map(|name| {
                self.header(name)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|value| ((*name).to_owned(), value.to_owned()))
            })
            .collect();
        (!headers.is_empty()).then_some(headers)
    }
}

fn error_message(error: &reqwest::Error) -> String {
    // undici 的网络错误是 `fetch failed`（cause 带具体原因）；Host 统一保留 reqwest 的描述，
    // 并补上最底层原因，便于诊断。
    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let text = cause.to_string();
        if !message.contains(&text) {
            message = format!("{message}: {text}");
        }
        source = cause.source();
    }
    message
}

/// 响应 401 的观察者：参数是最终请求 URL 与请求头，由登录态决定是否退出。同步调用、不能阻塞：
/// 需要异步处理的实现自己在所属的作用域里启动任务（docs/specs/rust-u2-task-scope.md §6.1）。
pub type UnauthorizedHandler = Arc<dyn Fn(String, HeaderList) + Send + Sync>;

enum NetworkSource {
    Settings(SettingStore),
    Fixed(NetworkOptions),
}

#[derive(Clone)]
pub struct ApiClient {
    inner: Arc<Inner>,
}

struct Inner {
    endpoints: Endpoints,
    source_headers: SourceHeaders,
    network: NetworkSource,
    /// 首次读取后缓存的网络选项；锁只保护这个值，持锁期间不 await。
    options: Mutex<Option<NetworkOptions>>,
    clients: Mutex<HashMap<String, reqwest::Client>>,
    unauthorized: RwLock<Option<UnauthorizedHandler>>,
}

impl ApiClient {
    pub fn new(endpoints: Endpoints, app_config_dir: PathBuf, settings: SettingStore) -> Self {
        Self::build(endpoints, app_config_dir, NetworkSource::Settings(settings))
    }

    /// 固定网络选项（测试与没有设置文件的宿主）。
    pub fn with_network(
        endpoints: Endpoints,
        app_config_dir: PathBuf,
        network: NetworkOptions,
    ) -> Self {
        Self::build(endpoints, app_config_dir, NetworkSource::Fixed(network))
    }

    fn build(endpoints: Endpoints, app_config_dir: PathBuf, network: NetworkSource) -> Self {
        Self {
            inner: Arc::new(Inner {
                source_headers: SourceHeaders::new(endpoints.clone(), app_config_dir),
                endpoints,
                network,
                options: Mutex::new(None),
                clients: Mutex::new(HashMap::new()),
                unauthorized: RwLock::new(None),
            }),
        }
    }

    pub fn endpoints(&self) -> &Endpoints {
        &self.inner.endpoints
    }

    pub fn source_headers(&self) -> &SourceHeaders {
        &self.inner.source_headers
    }

    pub fn set_unauthorized_handler(&self, handler: UnauthorizedHandler) {
        *self
            .inner
            .unauthorized
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handler);
    }

    /// 首次调用时读取并缓存。并发的首次调用各自读取设置（锁外），先写入的为准，之后都读缓存；
    /// 原来读取期间持 tokio 锁，其他调用方排队等它。
    async fn network_options(&self) -> NetworkOptions {
        if let Some(options) = self.lock_options().as_ref() {
            return options.clone();
        }
        let options = match &self.inner.network {
            NetworkSource::Fixed(options) => options.clone(),
            NetworkSource::Settings(settings) => {
                NetworkOptions::from_settings(&settings.get().await)
            }
        };
        self.lock_options().get_or_insert(options).clone()
    }

    fn lock_options(&self) -> std::sync::MutexGuard<'_, Option<NetworkOptions>> {
        self.inner
            .options
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn client_for(
        &self,
        route: &ProxyRoute,
        ca_cert_path: Option<&str>,
        follow_redirects: bool,
    ) -> Result<reqwest::Client, String> {
        let proxy = match route {
            ProxyRoute::Proxy(proxy) => Some(proxy.clone()),
            ProxyRoute::Direct { .. } => None,
            ProxyRoute::Invalid(reason) => return Err((*reason).to_owned()),
        };
        let key = format!(
            "{}:{}:{}:{follow_redirects}",
            if proxy.is_some() { "proxy" } else { "direct" },
            proxy.as_deref().unwrap_or("direct"),
            ca_cert_path.unwrap_or("")
        );
        if let Some(client) = self
            .inner
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
        {
            return Ok(client.clone());
        }
        // CA 读取失败只影响本次请求，不缓存失败（Node 清掉 rejected dispatcher）。
        let ca = match ca_cert_path {
            Some(path) => Some(
                tokio::fs::read(path)
                    .await
                    .map_err(|error| format!("{error}"))?,
            ),
            None => None,
        };
        let client =
            zcode_async::blocking(move || build_client(proxy.as_deref(), ca, follow_redirects))
                .await
                .map_err(|error| error.to_string())??;
        self.inner
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, client.clone());
        Ok(client)
    }

    /// `NodeApiClient.request`：不检查状态码。
    pub async fn request(&self, request: ApiRequest) -> Result<ApiResponse, ApiError> {
        let method = request.method.clone();
        let endpoint_origin = match self.inner.endpoints.current_origin().await {
            Ok(origin) => origin,
            Err(message) => return Err(ApiError::new(message, &request.url, &method)),
        };
        let url = rewrite_url(&request.url, &endpoint_origin);
        let for_endpoint = super::endpoint::origin_of(&url)
            .zip(normalize_origin(&endpoint_origin).ok())
            .is_some_and(|(request_origin, endpoint)| request_origin == endpoint);
        let mut headers = if for_endpoint {
            let mut merged: HeaderList = self.inner.source_headers.build().into_iter().collect();
            for (name, value) in request.headers.iter() {
                merged.set(name, value);
            }
            if merged.get("HTTP-Referer") == Some(DEFAULT_ZCODE_ENDPOINT_ORIGIN) {
                merged.set("HTTP-Referer", endpoint_origin.clone());
            }
            merged
        } else {
            request.headers.clone()
        };
        if !headers.contains(REQUEST_ID_HEADER) {
            headers.set(REQUEST_ID_HEADER, uuid_v4());
        }
        if for_endpoint {
            // 只记录 header key，避免 Authorization / token 落盘。
            log::debug(format!(
                "[node-api-client] zcode endpoint request {method} {url} headers={:?}",
                headers.keys()
            ));
        }
        let options = self.network_options().await;
        let route = resolve_proxy(&url, &options);
        let client = self
            .client_for(
                &route,
                options.ca_cert_path.as_deref(),
                request.follow_redirects,
            )
            .await
            .map_err(|message| ApiError::new(message, &url, &method))?;
        let parsed_method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|error| ApiError::new(error.to_string(), &url, &method))?;
        let mut builder = client.request(parsed_method, &url);
        for (name, value) in headers.iter() {
            builder = builder.header(name, value);
        }
        builder = match request.body {
            ApiBody::Empty => builder,
            ApiBody::Bytes(bytes) => builder.body(bytes),
            ApiBody::Stream(body) => builder.body(body),
        };
        let send = builder.send();
        let result = match request.timeout_ms.filter(|ms| *ms > 0) {
            Some(timeout_ms) => {
                match tokio::time::timeout(Duration::from_millis(timeout_ms), send).await {
                    Ok(result) => result,
                    Err(_) => {
                        return Err(ApiError::new(
                            format!("Request timed out after {timeout_ms}ms"),
                            &url,
                            &method,
                        ));
                    }
                }
            }
            None => send.await,
        };
        let response =
            result.map_err(|error| ApiError::new(error_message(&error), &url, &method))?;
        let status = response.status().as_u16();
        let response_headers: HeaderList = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_owned(), value.to_owned()))
            })
            .collect();
        if status == 401 {
            let handler = self
                .inner
                .unauthorized
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(handler) = handler {
                handler(url.clone(), headers.clone());
            }
        }
        // 最终 URL（重定向后）；调用方用它确认响应仍属于发起时的端点。
        let final_url = response.url().to_string();
        Ok(ApiResponse {
            status,
            url: final_url,
            method,
            headers: response_headers,
            response,
        })
    }

    /// `readApiJson`：非 2xx 抛带 status 的 ApiError，消息取响应体里的错误字段。
    pub async fn read_json(&self, request: ApiRequest) -> Result<Value, ApiError> {
        let original_url = request.url.clone();
        let method = request.method.clone();
        let response = self.request(request).await?;
        let status = response.status;
        let diagnostic = response.diagnostic_headers();
        if !response.ok() {
            let body = response.text().await.unwrap_or_default();
            return Err(ApiError {
                message: response_message(&body, status),
                url: original_url,
                method,
                status: Some(status),
                response_headers: diagnostic,
            });
        }
        let body = response.text().await.map_err(|mut error| {
            error.url = original_url.clone();
            error
        })?;
        serde_json::from_str(&body).map_err(|error| ApiError {
            message: json_error_message(&error),
            url: original_url,
            method,
            status: Some(status),
            response_headers: diagnostic,
        })
    }
}

/// `response.json()` 的 SyntaxError 文案（`Unexpected end of JSON input` 等）只用于诊断。
fn json_error_message(error: &serde_json::Error) -> String {
    if error.is_eof() {
        "Unexpected end of JSON input".to_owned()
    } else {
        format!("Invalid JSON response: {error}")
    }
}

/// `readResponseMessage`。
pub fn response_message(body: &str, status: u16) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return format!("HTTP {status}");
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(parsed) => {
            let message = ["error", "message", "msg", "detail"]
                .iter()
                .find_map(|key| parsed.get(*key).and_then(Value::as_str))
                .unwrap_or("");
            if message.trim().is_empty() {
                format!("HTTP {status}")
            } else {
                message.trim().to_owned()
            }
        }
        Err(_) => trimmed.to_owned(),
    }
}

fn build_client(
    proxy: Option<&str>,
    ca: Option<Vec<u8>>,
    follow_redirects: bool,
) -> Result<reqwest::Client, String> {
    // rustls-no-provider 不自动选择算法；与 CLI 一样统一用 ring。
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(if follow_redirects {
            reqwest::redirect::Policy::limited(MAX_REDIRECTS)
        } else {
            reqwest::redirect::Policy::none()
        });
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|error| error.to_string())?);
    }
    if let Some(pem) = ca {
        // Node 把企业 CA 追加到默认根证书之后（mergeHostApiCaCertificates），不是替换。
        let certificates =
            reqwest::Certificate::from_pem_bundle(&pem).map_err(|error| error.to_string())?;
        builder = builder.tls_certs_merge(certificates);
    }
    builder.build().map_err(|error| error.to_string())
}

/// RFC 4122 v4 UUID（`crypto.randomUUID`）。
pub fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    let _ = getrandom::fill(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
pub(crate) mod testing {
    //! 127.0.0.1 上的最小 HTTP mock：按顺序记录请求，按路由函数应答。
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[derive(Debug, Clone)]
    pub struct Recorded {
        pub method: String,
        pub target: String,
        pub headers: Vec<(String, String)>,
        pub body: String,
    }

    impl Recorded {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }
    }

    pub type Route = Arc<dyn Fn(&Recorded) -> (u16, String) + Send + Sync>;

    pub struct MockServer {
        pub origin: String,
        pub requests: Arc<Mutex<Vec<Recorded>>>,
    }

    impl MockServer {
        pub fn requests(&self) -> Vec<Recorded> {
            self.requests.lock().unwrap().clone()
        }
    }

    pub async fn serve(route: Route) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let route = route.clone();
                let recorded = recorded.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let (head_end, head) = loop {
                        let Ok(read) = socket.read(&mut chunk).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        if let Some(index) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                            break (
                                index + 4,
                                String::from_utf8_lossy(&buffer[..index]).into_owned(),
                            );
                        }
                    };
                    let mut lines = head.split("\r\n");
                    let request_line = lines.next().unwrap_or("");
                    let mut parts = request_line.split(' ');
                    let method = parts.next().unwrap_or("").to_owned();
                    let target = parts.next().unwrap_or("").to_owned();
                    let headers: Vec<(String, String)> = lines
                        .filter_map(|line| line.split_once(':'))
                        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
                        .collect();
                    let length: usize = headers
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                        .and_then(|(_, v)| v.parse().ok())
                        .unwrap_or(0);
                    while buffer.len() < head_end + length {
                        let Ok(read) = socket.read(&mut chunk).await else {
                            return;
                        };
                        if read == 0 {
                            break;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                    }
                    let body = String::from_utf8_lossy(
                        &buffer[head_end..(head_end + length).min(buffer.len())],
                    )
                    .into_owned();
                    let request = Recorded {
                        method,
                        target,
                        headers,
                        body,
                    };
                    let (status, response_body) = route(&request);
                    recorded.lock().unwrap().push(request);
                    let response = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nx-request-id: srv-1\r\nconnection: close\r\n\r\n{response_body}",
                        response_body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        MockServer { origin, requests }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{Recorded, serve};
    use super::*;
    use serde_json::json;

    fn endpoints(origin: &str) -> Endpoints {
        Endpoints::from_env(
            [
                ("ZCODE_BASE_URL".to_owned(), origin.to_owned()),
                ("ZCODE_APP_VERSION".to_owned(), "9.9.9".to_owned()),
                ("ZCODE_ENV".to_owned(), "production".to_owned()),
                ("LANG".to_owned(), "zh_CN.UTF-8".to_owned()),
            ]
            .into_iter()
            .collect(),
        )
    }

    #[test]
    fn no_proxy_rules_match_node() {
        let options = |no_proxy: &str| NetworkOptions {
            http_proxy: Some("127.0.0.1:8888".into()),
            no_proxy: Some(no_proxy.into()),
            ca_cert_path: None,
        };
        assert_eq!(
            resolve_proxy("https://api.example.com/x", &options("")),
            ProxyRoute::Proxy("http://127.0.0.1:8888/".into())
        );
        for rule in [
            "example.com",
            ".example.com",
            "*.example.com",
            "*",
            "api.example.com:443",
            "https://example.com",
        ] {
            assert_eq!(
                resolve_proxy("https://api.example.com/x", &options(rule)),
                ProxyRoute::Direct {
                    no_proxy_matched: true
                },
                "{rule}"
            );
        }
        assert!(matches!(
            resolve_proxy("https://api.example.com/x", &options("example.com:80")),
            ProxyRoute::Proxy(_)
        ));
        assert!(matches!(
            resolve_proxy("https://notexample.com/x", &options("example.com")),
            ProxyRoute::Proxy(_)
        ));
        let invalid = NetworkOptions {
            http_proxy: Some("socks5://h:1".into()),
            ..NetworkOptions::default()
        };
        assert_eq!(
            resolve_proxy("https://a/", &invalid),
            ProxyRoute::Invalid("Configured Host proxy URL is invalid")
        );
        assert_eq!(
            resolve_proxy("https://a/", &NetworkOptions::default()),
            ProxyRoute::Direct {
                no_proxy_matched: false
            }
        );
    }

    #[tokio::test]
    async fn endpoint_requests_get_source_headers_and_request_id() {
        let server = serve(Arc::new(|request: &Recorded| {
            if request.target == "/fail" {
                (400, r#"{"msg":"bad things"}"#.to_owned())
            } else {
                (200, r#"{"ok":true}"#.to_owned())
            }
        }))
        .await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("telemetry-state.json"),
            r#"{"deviceMid":"mid-1"}"#,
        )
        .unwrap();
        let client = ApiClient::with_network(
            endpoints(&server.origin),
            dir.path().to_path_buf(),
            NetworkOptions::default(),
        );
        let value = client
            .read_json(
                ApiRequest::get(format!("{}/ok", server.origin))
                    .header("Authorization", "Bearer t")
                    .header("user-agent", "custom"),
            )
            .await
            .unwrap();
        assert_eq!(value, json!({"ok": true}));
        let error = client
            .read_json(ApiRequest::post(format!("{}/fail", server.origin)).json(&json!({"a": 1})))
            .await
            .unwrap_err();
        assert_eq!(error.message, "bad things");
        assert_eq!(error.status, Some(400));
        assert_eq!(error.method, "POST");
        assert_eq!(
            error.response_headers,
            Some(BTreeMap::from([(
                "x-request-id".to_owned(),
                "srv-1".to_owned()
            )]))
        );
        let requests = server.requests();
        let first = &requests[0];
        assert_eq!(first.header("user-agent"), Some("custom"));
        assert_eq!(first.header("authorization"), Some("Bearer t"));
        assert_eq!(first.header("http-referer"), Some(server.origin.as_str()));
        assert_eq!(first.header("x-title"), Some("Z Code@electron"));
        assert_eq!(first.header("x-zcode-app-version"), Some("9.9.9"));
        assert_eq!(first.header("x-release-channel"), Some("production"));
        assert_eq!(first.header("x-client-language"), Some("zh-CN"));
        assert_eq!(first.header("x-device-mid"), Some("mid-1"));
        assert_eq!(first.header("x-request-id").map(str::len), Some(36));
        assert_eq!(requests[1].body, r#"{"a":1}"#);
        assert_eq!(requests[1].header("content-type"), Some("application/json"));
    }

    #[tokio::test]
    async fn default_origin_is_rewritten_and_other_hosts_get_no_source_headers() {
        let server = serve(Arc::new(|_: &Recorded| (200, "{}".to_owned()))).await;
        let dir = tempfile::tempdir().unwrap();
        let client = ApiClient::with_network(
            endpoints(&server.origin),
            dir.path().to_path_buf(),
            NetworkOptions::default(),
        );
        client
            .read_json(ApiRequest::get("https://zcode.z.ai/api/v1/x?y=1"))
            .await
            .unwrap();
        let other = endpoints("https://zcode.z.ai");
        let plain =
            ApiClient::with_network(other, dir.path().to_path_buf(), NetworkOptions::default());
        plain
            .read_json(ApiRequest::get(format!("{}/plain", server.origin)))
            .await
            .unwrap();
        let requests = server.requests();
        assert_eq!(requests[0].target, "/api/v1/x?y=1");
        assert_eq!(requests[0].header("x-title"), Some("Z Code@electron"));
        assert_eq!(requests[1].header("x-title"), None);
        assert!(requests[1].header("x-request-id").is_some());
    }

    #[tokio::test]
    async fn explicit_proxy_routes_requests_and_no_proxy_bypasses() {
        // mock 同时充当代理：代理请求的 request-target 是绝对 URL。
        let proxy = serve(Arc::new(|_: &Recorded| {
            (200, r#"{"via":"proxy"}"#.to_owned())
        }))
        .await;
        let direct = serve(Arc::new(|_: &Recorded| {
            (200, r#"{"via":"direct"}"#.to_owned())
        }))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let client = ApiClient::with_network(
            endpoints("https://zcode.z.ai"),
            dir.path().to_path_buf(),
            NetworkOptions {
                http_proxy: Some(proxy.origin.trim_start_matches("http://").to_owned()),
                no_proxy: Some("localhost".into()),
                ca_cert_path: None,
            },
        );
        let target = direct.origin.replace("127.0.0.1", "localhost");
        let bypass = client
            .read_json(ApiRequest::get(format!("{target}/a")))
            .await
            .unwrap();
        assert_eq!(bypass, json!({"via": "direct"}));
        let proxied = client
            .read_json(ApiRequest::get("http://upstream.invalid/b"))
            .await
            .unwrap();
        assert_eq!(proxied, json!({"via": "proxy"}));
        assert_eq!(proxy.requests()[0].target, "http://upstream.invalid/b");
    }

    #[tokio::test]
    async fn invalid_proxy_and_missing_ca_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let invalid = ApiClient::with_network(
            endpoints("https://zcode.z.ai"),
            dir.path().to_path_buf(),
            NetworkOptions {
                http_proxy: Some("ftp://x:1".into()),
                ..NetworkOptions::default()
            },
        );
        let error = invalid
            .request(ApiRequest::get("http://127.0.0.1:9/"))
            .await
            .err()
            .unwrap();
        assert_eq!(error.message, "Configured Host proxy URL is invalid");
        let missing_ca = ApiClient::with_network(
            endpoints("https://zcode.z.ai"),
            dir.path().to_path_buf(),
            NetworkOptions {
                ca_cert_path: Some(dir.path().join("none.pem").display().to_string()),
                ..NetworkOptions::default()
            },
        );
        assert!(
            missing_ca
                .request(ApiRequest::get("http://127.0.0.1:9/"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn timeout_and_unauthorized_hook() {
        let server = serve(Arc::new(|_: &Recorded| (401, "{}".to_owned()))).await;
        let dir = tempfile::tempdir().unwrap();
        let client = ApiClient::with_network(
            endpoints(&server.origin),
            dir.path().to_path_buf(),
            NetworkOptions::default(),
        );
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        client.set_unauthorized_handler(Arc::new(move |url, headers| {
            let _ = sender.send((url, headers.get("authorization").map(str::to_owned)));
        }));
        let response = client
            .request(ApiRequest::get(format!("{}/me", server.origin)).bearer("jwt"))
            .await
            .unwrap();
        assert_eq!(response.status, 401);
        let (url, auth) = receiver.recv().await.unwrap();
        assert!(url.ends_with("/me"));
        assert_eq!(auth.as_deref(), Some("Bearer jwt"));

        // 只接受连接、从不应答的端口：超时文案与 Node 相同。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        let error = client
            .request(ApiRequest::get(format!("http://{addr}/slow")).timeout_ms(50))
            .await
            .err()
            .unwrap();
        assert_eq!(error.message, "Request timed out after 50ms");
    }
}
