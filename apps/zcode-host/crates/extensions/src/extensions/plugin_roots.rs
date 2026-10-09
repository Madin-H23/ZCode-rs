//! 启用插件的资源目录发现（skillsService.ts / commandsService.ts 的 `resolvePlugin*RootDescriptors`）。
//!
//! 候选顺序：`plugins.dirs`（inline，默认启用）→ 官方 cache（默认按内置名单）→
//! `installed_plugins.json`；按 pluginId 先到先得，被“卸载”的内置官方插件（suppressedBuiltins）跳过。
use serde_json::{Map, Value};

use super::Roots;
use super::fsx;
use super::json;
use crate::support::collate::locale_compare;

pub const OFFICIAL_MARKETPLACE: &str = "zcode-plugins-official";
pub const INLINE_MARKETPLACE: &str = "inline";

/// `DEFAULT_ENABLED_OFFICIAL_PLUGIN_IDS`（packages/shared/src/plugin-marketplaces.ts）。
pub const DEFAULT_ENABLED_OFFICIAL_PLUGIN_IDS: [&str; 10] = [
    "browser-use@zcode-plugins-official",
    "image-search@zcode-plugins-official",
    "documents@zcode-plugins-official",
    "pdf@zcode-plugins-official",
    "presentations@zcode-plugins-official",
    "spreadsheets@zcode-plugins-official",
    "node-repl-host@zcode-plugins-official",
    "skill-creator@zcode-plugins-official",
    "plugin-creator@zcode-plugins-official",
    "zcode-guide@zcode-plugins-official",
];

pub fn default_enabled(plugin_id: &str) -> bool {
    DEFAULT_ENABLED_OFFICIAL_PLUGIN_IDS.contains(&plugin_id)
}

/// `readPluginConfigFromConfig`。
#[derive(Debug, Clone)]
pub struct PluginConfig {
    pub dirs: Vec<String>,
    pub enabled: bool,
    pub enabled_plugins: Vec<(String, bool)>,
    pub storage_dir: String,
    pub suppressed_builtins: Vec<String>,
}

impl PluginConfig {
    pub fn from_config(config: &Map<String, Value>) -> Self {
        let plugins = json::record(config.get("plugins"));
        Self {
            dirs: json::string_array(plugins.get("dirs")),
            enabled: plugins
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            enabled_plugins: json::boolean_record(plugins.get("enabledPlugins")),
            storage_dir: storage_dir(config),
            suppressed_builtins: json::string_array(plugins.get("suppressedBuiltins")),
        }
    }

    pub fn enabled_override(&self, plugin_id: &str) -> Option<bool> {
        self.enabled_plugins
            .iter()
            .find(|(id, _)| id == plugin_id)
            .map(|(_, enabled)| *enabled)
    }
}

/// `storage.dir`（非空字符串）或 `~/.zcode`。
pub fn storage_dir(config: &Map<String, Value>) -> String {
    let storage = json::record(config.get("storage"));
    match storage.get("dir").and_then(Value::as_str) {
        Some(dir) if !dir.trim().is_empty() => dir.to_owned(),
        _ => "~/.zcode".to_owned(),
    }
}

/// `resolveCliStorageRoot`：末段不是 `cli` 时追加 `cli`。
pub fn cli_storage_root(roots: &Roots, storage_dir: &str) -> String {
    let root = roots.resolve_config_path(storage_dir);
    if fsx::basename(&root) == "cli" {
        root
    } else {
        fsx::join(&root, &["cli"])
    }
}

pub fn plugin_storage_root(roots: &Roots, storage_dir: &str) -> String {
    fsx::join(&cli_storage_root(roots, storage_dir), &["plugins"])
}

/// `<plugins>/cache/zcode-plugins-official/<name>/<version>`，按 localeCompare 排序。
pub async fn scan_official_cache_roots(plugin_storage_root: &str) -> Vec<String> {
    let cache_root = fsx::join(plugin_storage_root, &["cache", OFFICIAL_MARKETPLACE]);
    let Ok(plugins) = fsx::read_dir(&cache_root).await else {
        return Vec::new();
    };
    let mut roots = Vec::new();
    for plugin in plugins.iter().filter(|entry| entry.is_dir) {
        let plugin_dir = fsx::join(&cache_root, &[&plugin.name]);
        let Ok(versions) = fsx::read_dir(&plugin_dir).await else {
            continue;
        };
        for version in versions.iter().filter(|entry| entry.is_dir) {
            roots.push(fsx::join(&plugin_dir, &[&version.name]));
        }
    }
    roots.sort_by(|left, right| locale_compare(left, right));
    roots
}

/// `installed_plugins.json` 中合法的记录（`installedPluginRoots.ts`）。
#[derive(Debug, Clone)]
pub struct InstalledRoot {
    pub id: String,
    pub marketplace: String,
    pub install_path: String,
}

pub async fn read_installed_roots(plugin_storage_root: &str) -> Vec<InstalledRoot> {
    let path = fsx::join(plugin_storage_root, &["installed_plugins.json"]);
    let Some(raw) = fsx::read_text_opt(&path).await else {
        return Vec::new();
    };
    let Some(parsed) = json::parse_record(&raw) else {
        return Vec::new();
    };
    let Some(Value::Array(items)) = parsed.get("plugins") else {
        return Vec::new();
    };
    let field = |item: &Map<String, Value>, key: &str| {
        item.get(key)
            .and_then(Value::as_str)
            .map(|value| zcode_kit::js::string::trim(value).to_owned())
            .unwrap_or_default()
    };
    items
        .iter()
        .filter_map(Value::as_object)
        .filter_map(|item| {
            let id = field(item, "id");
            let marketplace = field(item, "marketplace");
            let install_path = field(item, "installPath");
            (!id.is_empty()
                && !marketplace.is_empty()
                && !install_path.is_empty()
                && fsx::is_absolute(&install_path))
            .then_some(InstalledRoot {
                id,
                marketplace,
                install_path,
            })
        })
        .collect()
}

const MANIFEST_PATHS: [[&str; 2]; 3] = [
    [".zcode-plugin", "plugin.json"],
    [".claude-plugin", "plugin.json"],
    [".codex-plugin", "plugin.json"],
];

/// 第一个存在的 manifest 路径（后续路径只在前者不存在时才看）。
pub async fn find_manifest_path(root: &str) -> Option<String> {
    for segments in MANIFEST_PATHS {
        let path = fsx::join(root, &segments);
        if fsx::exists(&path).await {
            return Some(path);
        }
    }
    None
}

/// `readPluginManifest`：manifest 必须是对象且 name 合法（`^[a-z0-9][a-z0-9._-]{0,127}$`）。
pub async fn read_manifest(root: &str) -> Option<(String, Map<String, Value>)> {
    let path = find_manifest_path(root).await?;
    let raw = fsx::read_text_opt(&path).await?;
    let manifest = json::parse_record(&raw)?;
    let name = manifest
        .get("name")
        .and_then(Value::as_str)
        .map(|name| zcode_kit::js::string::trim(name).to_owned())
        .unwrap_or_default();
    valid_plugin_name(&name).then_some((name, manifest))
}

pub fn valid_plugin_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes[1..].iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

/// `parsePathList`：字符串视为单元素列表。
fn path_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(text)) => vec![text.clone()],
        other => json::string_array(other),
    }
}

/// `resolveInside`：只接受落在 root 内（含 root 本身）的相对路径。
pub fn resolve_inside(root: &str, raw: &str) -> Option<String> {
    if fsx::is_absolute(raw) {
        return None;
    }
    let resolved = fsx::resolve(root, &[raw]);
    let relative = fsx::relative(root, &resolved);
    let sep = std::path::MAIN_SEPARATOR;
    (relative.is_empty()
        || (!relative.starts_with("..") && !relative.contains(&format!("..{sep}"))))
    .then_some(resolved)
}

/// manifest 的 `skills` / `commands` 字段声明的目录；未声明时默认 `<root>/<key>`（存在才算）。
pub fn component_roots(root: &str, manifest: &Map<String, Value>, key: &str) -> Vec<String> {
    let mut roots: Vec<String> = path_list(manifest.get(key))
        .iter()
        .filter_map(|raw| resolve_inside(root, raw))
        .collect();
    if roots.is_empty() && !manifest.contains_key(key) {
        let default = fsx::join(root, &[key]);
        if std::path::Path::new(&default).exists() {
            roots.push(default);
        }
    }
    roots
}

/// 一个启用插件贡献的资源目录。
#[derive(Debug, Clone)]
pub struct ComponentRoot {
    pub plugin_id: String,
    pub plugin_name: String,
    pub marketplace: String,
    pub root_path: String,
}

/// 启用插件在 `key`（skills / commands）上的目录。插件总开关关闭时为空。
pub async fn enabled_component_roots(
    roots: &Roots,
    config: &Map<String, Value>,
    key: &str,
) -> Vec<ComponentRoot> {
    let config = PluginConfig::from_config(config);
    if !config.enabled {
        return Vec::new();
    }
    let storage = plugin_storage_root(roots, &config.storage_dir);
    let mut candidates: Vec<(bool, String, String)> = config
        .dirs
        .iter()
        .map(|dir| {
            (
                true,
                INLINE_MARKETPLACE.to_owned(),
                roots.resolve_config_path(dir),
            )
        })
        .collect();
    for root in scan_official_cache_roots(&storage).await {
        candidates.push((false, OFFICIAL_MARKETPLACE.to_owned(), root));
    }
    for installed in read_installed_roots(&storage).await {
        candidates.push((false, installed.marketplace, installed.install_path));
    }
    let mut seen: Vec<String> = Vec::new();
    let mut result = Vec::new();
    for (default_on, marketplace, root) in candidates {
        let Some((name, manifest)) = read_manifest(&root).await else {
            continue;
        };
        let plugin_id = format!("{name}@{marketplace}");
        if marketplace == OFFICIAL_MARKETPLACE && config.suppressed_builtins.contains(&plugin_id) {
            continue;
        }
        if seen.contains(&plugin_id) {
            continue;
        }
        seen.push(plugin_id.clone());
        let enabled = config
            .enabled_override(&plugin_id)
            .unwrap_or(default_on || default_enabled(&plugin_id));
        if !enabled {
            continue;
        }
        for root_path in component_roots(&root, &manifest, key) {
            result.push(ComponentRoot {
                plugin_id: plugin_id.clone(),
                plugin_name: name.clone(),
                marketplace: marketplace.clone(),
                root_path,
            });
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_names_and_inside_paths() {
        assert!(valid_plugin_name("my-plugin.v2_x"));
        assert!(!valid_plugin_name("My"));
        assert!(!valid_plugin_name("-x"));
        #[cfg(unix)]
        {
            assert_eq!(
                resolve_inside("/p", "./skills").as_deref(),
                Some("/p/skills")
            );
            assert_eq!(resolve_inside("/p", ".").as_deref(), Some("/p"));
            assert_eq!(resolve_inside("/p", "../x"), None);
            assert_eq!(resolve_inside("/p", "/abs"), None);
        }
    }
}
