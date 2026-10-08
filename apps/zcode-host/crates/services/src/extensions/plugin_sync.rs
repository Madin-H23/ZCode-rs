//! `plugin-sync` channel（packages/services/src/plugin-sync/pluginSyncService.ts）：把 `plugins.dirs`
//! 里的 inline 插件、或本地 marketplace 源镜像打包给远端，以及在远端导入（只新增，不覆盖）。
use std::collections::{HashMap, HashSet};

use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use zcode_cli_domain::js_string::trim as js_trim;
use zcode_host_kit::{Reply, RpcError, ServiceBuilder};

use super::archive::{self, Entry, Flavor};
use super::cli_config::{self, Update};
use super::skill_sync::truthy;
use super::{Roots, bind, bytes_param, bytes_value, check_write_access, fsx, json as js};
use crate::support::collate::locale_compare;

pub const CHANNEL: &str = "plugin-sync";
pub const DEFAULT_MAX_ARCHIVE_BYTES: u64 = 50 * 1024 * 1024;
const METADATA_PATH: &str = ".zcode-plugin-sync.json";
const SYNC_LABEL: &str = "unsafe plugin sync path";
const SOURCE_LABEL: &str = "unsafe marketplace plugin source path";
const MARKETPLACE_SOURCES_DIR: &str = "marketplace-sources";
const MIRROR_ROOT: &str = "plugins";

#[derive(Debug, Clone)]
struct Manifest {
    name: String,
    plugin_id: String,
    description: Option<String>,
    version: Option<String>,
    component_types: Vec<&'static str>,
}

#[derive(Debug, Clone)]
struct Candidate {
    id: String,
    manifest: Manifest,
    directory_name: String,
    path: String,
    size_bytes: u64,
    enabled_override: Option<bool>,
}

impl Candidate {
    fn to_value(&self) -> Value {
        let mut value = Map::new();
        value.insert("id".into(), json!(self.id));
        value.insert("name".into(), json!(self.manifest.name));
        value.insert("pluginId".into(), json!(self.manifest.plugin_id));
        value.insert("directoryName".into(), json!(self.directory_name));
        if let Some(description) = &self.manifest.description {
            value.insert("description".into(), json!(description));
        }
        if let Some(version) = &self.manifest.version {
            value.insert("version".into(), json!(version));
        }
        value.insert("path".into(), json!(self.path));
        value.insert("sizeBytes".into(), json!(self.size_bytes));
        value.insert(
            "enabled".into(),
            json!(self.enabled_override.unwrap_or(true)),
        );
        if let Some(enabled) = self.enabled_override {
            value.insert("enabledOverride".into(), json!(enabled));
        }
        value.insert(
            "componentTypes".into(),
            json!(self.manifest.component_types),
        );
        Value::Object(value)
    }
}

/// `normalizePluginId`。
fn plugin_id(value: &str) -> Result<String, RpcError> {
    let normalized = js_trim(value);
    if normalized.is_empty() || normalized.contains('/') || normalized.contains('\\') {
        return Err(fsx::error(format!("invalid plugin id: {value}")));
    }
    Ok(normalized.to_owned())
}

fn plugin_id_key(value: &str) -> Result<String, RpcError> {
    Ok(plugin_id(value)?.to_lowercase())
}

/// `readRequiredString`。
fn required(value: Option<&Value>, label: &str) -> Result<String, RpcError> {
    match value.and_then(Value::as_str).map(js_trim) {
        Some(text) if !text.is_empty() => Ok(text.to_owned()),
        _ => Err(fsx::error(format!("missing {label}"))),
    }
}

fn short_hash(value: &str) -> String {
    fsx::sha256_hex(value.as_bytes())
}

/// `toSafeDirectoryName`。
fn safe_directory_name(value: &str) -> String {
    let sanitized: String = js_trim(value)
        .chars()
        .map(|c| {
            if matches!(c, '\\' | '/' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect();
    let sanitized = if sanitized.is_empty() {
        "plugin".to_owned()
    } else {
        sanitized
    };
    archive::normalize_relative(&sanitized, SYNC_LABEL)
        .unwrap_or_else(|_| format!("plugin-{}", &short_hash(value)[..8]))
}

async fn lexists(path: &str) -> bool {
    fsx::lexists(path).await
}

async fn directory_exists(path: &str) -> bool {
    tokio::fs::symlink_metadata(path)
        .await
        .is_ok_and(|meta| meta.is_dir())
}

/// `readPluginManifestInfo`：第一个能读到的 manifest 决定结果（解析失败不再看后续路径）。
async fn read_manifest(root: &str) -> Option<Manifest> {
    for segments in [
        [".zcode-plugin", "plugin.json"],
        [".claude-plugin", "plugin.json"],
        [".codex-plugin", "plugin.json"],
    ] {
        let Some(raw) = fsx::read_text_opt(&fsx::join(root, &segments)).await else {
            continue;
        };
        let parsed = js::parse_record(&raw)?;
        let name = js_trim(parsed.get("name").and_then(Value::as_str).unwrap_or("")).to_owned();
        if name.is_empty() {
            return None;
        }
        let text = |key: &str| {
            parsed
                .get(key)
                .and_then(Value::as_str)
                .map(js_trim)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        };
        let mut component_types = Vec::new();
        for (key, path, kind) in [
            ("skills", vec!["skills"], "skills"),
            ("commands", vec!["commands"], "commands"),
            ("hooks", vec!["hooks", "hooks.json"], "hooks"),
            ("mcpServers", vec![".mcp.json"], "mcp"),
        ] {
            if parsed.contains_key(key) || lexists(&fsx::join(root, &path)).await {
                component_types.push(kind);
            }
        }
        return Some(Manifest {
            plugin_id: format!("{name}@inline"),
            name,
            description: text("description"),
            version: text("version"),
            component_types,
        });
    }
    None
}

/// `computeRecursiveSize`（不跟随软链）。
fn recursive_size(path: &str) -> BoxFuture<'_, u64> {
    Box::pin(async move {
        let Ok(meta) = tokio::fs::symlink_metadata(path).await else {
            return 0;
        };
        if meta.is_file() {
            return meta.len();
        }
        if !meta.is_dir() {
            return 0;
        }
        let mut total = 0;
        if let Ok(children) = fsx::read_dir(path).await {
            for child in children {
                total += recursive_size(&fsx::join(path, &[&child.name])).await;
            }
        }
        total
    })
}

/// `computeRecursiveContentFingerprint`。
fn fingerprint(path: &str) -> BoxFuture<'_, Result<String, RpcError>> {
    Box::pin(async move {
        let meta = tokio::fs::symlink_metadata(path)
            .await
            .map_err(|error| fsx::fs_error(&error, "lstat", path))?;
        let mut hash = Sha256::new();
        if meta.is_file() {
            hash.update(b"file\0");
            hash.update(
                tokio::fs::read(path)
                    .await
                    .map_err(|error| fsx::fs_error(&error, "open", path))?,
            );
            return Ok(hex(hash));
        }
        if !meta.is_dir() {
            return Err(fsx::error(format!(
                "unsupported plugin archive source: {path}"
            )));
        }
        hash.update(b"dir\0");
        let mut children = fsx::read_dir(path)
            .await
            .map_err(|error| fsx::fs_error(&error, "scandir", path))?;
        children.sort_by(|left, right| locale_compare(&left.name, &right.name));
        for child in children {
            hash.update(child.name.as_bytes());
            hash.update(b"\0");
            hash.update(
                fingerprint(&fsx::join(path, &[&child.name]))
                    .await?
                    .as_bytes(),
            );
            hash.update(b"\0");
        }
        Ok(hex(hash))
    })
}

fn hex(hash: Sha256) -> String {
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn string_array_non_blank(value: Option<&Value>) -> Vec<String> {
    js::string_array(value)
        .into_iter()
        .filter(|item| !js_trim(item).is_empty())
        .collect()
}

#[derive(Clone)]
pub struct PluginSyncService {
    roots: Roots,
    max_archive_bytes: u64,
}

struct ConfigState {
    dirs: Vec<String>,
    overrides: HashMap<String, bool>,
}

#[derive(Debug, Clone)]
struct MarketplaceEntry {
    name: String,
    dependencies: Vec<String>,
    raw: Map<String, Value>,
    source: Option<Value>,
}

#[derive(Debug, Clone)]
struct Marketplace {
    name: String,
    description: Option<String>,
    plugin_root: Option<String>,
    plugins: Vec<MarketplaceEntry>,
    raw: Map<String, Value>,
}

struct Loaded {
    manifest: Marketplace,
    source_root: Option<String>,
}

struct Mirror {
    entry: MarketplaceEntry,
    source_path: Option<String>,
    relative_path: Option<String>,
}

/// `readJsonFileOrEmpty`：不存在为 `{}`，JSON 损坏或不是对象时报错。
fn parse_config_or_empty(raw: Option<&str>, path: &str) -> Result<Map<String, Value>, RpcError> {
    let Some(raw) = raw else {
        return Ok(Map::new());
    };
    match js::parse(raw) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(fsx::error(format!("invalid json object: {path}"))),
        Err(message) => Err(RpcError::new("SyntaxError", message)),
    }
}

fn parse_marketplace(value: &Value) -> Result<Marketplace, RpcError> {
    let Value::Object(map) = value else {
        return Err(fsx::error("marketplace manifest is invalid"));
    };
    let name = required(map.get("name"), "marketplace name")?;
    let values: Vec<Value> = match map.get("plugins") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::Object(entries)) => entries
            .iter()
            .map(|(plugin_name, plugin)| {
                let mut merged = Map::new();
                merged.insert("name".into(), json!(plugin_name));
                if let Value::Object(fields) = plugin {
                    for (key, value) in fields {
                        merged.insert(key.clone(), value.clone());
                    }
                }
                Value::Object(merged)
            })
            .collect(),
        _ => Vec::new(),
    };
    let mut plugins = Vec::new();
    for plugin in values.iter().filter_map(Value::as_object) {
        let plugin_name = required(plugin.get("name"), "marketplace plugin name")?;
        plugins.push(MarketplaceEntry {
            name: plugin_name,
            dependencies: string_array_non_blank(plugin.get("dependencies")),
            raw: plugin.clone(),
            source: plugin.get("source").cloned(),
        });
    }
    let root_of = |map: &Map<String, Value>| {
        map.get("pluginRoot")
            .and_then(Value::as_str)
            .map(js_trim)
            .filter(|root| !root.is_empty())
            .map(str::to_owned)
    };
    Ok(Marketplace {
        name,
        description: map
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned),
        plugin_root: root_of(map).or_else(|| root_of(&js::record(map.get("metadata")))),
        plugins,
        raw: map.clone(),
    })
}

async fn read_marketplace(path: &str) -> Result<Marketplace, RpcError> {
    let raw = fsx::read_text(path)
        .await
        .map_err(|error| fsx::fs_error(&error, "open", path))?;
    let value = js::parse(&raw).map_err(|message| RpcError::new("SyntaxError", message))?;
    parse_marketplace(&value)
}

fn is_remote_source(source: Option<&Value>) -> bool {
    source
        .and_then(Value::as_object)
        .and_then(|source| source.get("source"))
        .and_then(Value::as_str)
        .is_some_and(|kind| ["github", "git", "url", "git-subdir", "npm", "pip"].contains(&kind))
}

impl PluginSyncService {
    pub fn new(roots: Roots) -> Self {
        Self {
            roots,
            max_archive_bytes: DEFAULT_MAX_ARCHIVE_BYTES,
        }
    }

    fn plugin_root(&self) -> String {
        fsx::join(&self.roots.home, &[".zcode", "plugins"])
    }

    async fn config_state(&self) -> Result<ConfigState, RpcError> {
        let path = self.roots.cli_config_path();
        let raw = match fsx::read_text(&path).await {
            Ok(raw) => Some(raw),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(fsx::fs_error(&error, "open", &path)),
        };
        let config = parse_config_or_empty(raw.as_deref(), &path)?;
        let plugins = js::record(config.get("plugins"));
        let mut overrides = HashMap::new();
        for (id, enabled) in js::boolean_record(plugins.get("enabledPlugins")) {
            overrides.insert(plugin_id_key(&id)?, enabled);
        }
        Ok(ConfigState {
            dirs: string_array_non_blank(plugins.get("dirs")),
            overrides,
        })
    }

    async fn candidates(&self) -> Result<Vec<Candidate>, RpcError> {
        let config = self.config_state().await?;
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();
        let mut used = HashSet::new();
        for raw in &config.dirs {
            let root = fsx::resolve(raw, &[]);
            let canonical = fsx::realpath_or_self(&root).await;
            if !seen.insert(canonical.clone()) {
                continue;
            }
            let Some(manifest) = read_manifest(&canonical).await else {
                continue;
            };
            let enabled_override = config
                .overrides
                .get(&plugin_id_key(&manifest.plugin_id)?)
                .copied();
            let base = safe_directory_name(&{
                let basename = fsx::basename(&canonical);
                if basename.is_empty() {
                    manifest.name.clone()
                } else {
                    basename
                }
            });
            let directory_name = if used.insert(base.clone()) {
                base
            } else {
                let deduped =
                    safe_directory_name(&format!("{base}-{}", &short_hash(&canonical)[..8]));
                used.insert(deduped.clone());
                deduped
            };
            candidates.push(Candidate {
                id: short_hash(&format!("{}:{canonical}", manifest.plugin_id)),
                size_bytes: recursive_size(&canonical).await,
                manifest,
                directory_name,
                path: canonical,
                enabled_override,
            });
        }
        candidates.sort_by(|left, right| locale_compare(&left.manifest.name, &right.manifest.name));
        Ok(candidates)
    }

    async fn configured_paths_by_id(&self) -> Result<HashMap<String, String>, RpcError> {
        let config = self.config_state().await?;
        let mut result = HashMap::new();
        for raw in &config.dirs {
            let root = fsx::resolve(raw, &[]);
            let canonical = fsx::realpath_or_self(&root).await;
            let Some(manifest) = read_manifest(&canonical).await else {
                continue;
            };
            result
                .entry(plugin_id_key(&manifest.plugin_id)?)
                .or_insert(canonical);
        }
        Ok(result)
    }

    pub async fn list_local_candidates(&self) -> Result<Value, RpcError> {
        let candidates = self.candidates().await?;
        Ok(json!({
            "candidates": candidates.iter().map(Candidate::to_value).collect::<Vec<_>>(),
            "maxArchiveBytes": self.max_archive_bytes,
        }))
    }

    pub async fn list_remote_statuses(&self, params: Value) -> Result<Value, RpcError> {
        let target_root = self.plugin_root();
        let existing = self.configured_paths_by_id().await?;
        let plugins = params
            .get("plugins")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut statuses = Vec::new();
        for plugin in plugins {
            let directory_name = archive::normalize_relative(
                plugin
                    .get("directoryName")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                SYNC_LABEL,
            )?;
            let id = plugin_id(plugin.get("pluginId").and_then(Value::as_str).unwrap_or(""))?;
            let target = archive::resolve_within(&target_root, &directory_name, SYNC_LABEL)?;
            if std::path::Path::new(&target).exists() {
                statuses.push(json!({ "pluginId": id, "directoryName": directory_name, "exists": true, "path": target, "reason": "targetExists" }));
                continue;
            }
            statuses.push(match existing.get(&plugin_id_key(&id)?) {
                Some(path) => json!({ "pluginId": id, "directoryName": directory_name, "exists": true, "path": path, "reason": "samePluginId" }),
                None => json!({ "pluginId": id, "directoryName": directory_name, "exists": false }),
            });
        }
        Ok(json!({ "statuses": statuses }))
    }

    fn metadata_entry(metadata: &Value) -> Entry {
        Entry::Content {
            archive_path: METADATA_PATH.to_owned(),
            content: js::pretty_line(metadata).into_bytes(),
        }
    }

    pub async fn export_plugins(&self, params: Value) -> Result<Value, RpcError> {
        let candidates = self.candidates().await?;
        let ids: Vec<String> = js::string_array(params.get("pluginIds"));
        let mut selected = Vec::new();
        for id in &ids {
            let candidate = candidates
                .iter()
                .find(|candidate| &candidate.id == id)
                .ok_or_else(|| fsx::error(format!("plugin sync candidate not found: {id}")))?;
            selected.push(candidate.clone());
        }
        let max = self.max_archive_bytes;
        let selected_bytes: u64 = selected.iter().map(|candidate| candidate.size_bytes).sum();
        if selected_bytes > max {
            return Err(fsx::error(format!(
                "plugin sync archive exceeds limit: {selected_bytes}/{max}"
            )));
        }
        let describe = |candidate: &Candidate, with_id: bool| {
            let mut value = Map::new();
            if with_id {
                value.insert("id".into(), json!(candidate.id));
            }
            value.insert("name".into(), json!(candidate.manifest.name));
            value.insert("pluginId".into(), json!(candidate.manifest.plugin_id));
            value.insert("directoryName".into(), json!(candidate.directory_name));
            if let Some(enabled) = candidate.enabled_override {
                value.insert("enabled".into(), json!(enabled));
            }
            Value::Object(value)
        };
        let mut entries = Vec::new();
        for candidate in &selected {
            entries.push(Entry::Source {
                source_path: fsx::realpath(&candidate.path)
                    .await
                    .map_err(|error| fsx::fs_error(&error, "realpath", &candidate.path))?,
                archive_path: candidate.directory_name.clone(),
            });
        }
        let metadata =
            json!({ "plugins": selected.iter().map(|c| describe(c, false)).collect::<Vec<_>>() });
        entries.push(Self::metadata_entry(&metadata));
        let archive = archive::create(Flavor::Plugin, entries).await?;
        if archive.len() as u64 > max {
            return Err(fsx::error(format!(
                "plugin sync archive exceeds limit: {}/{max}",
                archive.len()
            )));
        }
        Ok(json!({
            "archive": bytes_value(&archive),
            "archiveBytes": archive.len(),
            "plugins": selected.iter().map(|c| describe(c, true)).collect::<Vec<_>>(),
        }))
    }

    async fn load_source(&self, source: &Map<String, Value>) -> Result<Loaded, RpcError> {
        let kind = source.get("source").and_then(Value::as_str).unwrap_or("");
        match kind {
            "file" => {
                let path = required(source.get("path"), "marketplace source file path")?;
                Ok(Loaded {
                    manifest: read_marketplace(&path).await?,
                    source_root: Some(fsx::dirname(&path)),
                })
            }
            "directory" => {
                let root = required(source.get("path"), "marketplace source directory path")?;
                let mut manifest_path = None;
                for candidate in [
                    fsx::join(&root, &["marketplace.json"]),
                    fsx::join(&root, &[".claude-plugin", "marketplace.json"]),
                ] {
                    if std::path::Path::new(&candidate).exists() {
                        manifest_path = Some(candidate);
                        break;
                    }
                }
                let manifest_path = manifest_path.ok_or_else(|| {
                    fsx::error(format!(
                        "marketplace manifest not found in directory: {root}"
                    ))
                })?;
                Ok(Loaded {
                    manifest: read_marketplace(&manifest_path).await?,
                    source_root: Some(root),
                })
            }
            "settings" => {
                let marketplace = source
                    .get("marketplace")
                    .filter(|value| value.is_object())
                    .ok_or_else(|| {
                        fsx::error("settings marketplace source is missing marketplace manifest")
                    })?;
                Ok(Loaded {
                    manifest: parse_marketplace(marketplace)?,
                    source_root: None,
                })
            }
            other => Err(fsx::error(format!(
                "unsupported marketplace source for SSH sync: {}",
                if other.is_empty() { "unknown" } else { other }
            ))),
        }
    }

    fn plugin_base_dir(loaded: &Loaded) -> Result<String, RpcError> {
        let source_root = loaded
            .source_root
            .clone()
            .ok_or_else(|| fsx::error("cannot resolve marketplace plugin source root"))?;
        let Some(plugin_root) = &loaded.manifest.plugin_root else {
            return Ok(source_root);
        };
        let resolved =
            archive::resolve_within(&source_root, plugin_root, "unsafe marketplace pluginRoot")?;
        Ok(if std::path::Path::new(&resolved).exists() {
            resolved
        } else {
            source_root
        })
    }

    async fn assert_plugin_root(root: String) -> Result<String, RpcError> {
        // 只能复制选中插件目录：先验证 manifest，避免被篡改的 entry 指向普通目录把敏感文件发到远端。
        if read_manifest(&root).await.is_none() {
            return Err(fsx::error(format!(
                "local marketplace plugin source is missing plugin manifest: {root}"
            )));
        }
        Ok(root)
    }

    async fn local_source_path(
        loaded: &Loaded,
        source_path: Option<&Value>,
        label: &str,
        allow_absolute: bool,
    ) -> Result<String, RpcError> {
        let requested = required(source_path, label)?;
        let original = source_path.and_then(Value::as_str).unwrap_or("");
        let relative = requested
            .strip_prefix("./")
            .unwrap_or(&requested)
            .to_owned();
        if loaded.source_root.is_none() && allow_absolute {
            if !fsx::is_absolute(&requested) {
                return Err(fsx::error(format!(
                    "cannot mirror local marketplace plugin source without source root: {original}"
                )));
            }
            let absolute = fsx::resolve(&requested, &[]);
            if directory_exists(&absolute).await {
                return Self::assert_plugin_root(absolute).await;
            }
            return Err(fsx::error(format!(
                "local marketplace plugin source does not exist: {original}"
            )));
        }
        if loaded.source_root.is_none() {
            return Err(fsx::error(format!(
                "cannot mirror local marketplace plugin source without source root: {original}"
            )));
        }
        let base = Self::plugin_base_dir(loaded)?;
        let path = archive::resolve_within(&base, &relative, SOURCE_LABEL)?;
        if directory_exists(&path).await {
            return Self::assert_plugin_root(path).await;
        }
        Err(fsx::error(format!(
            "local marketplace plugin source does not exist: {original}"
        )))
    }

    async fn local_fallback(loaded: &Loaded, entry: &MarketplaceEntry) -> Result<String, RpcError> {
        if loaded.source_root.is_none() {
            return Err(fsx::error(format!(
                "cannot mirror local marketplace plugin source without source root: {}",
                entry.name
            )));
        }
        let base = Self::plugin_base_dir(loaded)?;
        let path = archive::resolve_within(&base, &entry.name, SOURCE_LABEL)?;
        if directory_exists(&path).await {
            return Self::assert_plugin_root(path).await;
        }
        Err(fsx::error(format!(
            "local marketplace plugin source does not exist: {path}"
        )))
    }

    async fn mirror_entry(loaded: &Loaded, entry: &MarketplaceEntry) -> Result<Mirror, RpcError> {
        if is_remote_source(entry.source.as_ref()) {
            return Ok(Mirror {
                entry: entry.clone(),
                source_path: None,
                relative_path: None,
            });
        }
        let source_path = match &entry.source {
            Some(Value::String(_)) => {
                Self::local_source_path(
                    loaded,
                    entry.source.as_ref(),
                    &format!("{} source", entry.name),
                    false,
                )
                .await?
            }
            Some(Value::Object(source))
                if source.get("source").and_then(Value::as_str) == Some("directory")
                    && source.get("path").is_some_and(Value::is_string) =>
            {
                Self::local_source_path(
                    loaded,
                    source.get("path"),
                    &format!("{} directory source", entry.name),
                    loaded.source_root.is_none(),
                )
                .await?
            }
            _ => Self::local_fallback(loaded, entry).await?,
        };
        Ok(Mirror {
            entry: entry.clone(),
            source_path: Some(source_path),
            relative_path: Some(format!(
                "{MIRROR_ROOT}/{}",
                safe_directory_name(&entry.name)
            )),
        })
    }

    pub async fn export_marketplace_source(&self, params: Value) -> Result<Value, RpcError> {
        let max = self.max_archive_bytes;
        let marketplace_id = plugin_id(
            params
                .get("marketplaceId")
                .and_then(Value::as_str)
                .unwrap_or(""),
        )?;
        let mut plugin_names: Vec<String> = Vec::new();
        for name in params
            .get("pluginNames")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let name = required(Some(&name), "plugin name")?;
            if !plugin_names.contains(&name) {
                plugin_names.push(name);
            }
        }
        if plugin_names.is_empty() {
            return Err(fsx::error("missing marketplace plugin names"));
        }
        let source = js::record(params.get("source"));
        let loaded = self.load_source(&source).await?;
        if loaded.manifest.name != marketplace_id {
            return Err(fsx::error(format!(
                "marketplace id mismatch: {} !== {marketplace_id}",
                loaded.manifest.name
            )));
        }
        let closure = closure(&loaded.manifest, &marketplace_id, &plugin_names)?;
        let mut mirrors = Vec::new();
        for entry in &closure {
            mirrors.push(Self::mirror_entry(&loaded, entry).await?);
        }
        let manifest = mirrored_manifest(&loaded.manifest, &mirrors);
        let manifest_content = js::pretty_line(&manifest);
        let mut selected_bytes = manifest_content.len() as u64;
        for mirror in &mirrors {
            if let Some(path) = &mirror.source_path {
                selected_bytes += recursive_size(path).await;
            }
        }
        if selected_bytes > max {
            return Err(fsx::error(format!(
                "plugin marketplace source archive exceeds limit: {selected_bytes}/{max}"
            )));
        }
        let mut fingerprints = Vec::new();
        for mirror in &mirrors {
            fingerprints.push(match &mirror.source_path {
                Some(path) => json!(fingerprint(path).await?),
                None => Value::Null,
            });
        }
        let hash_input = js::stringify(
            &json!({ "manifestContent": manifest_content, "sourceFingerprints": fingerprints }),
        );
        let directory_name = format!(
            "{}-{}",
            safe_directory_name(&marketplace_id),
            &short_hash(&hash_input)[..12]
        );
        let mut entries = vec![Entry::Content {
            archive_path: format!("{directory_name}/marketplace.json"),
            content: manifest_content.into_bytes(),
        }];
        for mirror in &mirrors {
            if let (Some(path), Some(relative)) = (&mirror.source_path, &mirror.relative_path) {
                entries.push(Entry::Source {
                    source_path: path.clone(),
                    archive_path: format!("{directory_name}/{relative}"),
                });
            }
        }
        entries.push(Self::metadata_entry(&json!({
            "plugins": [],
            "marketplaceSources": [{ "marketplaceId": marketplace_id, "directoryName": directory_name }],
        })));
        let archive = archive::create(Flavor::Plugin, entries).await?;
        if archive.len() as u64 > max {
            return Err(fsx::error(format!(
                "plugin marketplace source archive exceeds limit: {}/{max}",
                archive.len()
            )));
        }
        Ok(json!({
            "archive": bytes_value(&archive),
            "archiveBytes": archive.len(),
            "marketplaceId": marketplace_id,
            "pluginNames": closure.iter().map(|entry| entry.name.clone()).collect::<Vec<_>>(),
        }))
    }

    async fn read_metadata(temp: &str) -> Result<Value, RpcError> {
        let path = fsx::join(temp, &[METADATA_PATH]);
        let raw = fsx::read_text(&path)
            .await
            .map_err(|error| fsx::fs_error(&error, "open", &path))?;
        js::parse(&raw).map_err(|message| RpcError::new("SyntaxError", message))
    }

    async fn add_plugin_dir(
        &self,
        plugin_path: &str,
        id: &str,
        enabled: Option<bool>,
    ) -> Result<(), RpcError> {
        let path = self.roots.cli_config_path();
        let resolved = fsx::resolve(plugin_path, &[]);
        let id = id.to_owned();
        cli_config::update(&path, |raw| {
            let mut config = parse_config_or_empty(raw.as_deref(), &path)?;
            let mut plugins = js::record(config.get("plugins"));
            let dirs = string_array_non_blank(plugins.get("dirs"));
            let mut next_dirs = dirs.clone();
            if !dirs.iter().any(|dir| fsx::resolve(dir, &[]) == resolved) {
                next_dirs.push(resolved.clone());
            }
            plugins.insert("dirs".into(), json!(next_dirs));
            let mut enabled_plugins = js::record(plugins.get("enabledPlugins"));
            match enabled {
                Some(flag) => {
                    enabled_plugins.insert(id.clone(), json!(flag));
                    plugins.insert("enabledPlugins".into(), Value::Object(enabled_plugins));
                }
                None if !enabled_plugins.is_empty() => {
                    plugins.insert("enabledPlugins".into(), Value::Object(enabled_plugins));
                }
                None => {
                    // `{...plugins, dirs, ...{}}`：原有非对象的 enabledPlugins 原样保留。
                }
            }
            config.insert("plugins".into(), Value::Object(plugins));
            Ok(Update::Write(js::pretty_line(&Value::Object(config)), ()))
        })
        .await
    }

    pub async fn import_plugins(&self, params: Value) -> Result<Value, RpcError> {
        if params.get("overwrite").is_some_and(truthy) {
            return Err(fsx::error("plugin sync overwrite is not supported"));
        }
        let bytes = bytes_param(params.get("archive"))?;
        let max = self.max_archive_bytes;
        if bytes.len() as u64 > max {
            return Err(fsx::error(format!(
                "plugin sync archive exceeds limit: {}/{max}",
                bytes.len()
            )));
        }
        let temp = fsx::join(
            &fsx::path_str(&std::env::temp_dir()),
            &[&format!("zcode-plugin-sync-{}", fsx::random_uuid())],
        );
        let result = self.import_plugins_into(&bytes, &temp).await;
        let _ = fsx::remove_all(&temp).await;
        result
    }

    async fn import_plugins_into(&self, bytes: &[u8], temp: &str) -> Result<Value, RpcError> {
        archive::extract(Flavor::Plugin, bytes, temp, Some(self.max_archive_bytes)).await?;
        let metadata = Self::read_metadata(temp).await?;
        let Some(items) = metadata
            .as_object()
            .and_then(|map| map.get("plugins"))
            .and_then(Value::as_array)
        else {
            return Err(fsx::error("invalid plugin sync archive metadata"));
        };
        let mut plugins = Vec::new();
        for item in items {
            let Some(item) = item.as_object() else {
                return Err(fsx::error("invalid plugin sync archive plugin metadata"));
            };
            let name = required(item.get("name"), "plugin metadata name")?;
            let id = plugin_id(&required(item.get("pluginId"), "plugin metadata id")?)?;
            let directory_name = archive::normalize_relative(
                &required(item.get("directoryName"), "plugin metadata directory")?,
                SYNC_LABEL,
            )?;
            plugins.push((
                name,
                id,
                directory_name,
                item.get("enabled").and_then(Value::as_bool),
            ));
        }
        let target_root = self.plugin_root();
        let mut existing = self.configured_paths_by_id().await?;
        let mut results = Vec::new();
        for (name, id, directory_name, enabled) in plugins {
            let extracted = archive::resolve_within(temp, &directory_name, SYNC_LABEL)?;
            let target = archive::resolve_within(&target_root, &directory_name, SYNC_LABEL)?;
            let mut staging: Option<String> = None;
            let attempt: Result<Value, RpcError> = async {
                let manifest = read_manifest(&extracted)
                    .await
                    .ok_or_else(|| fsx::error(format!("plugin manifest not found: {directory_name}")))?;
                if plugin_id_key(&manifest.plugin_id)? != plugin_id_key(&id)? {
                    return Err(fsx::error(format!(
                        "plugin manifest id mismatch: {} !== {id}",
                        manifest.plugin_id
                    )));
                }
                if std::path::Path::new(&target).exists() {
                    return Ok(json!({ "name": manifest.name, "pluginId": id, "directoryName": directory_name, "status": "skipped", "path": target }));
                }
                if let Some(path) = existing.get(&plugin_id_key(&id)?) {
                    return Ok(json!({ "name": manifest.name, "pluginId": id, "directoryName": directory_name, "status": "skipped", "path": path }));
                }
                fsx::mkdirp(&fsx::dirname(&target)).await?;
                let stage = archive::resolve_within(
                    &target_root,
                    &format!(".importing-{}-{}", safe_directory_name(&directory_name), fsx::random_uuid()),
                    SYNC_LABEL,
                )?;
                staging = Some(stage.clone());
                fsx::copy_recursive(&extracted, &stage, false).await?;
                if std::path::Path::new(&target).exists() {
                    return Err(fsx::error(format!("plugin target already exists: {target}")));
                }
                tokio::fs::rename(&stage, &target)
                    .await
                    .map_err(|error| fsx::fs_error(&error, "rename", &stage))?;
                staging = None;
                // 只落目录不会被 Agent 发现：同一服务内追加远端绝对路径；写失败回滚目录，避免半同步。
                if let Err(error) = self.add_plugin_dir(&target, &id, enabled).await {
                    let _ = fsx::remove_all(&target).await;
                    return Err(error);
                }
                existing.insert(plugin_id_key(&id)?, target.clone());
                Ok(json!({ "name": manifest.name, "pluginId": id, "directoryName": directory_name, "status": "synced", "path": target }))
            }
            .await;
            match attempt {
                Ok(result) => results.push(result),
                Err(error) => {
                    if let Some(stage) = &staging {
                        let _ = fsx::remove_all(stage).await;
                    }
                    results.push(json!({
                        "name": name,
                        "pluginId": id,
                        "directoryName": directory_name,
                        "status": "failed",
                        "error": error.message,
                    }));
                }
            }
        }
        Ok(json!({ "results": results }))
    }

    pub async fn import_marketplace_source(&self, params: Value) -> Result<Value, RpcError> {
        if params.get("overwrite").is_some_and(truthy) {
            return Err(fsx::error(
                "plugin marketplace source overwrite is not supported",
            ));
        }
        let bytes = bytes_param(params.get("archive"))?;
        let max = self.max_archive_bytes;
        if bytes.len() as u64 > max {
            return Err(fsx::error(format!(
                "plugin marketplace source archive exceeds limit: {}/{max}",
                bytes.len()
            )));
        }
        let temp = fsx::join(
            &fsx::path_str(&std::env::temp_dir()),
            &[&format!(
                "zcode-plugin-marketplace-source-{}",
                fsx::random_uuid()
            )],
        );
        let result = self.import_marketplace_into(&bytes, &temp).await;
        let _ = fsx::remove_all(&temp).await;
        result
    }

    async fn import_marketplace_into(&self, bytes: &[u8], temp: &str) -> Result<Value, RpcError> {
        archive::extract(Flavor::Plugin, bytes, temp, Some(self.max_archive_bytes)).await?;
        let metadata = Self::read_metadata(temp).await?;
        let Some(sources) = metadata
            .as_object()
            .and_then(|map| map.get("marketplaceSources"))
            .and_then(Value::as_array)
        else {
            return Err(fsx::error(
                "invalid plugin marketplace source archive metadata",
            ));
        };
        if sources.len() != 1 {
            return Err(fsx::error(
                "plugin marketplace source archive must contain one marketplace",
            ));
        }
        let Some(item) = sources[0].as_object() else {
            return Err(fsx::error("invalid plugin marketplace source metadata"));
        };
        let marketplace_id = plugin_id(&required(
            item.get("marketplaceId"),
            "marketplace source id",
        )?)?;
        let directory_name = archive::normalize_relative(
            &required(item.get("directoryName"), "marketplace source directory")?,
            SYNC_LABEL,
        )?;
        let extracted = archive::resolve_within(temp, &directory_name, SYNC_LABEL)?;
        let manifest = read_marketplace(&fsx::join(&extracted, &["marketplace.json"])).await?;
        if manifest.name != marketplace_id {
            return Err(fsx::error(format!(
                "marketplace source archive id mismatch: {} !== {marketplace_id}",
                manifest.name
            )));
        }
        let target_root = fsx::join(&self.plugin_root(), &[MARKETPLACE_SOURCES_DIR]);
        let target = archive::resolve_within(&target_root, &directory_name, SYNC_LABEL)?;
        if std::path::Path::new(&target).exists() {
            return Ok(
                json!({ "marketplaceId": marketplace_id, "path": target, "status": "skipped" }),
            );
        }
        fsx::mkdirp(&fsx::dirname(&target)).await?;
        fsx::copy_recursive(&extracted, &target, false).await?;
        Ok(json!({ "marketplaceId": marketplace_id, "path": target, "status": "synced" }))
    }

    pub async fn check_write_access(&self) -> Value {
        let directories = [
            self.plugin_root(),
            fsx::dirname(&self.roots.cli_config_path()),
        ];
        for directory in &directories {
            let result = check_write_access(directory).await;
            if result["ok"] != true {
                return result;
            }
        }
        json!({ "ok": true, "path": directories.join(", ") })
    }
}

/// `resolveMarketplacePluginClosure`：选中插件及其同市场依赖（深度优先，去重）。
fn closure(
    manifest: &Marketplace,
    marketplace_id: &str,
    names: &[String],
) -> Result<Vec<MarketplaceEntry>, RpcError> {
    let mut by_name: HashMap<String, MarketplaceEntry> = HashMap::new();
    for entry in &manifest.plugins {
        by_name.insert(entry.name.clone(), entry.clone());
    }
    let mut visited = HashSet::new();
    let mut result = Vec::new();
    fn visit(
        name: &str,
        required_by: &str,
        marketplace_id: &str,
        by_name: &HashMap<String, MarketplaceEntry>,
        visited: &mut HashSet<String>,
        result: &mut Vec<MarketplaceEntry>,
    ) -> Result<(), RpcError> {
        let normalized = required(Some(&json!(name)), "marketplace plugin name")?;
        if visited.contains(&normalized) {
            return Ok(());
        }
        let entry = by_name
            .get(&normalized)
            .ok_or_else(|| fsx::error(format!("marketplace plugin not found: {normalized}")))?
            .clone();
        visited.insert(normalized);
        result.push(entry.clone());
        for dependency in &entry.dependencies {
            let trimmed = js_trim(dependency);
            let dependency_name = match trimmed.rfind('@') {
                None => trimmed.to_owned(),
                Some(at) if &trimmed[at + 1..] == marketplace_id => trimmed[..at].to_owned(),
                Some(_) => {
                    return Err(fsx::error(format!(
                        "cross-marketplace dependency is not supported for SSH source mirror: {dependency} required by {required_by}"
                    )));
                }
            };
            visit(
                &dependency_name,
                &entry.name,
                marketplace_id,
                by_name,
                visited,
                result,
            )?;
        }
        Ok(())
    }
    for name in names {
        visit(
            name,
            name,
            marketplace_id,
            &by_name,
            &mut visited,
            &mut result,
        )?;
    }
    Ok(result)
}

/// `buildMirroredMarketplaceManifest`。
fn mirrored_manifest(manifest: &Marketplace, mirrors: &[Mirror]) -> Value {
    let has_mirrored = mirrors.iter().any(|mirror| mirror.relative_path.is_some());
    let mut result = manifest.raw.clone();
    js::delete(&mut result, "pluginRoot");
    result.insert("name".into(), json!(manifest.name));
    if let Some(description) = manifest
        .description
        .as_ref()
        .filter(|text| !text.is_empty())
    {
        result.insert("description".into(), json!(description));
    }
    if has_mirrored {
        // agent 只读 metadata.pluginRoot；写顶层 pluginRoot 会把 ./x 解析到市场根目录。
        let mut metadata = js::record(manifest.raw.get("metadata"));
        metadata.insert("pluginRoot".into(), json!(MIRROR_ROOT));
        result.insert("metadata".into(), Value::Object(metadata));
    }
    let plugins: Vec<Value> = mirrors
        .iter()
        .map(|mirror| {
            let mut raw = mirror.entry.raw.clone();
            raw.insert("name".into(), json!(mirror.entry.name));
            if let Some(relative) = &mirror.relative_path {
                let rest: Vec<&str> = relative.split('/').skip(1).collect();
                raw.insert("source".into(), json!(format!("./{}", rest.join("/"))));
            }
            Value::Object(raw)
        })
        .collect();
    result.insert("plugins".into(), Value::Array(plugins));
    Value::Object(result)
}

pub fn service(sync: PluginSyncService) -> ServiceBuilder {
    let builder = ServiceBuilder::new(CHANNEL);
    let builder = bind(
        builder,
        "listLocalUserPluginCandidates",
        &sync,
        |s, _| async move { s.list_local_candidates().await.map(Reply::Value) },
    );
    let builder = bind(
        builder,
        "listRemoteUserPluginStatuses",
        &sync,
        |s, p| async move { s.list_remote_statuses(p).await.map(Reply::Value) },
    );
    let builder = bind(builder, "exportPluginsArchive", &sync, |s, p| async move {
        s.export_plugins(p).await.map(Reply::Value)
    });
    let builder = bind(
        builder,
        "exportMarketplaceSourceArchive",
        &sync,
        |s, p| async move { s.export_marketplace_source(p).await.map(Reply::Value) },
    );
    let builder = bind(builder, "importPluginsArchive", &sync, |s, p| async move {
        s.import_plugins(p).await.map(Reply::Value)
    });
    let builder = bind(
        builder,
        "checkRemoteUserPluginWriteAccess",
        &sync,
        |s, _| async move { Ok(Reply::Value(s.check_write_access().await)) },
    );
    bind(
        builder,
        "importMarketplaceSourceArchive",
        &sync,
        |s, p| async move { s.import_marketplace_source(p).await.map(Reply::Value) },
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn write_plugin(root: &std::path::Path, name: &str) {
        std::fs::create_dir_all(root.join(".zcode-plugin")).unwrap();
        std::fs::create_dir_all(root.join("skills/demo")).unwrap();
        std::fs::write(
            root.join(".zcode-plugin/plugin.json"),
            format!("{{\"name\":\"{name}\",\"version\":\" 1.0 \"}}"),
        )
        .unwrap();
        std::fs::write(root.join("skills/demo/SKILL.md"), "x").unwrap();
    }

    #[tokio::test]
    async fn export_import_registers_plugin_dir() {
        let home = tempfile::tempdir().unwrap();
        let plugin = home.path().join("src/my-plugin");
        write_plugin(&plugin, "my-plugin");
        std::fs::create_dir_all(home.path().join(".zcode/cli")).unwrap();
        std::fs::write(
            home.path().join(".zcode/cli/config.json"),
            format!(
                "{{\"plugins\":{{\"dirs\":[\"{}\"],\"enabledPlugins\":{{\"my-plugin@inline\":false}}}}}}",
                plugin.display()
            ),
        )
        .unwrap();
        let local = PluginSyncService::new(Roots::new(home.path(), home.path()));
        let list = local.list_local_candidates().await.unwrap();
        let candidate = &list["candidates"][0];
        assert_eq!(candidate["version"], "1.0");
        assert_eq!(candidate["enabled"], false);
        assert_eq!(candidate["componentTypes"], json!(["skills"]));
        let exported = local
            .export_plugins(json!({ "pluginIds": [candidate["id"]] }))
            .await
            .unwrap();
        let remote_home = tempfile::tempdir().unwrap();
        let remote = PluginSyncService::new(Roots::new(remote_home.path(), remote_home.path()));
        let imported = remote
            .import_plugins(json!({ "archive": exported["archive"] }))
            .await
            .unwrap();
        assert_eq!(imported["results"][0]["status"], "synced", "{imported}");
        let config =
            std::fs::read_to_string(remote_home.path().join(".zcode/cli/config.json")).unwrap();
        let config: Value = serde_json::from_str(&config).unwrap();
        assert_eq!(
            config["plugins"]["enabledPlugins"]["my-plugin@inline"],
            false
        );
        assert_eq!(config["plugins"]["dirs"].as_array().unwrap().len(), 1);
        let again = remote
            .import_plugins(json!({ "archive": exported["archive"] }))
            .await
            .unwrap();
        assert_eq!(again["results"][0]["status"], "skipped");
    }

    #[tokio::test]
    async fn marketplace_source_mirror_round_trip() {
        let home = tempfile::tempdir().unwrap();
        let market = home.path().join("market");
        write_plugin(&market.join("plugins/a"), "a");
        write_plugin(&market.join("plugins/b"), "b");
        std::fs::write(
            market.join("marketplace.json"),
            r#"{"name":"m","pluginRoot":"plugins","plugins":[{"name":"a","source":"./a","dependencies":["b@m"]},{"name":"b"}]}"#,
        )
        .unwrap();
        let service = PluginSyncService::new(Roots::new(home.path(), home.path()));
        let exported = service
            .export_marketplace_source(json!({
                "marketplaceId": "m",
                "pluginNames": ["a"],
                "source": { "source": "directory", "path": market.display().to_string() },
            }))
            .await
            .unwrap();
        assert_eq!(exported["pluginNames"], json!(["a", "b"]));
        let remote_home = tempfile::tempdir().unwrap();
        let remote = PluginSyncService::new(Roots::new(remote_home.path(), remote_home.path()));
        let imported = remote
            .import_marketplace_source(json!({ "archive": exported["archive"] }))
            .await
            .unwrap();
        assert_eq!(imported["status"], "synced");
        let manifest: Value = serde_json::from_str(
            &std::fs::read_to_string(format!(
                "{}/marketplace.json",
                imported["path"].as_str().unwrap()
            ))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["metadata"]["pluginRoot"], "plugins");
        assert_eq!(manifest["plugins"][0]["source"], "./a");
        assert!(manifest.get("pluginRoot").is_none());
    }
}
