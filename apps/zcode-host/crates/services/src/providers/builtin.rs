//! ZCode Built-in Provider Config 的 Active/LKG 物化与 CDN 刷新
//! （packages/provider-node/src/zcode-builtin-*.ts、endpoint-scoped-zcode-builtin-source.ts）。
//!
//! - Active 路径按平台、App 版本与规范化 Endpoint 隔离：
//!   `<appConfigDir>/runtime/provider/<platform>/<appVersion>/endpoint-<sha256(origin)[..32]>/zcode-builtin.json`。
//! - 读取时在 Bundled 与 Active 中选 revision 较新者（同 revision 内容冲突时 Bundled 优先），
//!   并在锁内把 Active 原子替换为选中的 Release；Active 不可用时退回 Bundled。
//! - 刷新经同目录的 `zcode-builtin-refresh.json` 控制文件合并多进程（租约 30s，成功后 1h，
//!   失败指数退避 1min..1h）；网络期间不持有文件锁。与 Node Host 共用同一组文件。
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use serde_json::{Map, Value, json};
use zcode_host_kit::log;

use crate::store::atomic::atomic_write_private_text;
use crate::store::lock::{LockOptions, with_file_lock};

const HOUR_MS: i64 = 60 * 60 * 1000;
const LEASE_MS: i64 = 30_000;
const FAILURE_BASE_DELAY_MS: i64 = 60_000;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const RETIRED_ZAPI_PROVIDER_ID: &str = "builtin:zapi";

pub type ResolveOrigin = Arc<dyn Fn() -> BoxFuture<'static, Result<String, String>> + Send + Sync>;
/// 按 Endpoint origin 下载 Release；没有发布时返回 `Ok(None)`。
pub type FetchRelease =
    Arc<dyn Fn(String) -> BoxFuture<'static, Result<Option<Value>, String>> + Send + Sync>;

/// Endpoint 作用域的 Built-in 来源所需的环境（`EndpointScopedZCodeBuiltinSourceOptions`）。
pub struct BuiltinEnvironment {
    pub config_root: PathBuf,
    pub platform: String,
    pub app_version: String,
    pub resolve_origin: ResolveOrigin,
    pub fetch_release: FetchRelease,
}

/// `resolveClientConfigPlatform`：`darwin-aarch64` 这类发布平台键（测试可用环境覆盖）。
pub fn release_platform(vars: &BTreeMap<String, String>, platform: &str, arch: &str) -> String {
    let read = |key: &str| {
        vars.get(key)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    if let Some(value) = read("ZCODE_TEST_CLIENT_CONFIG_PLATFORM") {
        return value;
    }
    let target = read("ZCODE_TEST_RELEASE_TARGET").unwrap_or_else(|| {
        match platform {
            "darwin" => "darwin",
            "linux" => "linux",
            "win32" => "windows",
            other => other,
        }
        .to_owned()
    });
    let arch = read("ZCODE_TEST_RELEASE_ARCH").unwrap_or_else(|| {
        match arch {
            "x64" => "x86_64",
            "arm64" => "aarch64",
            other => other,
        }
        .to_owned()
    });
    format!("{target}-{arch}")
}

/// `normalizeZCodeBuiltinEndpointOrigin`。
pub fn normalize_endpoint_origin(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("ZCode Built-in Endpoint Origin 不能为空".into());
    }
    let url = url::Url::parse(trimmed).map_err(|error| error.to_string())?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("ZCode Built-in Endpoint Origin 只支持 HTTP(S)".into());
    }
    Ok(url.origin().ascii_serialization())
}

fn sha256_hex(text: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachePaths {
    pub active: PathBuf,
    pub control: PathBuf,
}

/// `resolveZCodeBuiltinCachePaths`。
pub fn cache_paths(
    config_root: &Path,
    platform: &str,
    app_version: &str,
    origin: &str,
) -> Result<CachePaths, String> {
    let segment = |value: &str, name: &str| {
        let value = value.trim();
        if value.is_empty() || value == "." || value == ".." || value.contains(['/', '\\']) {
            return Err(format!("ZCode Built-in {name} 不是合法路径段"));
        }
        Ok(value.to_owned())
    };
    let origin = normalize_endpoint_origin(origin)?;
    let directory = config_root
        .join("runtime")
        .join("provider")
        .join(segment(platform, "platform")?)
        .join(segment(app_version, "appVersion")?)
        .join(format!("endpoint-{}", &sha256_hex(&origin)[..32]));
    Ok(CachePaths {
        active: directory.join("zcode-builtin.json"),
        control: directory.join("zcode-builtin-refresh.json"),
    })
}

/// 已校验的 Release（原样保留 JSON，供解析与写回）。
#[derive(Debug, Clone)]
pub struct Release {
    pub revision: u64,
    pub value: Value,
}

/// `decodeZCodeBuiltinRelease` 的结构校验：严格的顶层与 config 键、revision 范围、
/// 拒绝已退出的 ZAPI provider。规则内部字段由解析层按 TS 的容错规则处理。
pub fn decode_release(value: Value) -> Result<Release, String> {
    let object = value.as_object().ok_or("Release 不是对象")?;
    if object.keys().any(|key| !matches!(key.as_str(), "schemaVersion" | "revision" | "config")) {
        return Err("Release 含未知字段".into());
    }
    if object.get("schemaVersion") != Some(&json!(1)) {
        return Err("Release schemaVersion 不是 1".into());
    }
    let revision = object
        .get("revision")
        .and_then(Value::as_u64)
        .filter(|revision| *revision <= MAX_SAFE_INTEGER)
        .ok_or("Release revision 非法")?;
    let config = object
        .get("config")
        .and_then(Value::as_object)
        .ok_or("Release config 不是对象")?;
    if config.len() != 2 || !config.contains_key("providerConfigRules") || !config.contains_key("modelConfigRules") {
        return Err("Release config 字段非法".into());
    }
    let retired = config["providerConfigRules"]["providerRules"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|rule| rule["providerId"] == RETIRED_ZAPI_PROVIDER_ID);
    if retired {
        return Err(format!(
            "ZCode Built-in Release 包含已退出的 Provider: {RETIRED_ZAPI_PROVIDER_ID}"
        ));
    }
    Ok(Release { revision, value })
}

/// 语义相等：对象不计键序，数字按数值比较（TS 解码后再序列化比较的等价物）。
fn semantic_eq(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| semantic_eq(x, y))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter().all(|(key, x)| b.get(key).is_some_and(|y| semantic_eq(x, y)))
        }
        _ => left == right,
    }
}

fn same_release(left: &Release, right: &Release) -> bool {
    left.revision == right.revision && semantic_eq(&left.value["config"], &right.value["config"])
}

type Candidate = Option<Result<(Vec<u8>, Release), String>>;

async fn read_candidate(path: &Path) -> Candidate {
    match tokio::fs::read(path).await {
        Ok(bytes) => Some(
            serde_json::from_slice::<Value>(&bytes)
                .map_err(|error| error.to_string())
                .and_then(decode_release)
                .map(|release| (bytes, release)),
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => Some(Err(error.to_string())),
    }
}

/// `selectReleaseCandidate`：返回 true 表示选 Active。
fn select(bundled: &Candidate, active: &Candidate) -> Result<bool, String> {
    let ok = |candidate: &Candidate| match candidate {
        Some(Ok((_, release))) => Some(release.clone()),
        _ => None,
    };
    let (bundled_release, active_release) = (ok(bundled), ok(active));
    match (bundled_release, active_release) {
        (Some(b), Some(a)) => {
            // 同 revision 冲突属于 Active 缓存失效，不能反向使可信 Bundled 无法启动。
            if b.revision == a.revision && !same_release(&b, &a) {
                return Ok(false);
            }
            Ok(a.revision > b.revision)
        }
        (Some(_), None) => Ok(false),
        (None, Some(_)) => Ok(true),
        (None, None) => {
            let errors: Vec<String> = [bundled, active]
                .into_iter()
                .filter_map(|candidate| match candidate {
                    Some(Err(error)) => Some(error.clone()),
                    _ => None,
                })
                .collect();
            Err(format!(
                "Bundled 与 Active ZCode Built-in Release 均不可用: {}",
                errors.join("; ")
            ))
        }
    }
}

/// 一次读取的结果：Registry revision 用的路径（Active 路径）与选中的 Release 字节。
#[derive(Debug, Clone)]
pub struct BuiltinRead {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    /// Active 已与选中的 Release 对齐（CLI 可直接读 `path`）。
    pub materialized: bool,
}

type RefreshFuture = Shared<BoxFuture<'static, Result<String, String>>>;

pub struct BuiltinSource {
    bundled: PathBuf,
    environment: BuiltinEnvironment,
    in_flight: Mutex<Option<RefreshFuture>>,
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

impl BuiltinSource {
    pub fn new(bundled: PathBuf, environment: BuiltinEnvironment) -> Arc<Self> {
        Self::with_clock(bundled, environment, Arc::new(now_ms))
    }

    pub fn with_clock(
        bundled: PathBuf,
        environment: BuiltinEnvironment,
        now: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new(Self {
            bundled,
            environment,
            in_flight: Mutex::new(None),
            now,
        })
    }

    pub fn bundled(&self) -> &Path {
        &self.bundled
    }

    async fn paths(&self) -> Result<CachePaths, String> {
        let origin = (self.environment.resolve_origin)().await?;
        cache_paths(
            &self.environment.config_root,
            &self.environment.platform,
            &self.environment.app_version,
            &origin,
        )
    }

    /// `read()`：选出较新的 Release 并物化 Active；物化失败时退回 Bundled（路径仍是 Active）。
    pub async fn read(&self) -> Result<BuiltinRead, String> {
        let paths = match self.paths().await {
            Ok(paths) => paths,
            Err(error) => {
                log::warn(format!("ZCode Built-in Endpoint 不可用，使用随包配置: {error}"));
                let bytes = tokio::fs::read(&self.bundled).await.map_err(|e| e.to_string())?;
                return Ok(BuiltinRead {
                    path: self.bundled.clone(),
                    bytes,
                    materialized: true,
                });
            }
        };
        // 先无锁读取：Active 只经原子替换写入，已对齐时不必每次轮询都取目录锁。
        let (bundled, active) = (read_candidate(&self.bundled).await, read_candidate(&paths.active).await);
        if let Ok(true) = select(&bundled, &active)
            && let Some(Ok((bytes, _))) = active
        {
            return Ok(BuiltinRead {
                path: paths.active,
                bytes,
                materialized: true,
            });
        }
        if let (Ok(false), Some(Ok((_, b))), Some(Ok((bytes, a)))) = (select(&bundled, &active), &bundled, &active)
            && same_release(b, a)
        {
            return Ok(BuiltinRead {
                path: paths.active,
                bytes: bytes.clone(),
                materialized: true,
            });
        }
        match self.materialize(&paths.active).await {
            Ok(bytes) => Ok(BuiltinRead {
                path: paths.active,
                bytes,
                materialized: true,
            }),
            Err(error) => {
                // Active 只是可丢弃缓存：锁或原子写失败不能阻断 Bundled 基线。
                log::warn(format!("ZCode Built-in Active 物化失败，使用随包配置: {error}"));
                match bundled {
                    Some(Ok((bytes, _))) => Ok(BuiltinRead {
                        path: paths.active,
                        bytes,
                        materialized: false,
                    }),
                    Some(Err(error)) => Err(error),
                    None => Err("ZCode Built-in 随包配置不存在".into()),
                }
            }
        }
    }

    /// 锁内重读并把 Active 对齐到选中的 Release，返回 Active 的字节。
    async fn materialize(&self, active: &Path) -> Result<Vec<u8>, String> {
        let bundled = self.bundled.clone();
        let active_path = active.to_path_buf();
        with_file_lock(active, &LockOptions::default(), async || {
            let (selected, _) = materialize_locked(&bundled, &active_path).await?;
            Ok(selected)
        })
        .await
        .map_err(|error| error.to_string())
    }

    /// `applyRemoteRelease`：`updated` / `unchanged` / `stale`。
    async fn apply_remote(&self, active: &Path, release: Release) -> Result<&'static str, String> {
        let bundled = self.bundled.clone();
        let active_path = active.to_path_buf();
        with_file_lock(active, &LockOptions::default(), async || {
            let (_, current) = materialize_locked(&bundled, &active_path).await?;
            if release.revision < current.revision {
                return Ok("stale");
            }
            if release.revision == current.revision {
                if same_release(&release, &current) {
                    return Ok("unchanged");
                }
                anyhow::bail!("ZCode Built-in 相同 revision {} 对应不同内容", release.revision);
            }
            write_active(&active_path, &release.value).await?;
            Ok("updated")
        })
        .await
        .map_err(|error| error.to_string())
    }

    /// `refresh({ force })`：同一时刻只有一个刷新在途，并发调用共享结果。
    pub fn refresh(self: &Arc<Self>, force: bool) -> BoxFuture<'static, Result<String, String>> {
        let mut in_flight = self.in_flight.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(current) = in_flight.as_ref() {
            return current.clone().boxed();
        }
        let this = self.clone();
        let future: BoxFuture<'static, Result<String, String>> = async move {
            let result = this.refresh_once(force).await;
            *this.in_flight.lock().unwrap_or_else(|p| p.into_inner()) = None;
            result
        }
        .boxed();
        let shared = future.shared();
        *in_flight = Some(shared.clone());
        shared.boxed()
    }

    async fn refresh_once(&self, force: bool) -> Result<String, String> {
        let endpoint = normalize_endpoint_origin(&(self.environment.resolve_origin)().await?)?;
        let paths = self.paths().await?;
        let lease_id = crate::main_bridge::random_id();
        let now = (self.now)();
        let acquired = {
            let (control, endpoint, lease_id) = (paths.control.clone(), endpoint.clone(), lease_id.clone());
            with_file_lock(&paths.control, &LockOptions::default(), async || {
                let current = read_control(&control).await;
                let same = current["endpointKey"] == endpoint.as_str();
                if same && current["leaseUntil"].as_f64().unwrap_or(0.0) > now as f64 {
                    return Ok(Some("lease-held"));
                }
                if !force && same && current["nextEligibleAt"].as_f64().unwrap_or(0.0) > now as f64 {
                    return Ok(Some("not-due"));
                }
                let mut next = Map::new();
                next.insert("schemaVersion".into(), json!(1));
                next.insert("endpointKey".into(), json!(endpoint));
                next.insert("leaseId".into(), json!(lease_id));
                next.insert("leaseUntil".into(), json!(now + LEASE_MS));
                next.insert(
                    "nextEligibleAt".into(),
                    if same { current["nextEligibleAt"].clone() } else { json!(0) },
                );
                next.insert(
                    "failureCount".into(),
                    if same { current["failureCount"].clone() } else { json!(0) },
                );
                write_control(&control, &Value::Object(next)).await?;
                Ok(None)
            })
            .await
            .map_err(|error| error.to_string())?
        };
        if let Some(reason) = acquired {
            log::debug(format!("ZCode Built-in 刷新检查: skipped ({reason})"));
            return Ok("skipped".into());
        }
        let fetched = (self.environment.fetch_release)(endpoint.clone()).await;
        let outcome = async {
            let release = fetched?;
            let current_endpoint = normalize_endpoint_origin(&(self.environment.resolve_origin)().await?)?;
            if current_endpoint != endpoint {
                log::debug("ZCode Built-in 刷新检查: skipped (endpoint-changed)".to_owned());
                return Ok("skipped".to_owned());
            }
            let Some(release) = release else {
                log::debug("ZCode Built-in 刷新检查: missing".to_owned());
                return Ok("missing".to_owned());
            };
            let revision = release.get("revision").cloned().unwrap_or(Value::Null);
            let release = decode_release(release)?;
            let result = self.apply_remote(&paths.active, release).await?;
            if result == "updated" {
                log::info(format!("ZCode Built-in CDN 配置已更新 revision={revision}"));
            } else {
                log::debug(format!("ZCode Built-in 刷新检查: {result} revision={revision}"));
            }
            Ok(result.to_owned())
        }
        .await;
        self.finish_lease(&paths.control, &endpoint, &lease_id, outcome.is_ok()).await;
        outcome
    }

    async fn finish_lease(&self, control: &Path, endpoint: &str, lease_id: &str, success: bool) {
        let now = (self.now)();
        let result = with_file_lock(control, &LockOptions::default(), async || {
            let current = read_control(control).await;
            if current["endpointKey"] != endpoint || current["leaseId"] != lease_id {
                return Ok(());
            }
            let previous = current["failureCount"].as_u64().unwrap_or(0);
            let failure_count = if success { 0 } else { previous + 1 };
            let delay = if success {
                HOUR_MS
            } else {
                let exponent = failure_count.saturating_sub(1).min(32) as u32;
                FAILURE_BASE_DELAY_MS.saturating_mul(2i64.saturating_pow(exponent)).min(HOUR_MS)
            };
            write_control(
                control,
                &json!({
                    "schemaVersion": 1,
                    "endpointKey": endpoint,
                    "leaseUntil": 0,
                    "nextEligibleAt": now + delay,
                    "failureCount": failure_count,
                }),
            )
            .await
        })
        .await;
        if let Err(error) = result {
            log::warn(format!("ZCode Built-in 刷新控制文件写入失败: {error}"));
        }
    }
}

/// 锁内：读两份候选，选出较新者，必要时替换 Active。返回 (Active 字节, 选中的 Release)。
async fn materialize_locked(bundled: &Path, active: &Path) -> anyhow::Result<(Vec<u8>, Release)> {
    let (bundled_candidate, active_candidate) = (read_candidate(bundled).await, read_candidate(active).await);
    let use_active = select(&bundled_candidate, &active_candidate).map_err(anyhow::Error::msg)?;
    let selected = if use_active { &active_candidate } else { &bundled_candidate };
    let Some(Ok((_, release))) = selected else {
        anyhow::bail!("ZCode Built-in Release 不可用");
    };
    let release = release.clone();
    if let Some(Ok((bytes, current))) = &active_candidate
        && same_release(current, &release)
    {
        return Ok((bytes.clone(), release));
    }
    let bytes = write_active(active, &release.value).await?;
    Ok((bytes, release))
}

async fn write_active(active: &Path, value: &Value) -> anyhow::Result<Vec<u8>> {
    let text = serde_json::to_string_pretty(value)?;
    atomic_write_private_text(active, &text).await?;
    Ok(text.into_bytes())
}

/// 控制文件不是业务事实：缺失或损坏都按空控制处理，在锁内重建。
async fn read_control(path: &Path) -> Value {
    let empty = json!({"schemaVersion": 1, "endpointKey": "", "leaseUntil": 0, "nextEligibleAt": 0, "failureCount": 0});
    let Ok(text) = tokio::fs::read_to_string(path).await else {
        return empty;
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return empty;
    };
    let valid = value["schemaVersion"] == 1
        && value["endpointKey"].is_string()
        && (value.get("leaseId").is_none() || value["leaseId"].is_string())
        && value["leaseUntil"].is_number()
        && value["nextEligibleAt"].is_number()
        && value["failureCount"].is_u64();
    if valid { value } else { empty }
}

async fn write_control(path: &Path, value: &Value) -> anyhow::Result<()> {
    atomic_write_private_text(path, &serde_json::to_string_pretty(value)?).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

    fn release(revision: u64, label: &str) -> Value {
        json!({"schemaVersion": 1, "revision": revision, "config": {
            "providerConfigRules": {"templateRules": [], "providerRules": [{"providerId": "builtin:x", "providerName": label}]},
            "modelConfigRules": {},
        }})
    }

    fn source(
        dir: &Path,
        origin: Arc<Mutex<String>>,
        remote: Arc<Mutex<Result<Option<Value>, String>>>,
        fetches: Arc<AtomicUsize>,
        clock: Arc<AtomicI64>,
    ) -> Arc<BuiltinSource> {
        let bundled = dir.join("bundled.json");
        BuiltinSource::with_clock(
            bundled,
            BuiltinEnvironment {
                config_root: dir.join("v2"),
                platform: "darwin-aarch64".into(),
                app_version: "1.2.3".into(),
                resolve_origin: Arc::new(move || {
                    let origin = origin.lock().unwrap().clone();
                    Box::pin(async move { Ok(origin) })
                }),
                fetch_release: Arc::new(move |_| {
                    fetches.fetch_add(1, Ordering::SeqCst);
                    let result = remote.lock().unwrap().clone();
                    Box::pin(async move { result })
                }),
            },
            Arc::new(move || clock.load(Ordering::SeqCst)),
        )
    }

    #[test]
    fn cache_paths_match_node_layout() {
        let paths = cache_paths(Path::new("/root"), "darwin-aarch64", "1.2.3", "https://zcode.z.ai/x?y").unwrap();
        let key = &sha256_hex("https://zcode.z.ai")[..32];
        assert_eq!(
            paths.active,
            PathBuf::from(format!("/root/runtime/provider/darwin-aarch64/1.2.3/endpoint-{key}/zcode-builtin.json"))
        );
        assert!(cache_paths(Path::new("/root"), "..", "1", "https://a").is_err());
        assert!(cache_paths(Path::new("/root"), "p", "1", "ftp://a").is_err());
        let vars = BTreeMap::from([("ZCODE_TEST_RELEASE_ARCH".to_owned(), "riscv".to_owned())]);
        assert_eq!(release_platform(&vars, "win32", "x64"), "windows-riscv");
        assert_eq!(release_platform(&BTreeMap::new(), "darwin", "arm64"), "darwin-aarch64");
        assert!(decode_release(json!({"schemaVersion": 1, "revision": 1, "config": {"providerConfigRules": {"providerRules": [{"providerId": "builtin:zapi"}]}, "modelConfigRules": {}}})).is_err());
        assert!(decode_release(json!({"schemaVersion": 1, "revision": -1, "config": {}})).is_err());
    }

    #[tokio::test]
    async fn read_materializes_newest_and_refresh_applies_remote_once_per_lease() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bundled.json"), release(5, "bundled").to_string()).unwrap();
        let origin = Arc::new(Mutex::new("http://127.0.0.1:9".to_owned()));
        let remote = Arc::new(Mutex::new(Ok(Some(release(7, "remote")))));
        let fetches = Arc::new(AtomicUsize::new(0));
        let clock = Arc::new(AtomicI64::new(1_000_000));
        let source = source(dir.path(), origin.clone(), remote.clone(), fetches.clone(), clock.clone());

        // 首读：Active 不存在 → 物化为 Bundled。
        let first = source.read().await.unwrap();
        assert!(first.path.ends_with("zcode-builtin.json"));
        assert!(first.path.starts_with(dir.path().join("v2/runtime/provider/darwin-aarch64/1.2.3")));
        let written: Value = serde_json::from_slice(&std::fs::read(&first.path).unwrap()).unwrap();
        assert_eq!(written["revision"], 5);

        // 刷新：下载到更高 revision → 写 Active；控制文件记录下次时间。
        assert_eq!(source.refresh(false).await.unwrap(), "updated");
        let second = source.read().await.unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&second.bytes).unwrap()["revision"], 7);
        let control_path = first.path.with_file_name("zcode-builtin-refresh.json");
        let control: Value = serde_json::from_str(&std::fs::read_to_string(&control_path).unwrap()).unwrap();
        assert_eq!(control["endpointKey"], "http://127.0.0.1:9");
        assert_eq!(control["nextEligibleAt"], 1_000_000 + HOUR_MS);
        assert!(control.get("leaseId").is_none());

        // 未到期：不下载；force 时下载但内容相同 → unchanged。
        assert_eq!(source.refresh(false).await.unwrap(), "skipped");
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        assert_eq!(source.refresh(true).await.unwrap(), "unchanged");
        // 更低 revision → stale；同 revision 不同内容 → 错误并退避。
        *remote.lock().unwrap() = Ok(Some(release(6, "older")));
        assert_eq!(source.refresh(true).await.unwrap(), "stale");
        *remote.lock().unwrap() = Ok(Some(release(7, "conflict")));
        assert!(source.refresh(true).await.unwrap_err().contains("相同 revision 7"));
        let control: Value = serde_json::from_str(&std::fs::read_to_string(&control_path).unwrap()).unwrap();
        assert_eq!(control["failureCount"], 1);
        assert_eq!(control["nextEligibleAt"], 1_000_000 + FAILURE_BASE_DELAY_MS);

        // Bundled 升级到更高 revision → 读取时以 Bundled 覆盖 Active。
        std::fs::write(dir.path().join("bundled.json"), release(9, "upgraded").to_string()).unwrap();
        let third = source.read().await.unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&third.bytes).unwrap()["revision"], 9);

        // Endpoint 切换：换到另一份 Active，不复用上一 Endpoint 的缓存。
        *origin.lock().unwrap() = "http://127.0.0.1:10".to_owned();
        let other = source.read().await.unwrap();
        assert_ne!(other.path, third.path);
    }
}
