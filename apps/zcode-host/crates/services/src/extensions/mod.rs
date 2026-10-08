//! R5 扩展服务（docs/specs/rust-host-extensions.md）：skills、skill-sync、plugins、plugin-sync、
//! commands、memory、hooks、mcp-sync、subagents。
//!
//! 这些 channel 都是无状态的文件系统视图：每次调用重新扫描，Host 不缓存；与 Main、CLI、Node Host
//! 共享的配置文件一律经 [`cli_config::update`] 持跨进程锁读改写。
pub mod archive;
pub mod cli_config;
pub mod commands;
pub mod fsx;
pub mod json;
pub mod memory;
pub mod plugin_roots;
pub mod plugin_sync;
pub mod plugins;
pub mod skill_sync;
pub mod skills;
pub mod walk;
pub mod yaml;

use std::future::Future;
use std::path::Path;

use serde_json::Value;
use zcode_host_kit::{Reply, RpcError, ServiceBuilder};

/// 路径根：用户目录（`HOME` / `USERPROFILE`）与数据根（`{dataBaseDir}/.zcode`）。
#[derive(Debug, Clone)]
pub struct Roots {
    pub home: String,
    pub data_root: String,
}

impl Roots {
    pub fn new(home: &Path, data_base_dir: &Path) -> Self {
        Self {
            home: fsx::path_str(home),
            data_root: fsx::join(&fsx::path_str(data_base_dir), &[".zcode"]),
        }
    }

    /// `~/.zcode/cli/config.json`。
    pub fn cli_config_path(&self) -> String {
        fsx::join(&self.home, &[".zcode", "cli", "config.json"])
    }

    /// `resolveConfigPath`：`~/` 展开到用户目录；相对路径相对进程 cwd。
    pub fn resolve_config_path(&self, path: &str) -> String {
        let expanded = match path.strip_prefix("~/") {
            Some(rest) => fsx::join(&self.home, &[rest]),
            None => path.to_owned(),
        };
        if fsx::is_absolute(&expanded) {
            expanded
        } else {
            fsx::resolve(&expanded, &[])
        }
    }
}

/// 读 `~/.zcode/cli/config.json`：读不到、不是 JSON 或不是对象时为 `{}`
/// （skills、commands、hooks 的 `readCliConfigFile` / `readUserCliConfig` 语义）。
pub async fn read_cli_config_lenient(roots: &Roots) -> serde_json::Map<String, serde_json::Value> {
    fsx::read_text_opt(&roots.cli_config_path())
        .await
        .and_then(|raw| json::parse_record(&raw))
        .unwrap_or_default()
}

/// 持锁读改写 `config.json`，读取语义同 [`read_cli_config_lenient`]；`mutate` 返回 `false` 时不写。
pub async fn update_cli_config_lenient<T>(
    roots: &Roots,
    mutate: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> (bool, T),
) -> Result<T, zcode_host_kit::RpcError> {
    cli_config::update(&roots.cli_config_path(), |raw| {
        let mut config = raw
            .as_deref()
            .and_then(json::parse_record)
            .unwrap_or_default();
        let (write, value) = mutate(&mut config);
        Ok(if write {
            cli_config::Update::Write(
                json::pretty_line(&serde_json::Value::Object(config)),
                value,
            )
        } else {
            cli_config::Update::Skip(value)
        })
    })
    .await
}

/// 注册一个方法：第 0 个位置参数（缺省为 `null`）交给 `call`。
pub fn bind<S, F, Fut>(builder: ServiceBuilder, name: &str, service: &S, call: F) -> ServiceBuilder
where
    S: Clone + Send + Sync + 'static,
    F: Fn(S, Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Reply, RpcError>> + Send + 'static,
{
    let service = service.clone();
    builder.method(name, move |_, args| {
        let params = args.raw(0).cloned().unwrap_or(Value::Null);
        call(service.clone(), params)
    })
}

/// 参数对象解码失败时回与 zod 校验失败同类的错误。
pub fn parse_params<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, RpcError> {
    serde_json::from_value(value)
        .map_err(|error| zcode_host_kit::args::invalid_argument("params", &error.to_string()))
}

const NESTED_BYTES_MARKER: &str = "__zcode_rpc_nested_uint8array_v1";

/// 参数里的 `Uint8Array`：RPC 以 `{"__zcode_rpc_nested_uint8array_v1":true,"base64":…}` 传入；
/// 也接受被 JSON 展开成数组或 `{"0":…}` 的旧形态。
pub fn bytes_param(value: Option<&Value>) -> Result<Vec<u8>, RpcError> {
    let invalid = || zcode_host_kit::args::invalid_argument("archive", "expected Uint8Array");
    match value {
        Some(Value::Object(map)) if map.get(NESTED_BYTES_MARKER) == Some(&Value::Bool(true)) => {
            let text = map.get("base64").and_then(Value::as_str).ok_or_else(invalid)?;
            crate::agent::wire::decode_base64(text).ok_or_else(invalid)
        }
        Some(Value::Object(map)) => (0..map.len())
            .map(|index| {
                map.get(&index.to_string())
                    .and_then(Value::as_u64)
                    .and_then(|byte| u8::try_from(byte).ok())
                    .ok_or_else(invalid)
            })
            .collect(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_u64()
                    .and_then(|byte| u8::try_from(byte).ok())
                    .ok_or_else(invalid)
            })
            .collect(),
        _ => Err(invalid()),
    }
}

/// 结果对象里的 `Uint8Array`（TS 客户端按标记还原）。
pub fn bytes_value(bytes: &[u8]) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(NESTED_BYTES_MARKER.into(), Value::Bool(true));
    map.insert(
        "base64".into(),
        Value::String(zcode_host_kit::base64_encode(bytes)),
    );
    Value::Object(map)
}

/// `checkRemoteSyncDirectoryWriteAccess`：建目录、排他写入探针文件再删除。
pub async fn check_write_access(directory: &str) -> Value {
    let marker = fsx::join(
        directory,
        &[&format!(
            ".zcode-sync-preflight-{}-{}",
            std::process::id(),
            fsx::random_uuid()
        )],
    );
    let result: Result<(), RpcError> = async {
        fsx::mkdirp(directory).await?;
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = options
            .open(&marker)
            .await
            .map_err(|error| fsx::fs_error(&error, "open", &marker))?;
        tokio::io::AsyncWriteExt::write_all(&mut file, b"ok")
            .await
            .map_err(|error| fsx::fs_error(&error, "write", &marker))?;
        drop(file);
        let _ = tokio::fs::remove_file(&marker).await;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => serde_json::json!({ "ok": true, "path": directory }),
        Err(error) => {
            let _ = tokio::fs::remove_file(&marker).await;
            serde_json::json!({ "ok": false, "path": directory, "error": error.message })
        }
    }
}
