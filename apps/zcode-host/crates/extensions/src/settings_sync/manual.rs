//! settings-sync 的“从其他 Agent 手动导入”（settingsSyncService.ts 的 `detect` 带 categories 与
//! `importSelected`；docs/specs/rust-host-integration.md §2.2）。
//!
//! 扫描外部 Agent 的 skills / commands / plugins / MCP，按目标根或目标配置判定冲突，导入时软链或
//! 复制到 `~/.zcode` / `<workspace>/.zcode`。`config.json` 的写入经 extensions 区域的
//! `cli_config::update`（跨进程锁 + 原子写），与其他写入方共用一条写路径。
use std::collections::{HashMap, HashSet};

use regex::Regex;
use serde_json::{Map, Value, json};
use zcode_host_kit::{RpcError, log};
use zcode_kit::js::string::trim as js_trim;

use crate::extensions::{cli_config, commands, fsx, json as js, walk, yaml};
use crate::support::collate::locale_compare;

type Agent = &'static str;

/// 来源表的一行：项目内路径与用户级路径。
struct PathSource {
    agent: Agent,
    project: &'static [&'static str],
    global: &'static [&'static str],
}

const fn source(
    agent: Agent,
    project: &'static [&'static str],
    global: &'static [&'static str],
) -> PathSource {
    PathSource {
        agent,
        project,
        global,
    }
}

/// `SUPPORTED_SKILL_AGENT_SOURCES`。
const SKILL_SOURCES: [PathSource; 16] = [
    source("claudeCode", &[".claude", "skills"], &[".claude", "skills"]),
    source("codexCli", &[".codex", "skills"], &[".codex", "skills"]),
    source(
        "openCode",
        &[".opencode", "skills"],
        &[".config", "opencode", "skills"],
    ),
    source("openClaw", &["skills"], &[".openclaw", "skills"]),
    source("augment", &[".augment", "skills"], &[".augment", "skills"]),
    source(
        "continue",
        &[".continue", "skills"],
        &[".continue", "skills"],
    ),
    source(
        "goose",
        &[".goose", "skills"],
        &[".config", "goose", "skills"],
    ),
    source("qwenCode", &[".qwen", "skills"], &[".qwen", "skills"]),
    source("qode", &[".qoder", "skills"], &[".qoder", "skills"]),
    source("qodeCn", &[".qoder", "skills"], &[".qoder-cn", "skills"]),
    source(
        "windsurf",
        &[".windsurf", "skills"],
        &[".codeium", "windsurf", "skills"],
    ),
    source("trae", &[".trae", "skills"], &[".trae", "skills"]),
    source("traeCn", &[".trae", "skills"], &[".trae-cn", "skills"]),
    source("kiroCli", &[".kiro", "skills"], &[".kiro", "skills"]),
    source("roo", &[".roo", "skills"], &[".roo", "skills"]),
    source(
        "codeBuddy",
        &[".codebuddy", "skills"],
        &[".codebuddy", "skills"],
    ),
];

/// `SUPPORTED_COMMAND_AGENT_SOURCES`。
const COMMAND_SOURCES: [PathSource; 15] = [
    source(
        "claudeCode",
        &[".claude", "commands"],
        &[".claude", "commands"],
    ),
    source("codexCli", &[".codex", "commands"], &[".codex", "commands"]),
    source(
        "openCode",
        &[".opencode", "commands"],
        &[".config", "opencode", "commands"],
    ),
    source("openClaw", &["commands"], &[".openclaw", "commands"]),
    source(
        "augment",
        &[".augment", "commands"],
        &[".augment", "commands"],
    ),
    source(
        "continue",
        &[".continue", "commands"],
        &[".continue", "commands"],
    ),
    source(
        "goose",
        &[".goose", "commands"],
        &[".config", "goose", "commands"],
    ),
    source("qwenCode", &[".qwen", "commands"], &[".qwen", "commands"]),
    source("qode", &[".qoder", "commands"], &[".qoder", "commands"]),
    source(
        "qodeCn",
        &[".qoder", "commands"],
        &[".qoder-cn", "commands"],
    ),
    source(
        "windsurf",
        &[".windsurf", "commands"],
        &[".codeium", "windsurf", "commands"],
    ),
    source("trae", &[".trae", "commands"], &[".trae", "commands"]),
    source("kiroCli", &[".kiro", "commands"], &[".kiro", "commands"]),
    source("roo", &[".roo", "commands"], &[".roo", "commands"]),
    source(
        "codeBuddy",
        &[".codebuddy", "commands"],
        &[".codebuddy", "commands"],
    ),
];

/// `SUPPORTED_PLUGIN_AGENT_SOURCES`。
const PLUGIN_SOURCES: [PathSource; 15] = [
    source(
        "claudeCode",
        &[".claude", "plugins"],
        &[".claude", "plugins"],
    ),
    source("codexCli", &[".codex", "plugins"], &[".codex", "plugins"]),
    source(
        "openCode",
        &[".opencode", "plugins"],
        &[".config", "opencode", "plugins"],
    ),
    source("openClaw", &["plugins"], &[".openclaw", "plugins"]),
    source(
        "augment",
        &[".augment", "plugins"],
        &[".augment", "plugins"],
    ),
    source(
        "continue",
        &[".continue", "plugins"],
        &[".continue", "plugins"],
    ),
    source(
        "goose",
        &[".goose", "plugins"],
        &[".config", "goose", "plugins"],
    ),
    source("qwenCode", &[".qwen", "plugins"], &[".qwen", "plugins"]),
    source("qode", &[".qoder", "plugins"], &[".qoder", "plugins"]),
    source("qodeCn", &[".qoder", "plugins"], &[".qoder-cn", "plugins"]),
    source(
        "windsurf",
        &[".windsurf", "plugins"],
        &[".codeium", "windsurf", "plugins"],
    ),
    source("trae", &[".trae", "plugins"], &[".trae", "plugins"]),
    source("kiroCli", &[".kiro", "plugins"], &[".kiro", "plugins"]),
    source("roo", &[".roo", "plugins"], &[".roo", "plugins"]),
    source(
        "codeBuddy",
        &[".codebuddy", "plugins"],
        &[".codebuddy", "plugins"],
    ),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum McpFormat {
    /// `{ mcpServers: {…} }`。
    ServersJson,
    /// OpenCode：`{ mcp: {…} }`，command 可能是数组。
    OpenCodeJson,
    /// Codex：`config.toml` 的 `mcp_servers`。
    CodexToml,
}

struct McpSource {
    agent: Agent,
    project: &'static [&'static [&'static str]],
    global: &'static [&'static [&'static str]],
    format: McpFormat,
}

const fn mcp(
    agent: Agent,
    project: &'static [&'static [&'static str]],
    global: &'static [&'static [&'static str]],
    format: McpFormat,
) -> McpSource {
    McpSource {
        agent,
        project,
        global,
        format,
    }
}

/// `SUPPORTED_MCP_AGENT_SOURCES`。
const MCP_SOURCES: [McpSource; 12] = [
    mcp(
        "claudeCode",
        &[&[".claude", "settings.json"], &[".mcp.json"]],
        &[&[".claude", "settings.json"]],
        McpFormat::ServersJson,
    ),
    mcp(
        "codexCli",
        &[&[".codex", "config.toml"]],
        &[&[".codex", "config.toml"]],
        McpFormat::CodexToml,
    ),
    mcp(
        "openCode",
        &[&[".opencode", "opencode.json"]],
        &[&[".config", "opencode", "opencode.json"]],
        McpFormat::OpenCodeJson,
    ),
    mcp(
        "openClaw",
        &[&["settings.json"]],
        &[&[".openclaw", "settings.json"]],
        McpFormat::ServersJson,
    ),
    mcp(
        "qwenCode",
        &[&[".qwen", "settings.json"]],
        &[&[".qwen", "settings.json"]],
        McpFormat::ServersJson,
    ),
    mcp(
        "qode",
        &[&[".qoder", "settings.json"]],
        &[&[".qoder", "settings.json"]],
        McpFormat::ServersJson,
    ),
    mcp(
        "qodeCn",
        &[&[".qoder", "settings.json"]],
        &[&[".qoder-cn", "settings.json"]],
        McpFormat::ServersJson,
    ),
    mcp(
        "trae",
        &[&[".trae", "settings.json"]],
        &[&[".trae", "settings.json"]],
        McpFormat::ServersJson,
    ),
    mcp(
        "kiroCli",
        &[&[".kiro", "settings.json"]],
        &[&[".kiro", "settings.json"]],
        McpFormat::ServersJson,
    ),
    mcp(
        "roo",
        &[&[".roo", "settings.json"]],
        &[&[".roo", "settings.json"]],
        McpFormat::ServersJson,
    ),
    mcp(
        "codeBuddy",
        &[&[".codebuddy", "settings.json"]],
        &[&[".codebuddy", "settings.json"]],
        McpFormat::ServersJson,
    ),
    mcp(
        "agents",
        &[&[".agents", "mcp.json"]],
        &[&[".agents", "mcp.json"]],
        McpFormat::ServersJson,
    ),
];

const PLUGIN_MANIFESTS: [[&str; 2]; 3] = [
    [".zcode-plugin", "plugin.json"],
    [".claude-plugin", "plugin.json"],
    [".codex-plugin", "plugin.json"],
];
const INLINE_PLUGIN_MARKETPLACE: &str = "inline";

/// 来源 scope：用户级根为 `global`，项目内根为 `project`（`SettingsSyncSourceScope`）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Scope {
    Global,
    Project,
}

impl Scope {
    fn as_str(self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Project => "project",
        }
    }

    fn parse(value: &Value) -> Option<Self> {
        match value.as_str() {
            Some("global") => Some(Scope::Global),
            Some("project") => Some(Scope::Project),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ImportMode {
    Copy,
    Symlink,
}

/// 扫描与导入共用的路径根。
pub struct Paths<'a> {
    pub home: &'a str,
    pub workspace: Option<&'a str>,
}

impl Paths<'_> {
    fn zcode_root(&self, scope: Scope, leaf: &str) -> Option<String> {
        match scope {
            Scope::Global => Some(fsx::join(self.home, &[".zcode", leaf])),
            Scope::Project => self
                .workspace
                .map(|workspace| fsx::join(workspace, &[".zcode", leaf])),
        }
    }

    /// `resolvePluginConfigPathForScope` / `resolveMcpConfigPathForScope`。
    fn config_path(&self, scope: Scope) -> Option<String> {
        match scope {
            Scope::Global => Some(fsx::join(self.home, &[".zcode", "cli", "config.json"])),
            Scope::Project => self
                .workspace
                .map(|workspace| fsx::join(workspace, &[".zcode", "config.json"])),
        }
    }

    /// 用户级根在前，带 workspace 时再加项目内根（全局导入不依赖当前窗口打开 workspace）。
    fn roots<'s>(&self, sources: &'s [PathSource]) -> Vec<(&'s PathSource, String, Scope)> {
        let mut roots: Vec<_> = sources
            .iter()
            .map(|source| (source, fsx::join(self.home, source.global), Scope::Global))
            .collect();
        if let Some(workspace) = self.workspace {
            roots.extend(
                sources
                    .iter()
                    .map(|source| (source, fsx::join(workspace, source.project), Scope::Project)),
            );
        }
        roots
    }
}

// ── 文件系统小工具 ──

/// `exists`：能 readdir（目录，跟随软链）。
async fn dir_exists(path: &str) -> bool {
    tokio::fs::read_dir(path).await.is_ok()
}

/// `pathExists`：lstat 成功。
async fn path_exists(path: &str) -> bool {
    tokio::fs::symlink_metadata(path).await.is_ok()
}

fn sort_locale(items: &mut [String]) {
    items.sort_by(|left, right| locale_compare(left, right));
}

fn name_key(name: &str) -> String {
    js_trim(name).to_lowercase()
}

/// 解析结果按 JS 读取：对象（`isRecord`）或空。
async fn read_record(path: &str) -> Map<String, Value> {
    fsx::read_text_opt(path)
        .await
        .and_then(|raw| js::parse_record(&raw))
        .unwrap_or_default()
}

// ── skills ──

struct SkillMeta {
    name: String,
    version: Option<String>,
}

fn strip_scalar_quotes(value: &str) -> String {
    let trimmed = js_trim(value);
    let quoted = |quote: char| trimmed.starts_with(quote) && trimmed.ends_with(quote);
    if quoted('"') || quoted('\'') {
        let inner = trimmed
            .get(1..trimmed.len().saturating_sub(1))
            .unwrap_or("");
        return js_trim(inner).to_owned();
    }
    trimmed.to_owned()
}

fn loose_value(frontmatter: &str, key: &str) -> Option<String> {
    let pattern = Regex::new(&format!(r"(?m)^{key}\s*:\s*(.+)$")).ok()?;
    pattern
        .captures(frontmatter)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_owned())
}

fn loose_name(frontmatter: &str, fallback: &str) -> String {
    let name =
        strip_scalar_quotes(&loose_value(frontmatter, "name").unwrap_or(fallback.to_owned()));
    if name.is_empty() {
        fallback.to_owned()
    } else {
        name
    }
}

fn loose_version(frontmatter: &str) -> Option<String> {
    let version = strip_scalar_quotes(&loose_value(frontmatter, "version").unwrap_or_default());
    (!version.is_empty()).then_some(version)
}

/// `/^---\n([\s\S]*?)\n---(?:\n|$)/`：第一个闭合 `---` 之前的内容。
fn frontmatter(content: &str) -> Option<&str> {
    let body = content.strip_prefix("---\n")?;
    let mut from = 0;
    while let Some(offset) = body[from..].find("\n---") {
        let at = from + offset;
        let after = &body[at + 4..];
        if after.is_empty() || after.starts_with('\n') {
            return Some(&body[..at]);
        }
        from = at + 1;
    }
    None
}

/// `readSkillMetadataFromMarkdown`。
fn skill_meta_from_markdown(content: &str, fallback: &str) -> SkillMeta {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let Some(text) = frontmatter(&normalized) else {
        return SkillMeta {
            name: fallback.to_owned(),
            version: None,
        };
    };
    if let Ok(Value::Object(parsed)) = yaml::parse(text) {
        let name = parsed
            .get("name")
            .and_then(Value::as_str)
            .map(js_trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| loose_name(text, fallback));
        let parsed_version = match parsed.get("version") {
            Some(Value::String(version)) => js_trim(version).to_owned(),
            Some(number @ Value::Number(_)) => js::stringify(number),
            _ => String::new(),
        };
        let version = if parsed_version.is_empty() {
            loose_version(text)
        } else {
            Some(parsed_version)
        };
        return SkillMeta { name, version };
    }
    SkillMeta {
        name: loose_name(text, fallback),
        version: loose_version(text),
    }
}

async fn skill_meta(skill_path: &str) -> SkillMeta {
    let fallback = fsx::basename(&fsx::dirname(skill_path));
    match tokio::fs::read(skill_path).await {
        Ok(bytes) => skill_meta_from_markdown(&String::from_utf8_lossy(&bytes), &fallback),
        Err(_) => SkillMeta {
            name: fallback,
            version: None,
        },
    }
}

/// `collectSkillMarkdownPaths`：有界遍历（与技能扫描同规则），去重后按 localeCompare 排序。
async fn skill_markdown_paths(root: &str) -> Vec<String> {
    let (found, _) = walk::walk(root).await;
    let mut unique: Vec<String> = found
        .into_iter()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    sort_locale(&mut unique);
    unique
}

async fn existing_skill_names(root: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    if !dir_exists(root).await {
        return names;
    }
    for path in skill_markdown_paths(root).await {
        names.insert(name_key(&skill_meta(&path).await.name));
    }
    names
}

// ── commands ──

/// `collectCommandMarkdownPaths`：栈式遍历 `*.md`，跳过点名，不跟随软链。
async fn command_markdown_paths(root: &str) -> Vec<String> {
    let mut found = HashSet::new();
    let mut stack = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fsx::read_dir(&dir).await else {
            continue;
        };
        for entry in entries {
            if entry.name.starts_with('.') {
                continue;
            }
            let path = fsx::join(&dir, &[&entry.name]);
            if entry.is_dir {
                stack.push(path);
            } else if entry.is_file && entry.name.to_lowercase().ends_with(".md") {
                found.insert(path);
            }
        }
    }
    let mut paths: Vec<String> = found.into_iter().collect();
    sort_locale(&mut paths);
    paths
}

/// `getCommandNameFromPath`。
fn command_name(root: &str, path: &str) -> String {
    let relative = fsx::relative(root, path);
    let stem = if relative.len() >= 3
        && relative.is_char_boundary(relative.len() - 3)
        && relative[relative.len() - 3..].eq_ignore_ascii_case(".md")
    {
        &relative[..relative.len() - 3]
    } else {
        relative.as_str()
    };
    let segments: Vec<&str> = stem
        .split(['/', '\\'])
        .filter(|segment| !segment.is_empty())
        .collect();
    format!("/{}", segments.join("/"))
}

async fn existing_command_names(root: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    if !dir_exists(root).await {
        return names;
    }
    for path in command_markdown_paths(root).await {
        names.insert(name_key(&command_name(root, &path)));
    }
    names
}

// ── plugins ──

async fn plugin_manifest(plugin: &str) -> Option<String> {
    for manifest in PLUGIN_MANIFESTS {
        let path = fsx::join(plugin, &manifest);
        if path_exists(&path).await {
            return Some(path);
        }
    }
    None
}

/// `collectPluginPaths`：根下一级的目录或软链（跳过点名），有 manifest 才算。
async fn plugin_paths(root: &str) -> Vec<String> {
    let Ok(entries) = fsx::read_dir(root).await else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries {
        if (!entry.is_dir && !entry.is_symlink) || entry.name.starts_with('.') {
            continue;
        }
        let path = fsx::join(root, &[&entry.name]);
        if plugin_manifest(&path).await.is_some() {
            found.push(path);
        }
    }
    sort_locale(&mut found);
    found
}

struct PluginMeta {
    id: String,
    name: String,
    version: Option<String>,
}

async fn plugin_meta(plugin: &str) -> Option<PluginMeta> {
    let manifest = plugin_manifest(plugin).await?;
    let parsed = js::parse_record(&fsx::read_text_opt(&manifest).await?)?;
    let name = parsed.get("name").and_then(Value::as_str).map(js_trim)?;
    if name.is_empty() {
        return None;
    }
    let version = parsed
        .get("version")
        .and_then(Value::as_str)
        .map(js_trim)
        .filter(|version| !version.is_empty())
        .map(str::to_owned);
    Some(PluginMeta {
        id: format!("{name}@{INLINE_PLUGIN_MARKETPLACE}"),
        name: name.to_owned(),
        version,
    })
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// `collectConfiguredPluginIds`。
async fn configured_plugin_ids(config: &str) -> HashSet<String> {
    let parsed = read_record(config).await;
    let mut ids = HashSet::new();
    for dir in string_array(
        parsed
            .get("plugins")
            .and_then(|plugins| plugins.get("dirs")),
    ) {
        if let Some(meta) = plugin_meta(&fsx::resolve(&dir, &[])).await {
            ids.insert(meta.id.to_lowercase());
        }
    }
    ids
}

// ── MCP ──

fn server_map(value: Option<&Value>) -> Map<String, Value> {
    let mut servers = Map::new();
    if let Some(Value::Object(map)) = value {
        for (name, config) in map {
            if js_trim(name).is_empty() || !config.is_object() {
                continue;
            }
            servers.insert(name.clone(), config.clone());
        }
    }
    servers
}

/// `normalizeOpenCodeMcpConfig`：数组 command 拆成 command + args，local/remote 改成 stdio/http。
fn open_code_config(config: Value) -> Value {
    let Value::Object(object) = config else {
        return config;
    };
    if let Some(Value::Array(command)) = object.get("command") {
        let strings: Vec<String> = command
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        if let Some((first, args)) = strings.split_first()
            && !first.is_empty()
        {
            let kind = object
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let mut out: Map<String, Value> = object
                .into_iter()
                .filter(|(key, _)| key != "command" && key != "type")
                .collect();
            out.insert("command".into(), json!(first));
            out.insert("args".into(), json!(args));
            match kind.as_deref() {
                Some("local") => {
                    out.insert("type".into(), json!("stdio"));
                }
                Some("remote") => {
                    out.insert("type".into(), json!("http"));
                }
                _ => {}
            }
            return Value::Object(out);
        }
    }
    let local_command = (object.get("type").and_then(Value::as_str) == Some("local"))
        .then(|| object.get("command").and_then(Value::as_str))
        .flatten()
        .map(js_trim)
        .filter(|command| !command.is_empty())
        .map(str::to_owned);
    match local_command {
        Some(command) => {
            let mut out = object;
            out.insert("type".into(), json!("stdio"));
            out.insert("command".into(), json!(command));
            Value::Object(out)
        }
        None => Value::Object(object),
    }
}

/// TOML 日期时间按 `JSON.stringify(TomlDate)` 写成字符串。
fn toml_value(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            if map.len() == 1
                && let Some(Value::String(datetime)) = map.get("$__toml_private_datetime")
            {
                return Value::String(datetime.clone());
            }
            js::js_order(Value::Object(
                map.into_iter()
                    .map(|(key, value)| (key, toml_value(value)))
                    .collect(),
            ))
        }
        Value::Array(items) => Value::Array(items.into_iter().map(toml_value).collect()),
        other => other,
    }
}

/// `readMcpServersFromSourceFile`：任何解析失败都视为没有条目。
async fn source_servers(path: &str, format: McpFormat) -> Map<String, Value> {
    let Some(raw) = fsx::read_text_opt(path).await else {
        return Map::new();
    };
    match format {
        McpFormat::CodexToml => {
            let Ok(parsed) = toml::from_str::<Value>(&raw) else {
                return Map::new();
            };
            let parsed = toml_value(parsed);
            let servers = parsed
                .get("mcp_servers")
                .filter(|value| !value.is_null())
                .or_else(|| parsed.get("mcpServers"));
            server_map(servers)
        }
        McpFormat::ServersJson => {
            let Some(parsed) = js::parse_record(&raw) else {
                return Map::new();
            };
            server_map(parsed.get("mcpServers"))
        }
        McpFormat::OpenCodeJson => {
            let Some(parsed) = js::parse_record(&raw) else {
                return Map::new();
            };
            server_map(parsed.get("mcp"))
                .into_iter()
                .map(|(name, config)| (name, open_code_config(config)))
                .collect()
        }
    }
}

/// `readZcodeMcpServers`：目标配置的 `mcp.servers`。
fn zcode_servers(parsed: &Map<String, Value>) -> Map<String, Value> {
    match parsed.get("mcp") {
        Some(Value::Object(mcp)) => server_map(mcp.get("servers")),
        _ => Map::new(),
    }
}

async fn existing_server_names(paths: &Paths<'_>, scope: Scope) -> HashSet<String> {
    let mut names = HashSet::new();
    if let Some(config) = paths.config_path(scope) {
        for name in zcode_servers(&read_record(&config).await).keys() {
            names.insert(name_key(name));
        }
    }
    names
}

// ── 候选 ──

struct Candidate {
    agent: Agent,
    name: String,
    name_key: String,
    version: Option<String>,
    description: Option<String>,
    argument_hint: Option<String>,
    /// plugin id；MCP 的配置。
    id: String,
    config: Value,
    scope: Scope,
    root: String,
    source: String,
    /// commands 的来源相对路径。
    relative: String,
    target_root: Option<String>,
}

impl Candidate {
    fn new(agent: Agent, scope: Scope, root: &str, source: String) -> Self {
        Self {
            agent,
            name: String::new(),
            name_key: String::new(),
            version: None,
            description: None,
            argument_hint: None,
            id: String::new(),
            config: Value::Null,
            scope,
            root: root.to_owned(),
            source,
            relative: String::new(),
            target_root: None,
        }
    }
}

/// `collectSkillImportCandidates`。
async fn skill_candidates(paths: &Paths<'_>) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for (source, root, scope) in paths.roots(&SKILL_SOURCES) {
        if !dir_exists(&root).await {
            continue;
        }
        for skill in skill_markdown_paths(&root).await {
            if !seen.insert(skill.clone()) {
                continue;
            }
            let Some(target_root) = paths.zcode_root(scope, "skills") else {
                continue;
            };
            let meta = skill_meta(&skill).await;
            let mut candidate = Candidate::new(source.agent, scope, &root, fsx::dirname(&skill));
            candidate.name_key = name_key(&meta.name);
            candidate.name = meta.name;
            candidate.version = meta.version;
            candidate.target_root = Some(target_root);
            candidates.push(candidate);
        }
    }
    candidates
}

/// `collectCommandImportCandidates`。
async fn command_candidates(paths: &Paths<'_>) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for (source, root, scope) in paths.roots(&COMMAND_SOURCES) {
        if !dir_exists(&root).await {
            continue;
        }
        for path in command_markdown_paths(&root).await {
            if !seen.insert(path.clone()) {
                continue;
            }
            let Some(target_root) = paths.zcode_root(scope, "commands") else {
                continue;
            };
            let name = command_name(&root, &path);
            let mut candidate = Candidate::new(source.agent, scope, &root, path.clone());
            if let Ok(bytes) = tokio::fs::read(&path).await {
                let parsed = commands::parse_command_file(&String::from_utf8_lossy(&bytes), &path);
                candidate.description = parsed
                    .get("description")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned);
                candidate.argument_hint = parsed
                    .get("argumentHint")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned);
            }
            candidate.name_key = name_key(&name);
            candidate.name = name;
            candidate.relative = fsx::relative(&root, &path);
            candidate.target_root = Some(target_root);
            candidates.push(candidate);
        }
    }
    candidates
}

/// `collectPluginImportCandidates`。
async fn plugin_candidates(paths: &Paths<'_>) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for (source, root, scope) in paths.roots(&PLUGIN_SOURCES) {
        if !dir_exists(&root).await {
            continue;
        }
        for plugin in plugin_paths(&root).await {
            if !seen.insert(plugin.clone()) {
                continue;
            }
            let Some(target_root) = paths.zcode_root(scope, "plugins") else {
                continue;
            };
            let Some(meta) = plugin_meta(&plugin).await else {
                continue;
            };
            let mut candidate = Candidate::new(source.agent, scope, &root, plugin);
            candidate.id = meta.id;
            candidate.name = meta.name;
            candidate.version = meta.version;
            candidate.target_root = Some(target_root);
            candidates.push(candidate);
        }
    }
    candidates
}

/// `collectMcpImportCandidates`。
async fn mcp_candidates(paths: &Paths<'_>) -> Vec<Candidate> {
    let mut roots: Vec<(Agent, String, Scope, McpFormat)> = Vec::new();
    for source in &MCP_SOURCES {
        for file in source.global {
            roots.push((
                source.agent,
                fsx::join(paths.home, file),
                Scope::Global,
                source.format,
            ));
        }
    }
    if let Some(workspace) = paths.workspace {
        for source in &MCP_SOURCES {
            for file in source.project {
                roots.push((
                    source.agent,
                    fsx::join(workspace, file),
                    Scope::Project,
                    source.format,
                ));
            }
        }
    }
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for (agent, root, scope, format) in roots {
        if !path_exists(&root).await {
            continue;
        }
        for (name, config) in source_servers(&root, format).await {
            let source_path = format!("{root}#{name}");
            if !seen.insert(format!("{agent}:{source_path}")) {
                continue;
            }
            let mut config = config;
            if let Some(object) = config.as_object_mut() {
                object.shift_remove("timeout");
                object.shift_remove("startup_timeout_sec");
            }
            let mut candidate = Candidate::new(agent, scope, &root, source_path);
            candidate.name_key = name_key(&name);
            candidate.name = name;
            candidate.config = config;
            candidates.push(candidate);
        }
    }
    candidates
}

// ── 冲突判定 ──

/// 目标根 / 配置路径 → 已有名称（或 plugin id）；导入成功后追加。
#[derive(Default)]
struct Existing {
    names: HashMap<String, HashSet<String>>,
}

impl Existing {
    fn add(&mut self, key: &str, name: &str) {
        if let Some(names) = self.names.get_mut(key) {
            names.insert(name.to_owned());
        }
    }
}

#[derive(Clone, Copy)]
enum Category {
    Skills,
    Commands,
    Plugins,
    McpServers,
}

impl Category {
    fn id(self) -> &'static str {
        match self {
            Category::Skills => "skills",
            Category::Commands => "commands",
            Category::Plugins => "plugins",
            Category::McpServers => "mcpServers",
        }
    }

    fn items_key(self) -> &'static str {
        match self {
            Category::Skills => "skills",
            Category::Commands => "commands",
            Category::Plugins => "plugins",
            Category::McpServers => "mcpServers",
        }
    }

    fn results_key(self) -> &'static str {
        match self {
            Category::Skills => "skillResults",
            Category::Commands => "commandResults",
            Category::Plugins => "pluginResults",
            Category::McpServers => "mcpServerResults",
        }
    }

    fn agents(self) -> Vec<Agent> {
        match self {
            Category::Skills => SKILL_SOURCES.iter().map(|source| source.agent).collect(),
            Category::Commands => COMMAND_SOURCES.iter().map(|source| source.agent).collect(),
            Category::Plugins => PLUGIN_SOURCES.iter().map(|source| source.agent).collect(),
            Category::McpServers => MCP_SOURCES.iter().map(|source| source.agent).collect(),
        }
    }

    async fn candidates(self, paths: &Paths<'_>) -> Vec<Candidate> {
        match self {
            Category::Skills => skill_candidates(paths).await,
            Category::Commands => command_candidates(paths).await,
            Category::Plugins => plugin_candidates(paths).await,
            Category::McpServers => mcp_candidates(paths).await,
        }
    }
}

/// skills / commands：目标路径已存在 → targetExists；目标根有同名 → sameNameExists。
async fn root_skip_reason(
    category: Category,
    candidate: &Candidate,
    target_root: &str,
    target: &str,
    existing: &mut Existing,
) -> Option<&'static str> {
    let exists = match category {
        // skills 用 readdir 判定（目录），commands 用 lstat。
        Category::Skills => dir_exists(target).await,
        _ => path_exists(target).await,
    };
    if exists {
        return Some("targetExists");
    }
    if !existing.names.contains_key(target_root) {
        let names = match category {
            Category::Skills => existing_skill_names(target_root).await,
            _ => existing_command_names(target_root).await,
        };
        existing.names.insert(target_root.to_owned(), names);
    }
    existing.names[target_root]
        .contains(&candidate.name_key)
        .then_some("sameNameExists")
}

/// plugins：目标目录已存在或没有配置路径 → targetExists；配置里已有同 id → sameNameExists。
async fn plugin_skip_reason(
    candidate: &Candidate,
    target: &str,
    config: Option<&str>,
    existing: &mut Existing,
) -> Option<&'static str> {
    if path_exists(target).await {
        return Some("targetExists");
    }
    let Some(config) = config else {
        return Some("targetExists");
    };
    if !existing.names.contains_key(config) {
        existing
            .names
            .insert(config.to_owned(), configured_plugin_ids(config).await);
    }
    existing.names[config]
        .contains(&candidate.id.to_lowercase())
        .then_some("sameNameExists")
}

/// MCP：目标配置里已有同名 → sameNameExists。
async fn mcp_skip_reason(
    candidate: &Candidate,
    paths: &Paths<'_>,
    scope: Scope,
    existing: &mut Existing,
) -> Option<&'static str> {
    let key = scope.as_str();
    if !existing.names.contains_key(key) {
        existing
            .names
            .insert(key.to_owned(), existing_server_names(paths, scope).await);
    }
    existing.names[key]
        .contains(&candidate.name_key)
        .then_some("sameNameExists")
}

/// detect 时按候选自身的目标判定。
async fn detect_skip_reason(
    category: Category,
    candidate: &Candidate,
    paths: &Paths<'_>,
    existing: &mut Existing,
) -> Option<&'static str> {
    match category {
        Category::Skills | Category::Commands => {
            let root = candidate.target_root.as_deref().unwrap_or_default();
            let target = target_path(category, candidate, root);
            root_skip_reason(category, candidate, root, &target, existing).await
        }
        Category::Plugins => {
            let root = candidate.target_root.as_deref().unwrap_or_default();
            let target = target_path(category, candidate, root);
            let config = paths.config_path(candidate.scope);
            plugin_skip_reason(candidate, &target, config.as_deref(), existing).await
        }
        Category::McpServers => mcp_skip_reason(candidate, paths, candidate.scope, existing).await,
    }
}

fn target_path(category: Category, candidate: &Candidate, root: &str) -> String {
    match category {
        Category::Commands => fsx::join(root, &[&candidate.relative]),
        _ => fsx::join(root, &[&fsx::basename(&candidate.source)]),
    }
}

// ── detect ──

fn summary_item(category: Category, candidate: &Candidate, skip: Option<&str>) -> Value {
    let mut item = Map::new();
    item.insert("name".into(), json!(candidate.name));
    item.insert("path".into(), json!(candidate.source));
    item.insert("importable".into(), json!(skip.is_none()));
    if let Some(skip) = skip {
        item.insert("skipReason".into(), json!(skip));
    }
    match category {
        Category::Skills | Category::Plugins => {
            if let Some(version) = &candidate.version {
                item.insert("version".into(), json!(version));
            }
        }
        Category::Commands => {
            if let Some(description) = &candidate.description {
                item.insert("description".into(), json!(description));
            }
            if let Some(hint) = &candidate.argument_hint {
                item.insert("argumentHint".into(), json!(hint));
            }
        }
        Category::McpServers => {}
    }
    Value::Object(item)
}

/// `build*SourceRootSummaries`：按 `${scope}:${root}` 分组，组内按名称、组间按 scope 再按路径排序。
async fn source_root_summaries(
    category: Category,
    candidates: &[&Candidate],
    paths: &Paths<'_>,
) -> Vec<Value> {
    let mut existing = Existing::default();
    let mut groups: Vec<(String, Vec<&Candidate>)> = Vec::new();
    for candidate in candidates {
        let key = format!("{}:{}", candidate.scope.as_str(), candidate.root);
        match groups.iter_mut().find(|(group, _)| *group == key) {
            Some((_, members)) => members.push(candidate),
            None => groups.push((key, vec![candidate])),
        }
    }
    let mut summaries: Vec<(Scope, String, Value)> = Vec::new();
    for (_, members) in groups {
        let first = members[0];
        let mut importable = 0;
        let mut items: Vec<(String, Value)> = Vec::new();
        for candidate in &members {
            let skip = detect_skip_reason(category, candidate, paths, &mut existing).await;
            if skip.is_none() {
                importable += 1;
            }
            items.push((
                candidate.name.clone(),
                summary_item(category, candidate, skip),
            ));
        }
        items.sort_by(|left, right| locale_compare(&left.0, &right.0));
        let mut summary = Map::new();
        summary.insert("scope".into(), json!(first.scope.as_str()));
        summary.insert("path".into(), json!(first.root));
        summary.insert("discoveredCount".into(), json!(members.len()));
        summary.insert("importableCount".into(), json!(importable));
        summary.insert("skippedCount".into(), json!(members.len() - importable));
        summary.insert(
            category.items_key().into(),
            Value::Array(items.into_iter().map(|(_, item)| item).collect()),
        );
        summaries.push((first.scope, first.root.clone(), Value::Object(summary)));
    }
    summaries.sort_by(|left, right| {
        locale_compare(left.0.as_str(), right.0.as_str())
            .then_with(|| locale_compare(&left.1, &right.1))
    });
    summaries
        .into_iter()
        .map(|(_, _, summary)| summary)
        .collect()
}

/// `build*Discovery`。
async fn discovery(category: Category, paths: &Paths<'_>) -> Value {
    let candidates = category.candidates(paths).await;
    let mut agents = Vec::new();
    for agent in category.agents() {
        let own: Vec<&Candidate> = candidates
            .iter()
            .filter(|candidate| candidate.agent == agent)
            .collect();
        if own.is_empty() {
            continue;
        }
        let mut source_paths: Vec<String> = own
            .iter()
            .map(|candidate| candidate.root.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        sort_locale(&mut source_paths);
        let source_roots = source_root_summaries(category, &own, paths).await;
        let mut existing = Existing::default();
        let mut importable = 0;
        for candidate in &own {
            if detect_skip_reason(category, candidate, paths, &mut existing)
                .await
                .is_none()
            {
                importable += 1;
            }
        }
        agents.push(json!({
            "agent": agent,
            "discovered": true,
            "categories": [{
                "category": category.id(),
                "discoveredCount": own.len(),
                "importableCount": importable,
                "skippedCount": own.len() - importable,
                "sourcePaths": source_paths,
                "sourceRoots": source_roots,
                "selectedByDefault": matches!(category, Category::Skills) && importable > 0,
            }],
        }));
    }
    json!({"agents": agents})
}

/// `detect`：只有 `intent: "manualImport"` 才扫描，类别按 skills → commands → plugins → mcpServers
/// 取第一个命中；其余（含首启）返回空结果。
pub async fn detect(paths: &Paths<'_>, request: &Value) -> Value {
    let wants = |category: &str| {
        request["intent"] == "manualImport"
            && request["categories"]
                .as_array()
                .is_some_and(|categories| categories.iter().any(|item| item == category))
    };
    for category in [
        Category::Skills,
        Category::Commands,
        Category::Plugins,
        Category::McpServers,
    ] {
        if wants(category.id()) {
            return discovery(category, paths).await;
        }
    }
    json!({"agents": []})
}

// ── importSelected ──

/// 目录软链：绝对路径（Windows 上 Node 用 junction，这里用目录软链）。
async fn symlink_dir(source: &str, target: &str) -> std::io::Result<()> {
    let source = fsx::resolve(source, &[]);
    #[cfg(unix)]
    {
        tokio::fs::symlink(source, target).await
    }
    #[cfg(windows)]
    {
        tokio::fs::symlink_dir(source, target).await
    }
}

/// 文件导入：symlink 模式下非 Windows 建文件软链；Windows 用硬链接，失败回退复制。
async fn import_file(source: &str, target: &str, mode: ImportMode) -> Result<(), String> {
    if mode == ImportMode::Symlink {
        let resolved = fsx::resolve(source, &[]);
        #[cfg(unix)]
        {
            return tokio::fs::symlink(&resolved, target)
                .await
                .map_err(|error| error.to_string());
        }
        #[cfg(windows)]
        {
            match tokio::fs::hard_link(&resolved, target).await {
                Ok(()) => {
                    log::info(format!(
                        "[settings-sync] Windows 硬链接创建成功 sourcePath={source} targetPath={target}"
                    ));
                    return Ok(());
                }
                Err(error) => log::warn(format!(
                    "[settings-sync] Windows 硬链接失败，回退到复制模式 sourcePath={source} targetPath={target}: {error}"
                )),
            }
        }
    }
    fsx::copy_recursive(source, target, false)
        .await
        .map_err(|error| error.message)
}

async fn import_dir(source: &str, target: &str, mode: ImportMode) -> Result<(), String> {
    if mode == ImportMode::Symlink {
        return symlink_dir(source, target)
            .await
            .map_err(|error| error.to_string());
    }
    fsx::copy_recursive(source, target, false)
        .await
        .map_err(|error| error.message)
}

/// `addPluginDirToConfig`：已有（按 resolve 比较）则不写。
async fn add_plugin_dir(config: &str, plugin: &str) -> Result<(), RpcError> {
    let resolved = fsx::resolve(plugin, &[]);
    cli_config::update(config, |raw| {
        let mut parsed = raw
            .as_deref()
            .and_then(js::parse_record)
            .unwrap_or_default();
        let mut plugins = match parsed.get("plugins") {
            Some(Value::Object(plugins)) => plugins.clone(),
            _ => Map::new(),
        };
        let mut dirs = string_array(plugins.get("dirs"));
        if dirs.iter().any(|dir| fsx::resolve(dir, &[]) == resolved) {
            return Ok(cli_config::Update::Skip(()));
        }
        dirs.push(resolved.clone());
        plugins.insert("dirs".into(), json!(dirs));
        parsed.insert("plugins".into(), Value::Object(plugins));
        Ok(cli_config::Update::Write(
            js::pretty_line(&Value::Object(parsed)),
            (),
        ))
    })
    .await
}

/// `addMcpServerToZcodeConfig`：合并进 `mcp.servers`（无效条目随之丢弃，同 Node）。
async fn add_mcp_server(config: &str, name: &str, server: &Value) -> Result<(), RpcError> {
    cli_config::update(config, |raw| {
        let mut parsed = raw
            .as_deref()
            .and_then(js::parse_record)
            .unwrap_or_default();
        let mut mcp = match parsed.get("mcp") {
            Some(Value::Object(mcp)) => mcp.clone(),
            _ => Map::new(),
        };
        let mut servers = zcode_servers(&parsed);
        servers.insert(name.to_owned(), server.clone());
        mcp.insert("servers".into(), Value::Object(servers));
        parsed.insert("mcp".into(), Value::Object(mcp));
        Ok(cli_config::Update::Write(
            js::pretty_line(&Value::Object(parsed)),
            (),
        ))
    })
    .await
}

fn item_result(
    category: Category,
    candidate: &Candidate,
    status: &str,
    skip: Option<&str>,
) -> Value {
    let mut item = Map::new();
    item.insert("name".into(), json!(candidate.name));
    item.insert("path".into(), json!(candidate.source));
    item.insert("sourceScope".into(), json!(candidate.scope.as_str()));
    item.insert("status".into(), json!(status));
    if let Some(skip) = skip {
        item.insert("skipReason".into(), json!(skip));
    }
    if matches!(category, Category::Skills | Category::Plugins)
        && let Some(version) = &candidate.version
    {
        item.insert("version".into(), json!(version));
    }
    Value::Object(item)
}

struct Selection {
    agent: String,
    category: String,
    source_scope: Option<Scope>,
    target_scope: Option<Scope>,
    mode: ImportMode,
    paths: Option<HashSet<String>>,
}

impl Selection {
    fn parse(value: &Value) -> Self {
        let paths_key = match value["category"].as_str() {
            Some("skills") => "skillPaths",
            Some("commands") => "commandPaths",
            Some("plugins") => "pluginPaths",
            _ => "mcpServerPaths",
        };
        Self {
            agent: value["agent"].as_str().unwrap_or_default().to_owned(),
            category: value["category"].as_str().unwrap_or_default().to_owned(),
            source_scope: Scope::parse(&value["sourceScope"]),
            target_scope: Scope::parse(&value["targetScope"]),
            mode: if value["importMode"] == "copy" {
                ImportMode::Copy
            } else {
                ImportMode::Symlink
            },
            paths: value[paths_key].as_array().map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            }),
        }
    }
}

/// `import*ForAgent`：一条 selection 的导入。
async fn import_category(category: Category, selection: &Selection, paths: &Paths<'_>) -> Value {
    let candidates: Vec<Candidate> = category
        .candidates(paths)
        .await
        .into_iter()
        .filter(|candidate| {
            candidate.agent == selection.agent
                && selection
                    .source_scope
                    .is_none_or(|scope| scope == candidate.scope)
                && selection
                    .paths
                    .as_ref()
                    .is_none_or(|paths| paths.contains(&candidate.source))
        })
        .collect();
    let (mut imported, mut skipped, mut failed) = (0usize, 0usize, 0usize);
    let mut results = Vec::new();
    let mut existing = Existing::default();
    for candidate in &candidates {
        let outcome: Result<Option<&'static str>, ()> = match category {
            Category::Skills | Category::Commands => {
                let leaf = if matches!(category, Category::Skills) {
                    "skills"
                } else {
                    "commands"
                };
                let root = match selection.target_scope {
                    Some(scope) => paths.zcode_root(scope, leaf),
                    None => candidate.target_root.clone(),
                };
                match root {
                    None => Ok(Some("targetExists")),
                    Some(root) => {
                        let target = target_path(category, candidate, &root);
                        match root_skip_reason(category, candidate, &root, &target, &mut existing)
                            .await
                        {
                            Some(skip) => Ok(Some(skip)),
                            None => {
                                let done = async {
                                    fsx::mkdirp(&fsx::dirname(&target))
                                        .await
                                        .map_err(|error| error.message)?;
                                    if matches!(category, Category::Skills) {
                                        import_dir(&candidate.source, &target, selection.mode).await
                                    } else {
                                        import_file(&candidate.source, &target, selection.mode)
                                            .await
                                    }
                                }
                                .await;
                                match done {
                                    Ok(()) => {
                                        existing.add(&root, &candidate.name_key);
                                        Ok(None)
                                    }
                                    Err(error) => {
                                        log::warn(format!(
                                            "[settings-sync] import {} failed path={}: {error}",
                                            category.id(),
                                            candidate.source
                                        ));
                                        Err(())
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Category::Plugins => {
                let scope = selection.target_scope.unwrap_or(candidate.scope);
                match (paths.zcode_root(scope, "plugins"), paths.config_path(scope)) {
                    (Some(root), Some(config)) => {
                        let target = target_path(category, candidate, &root);
                        match plugin_skip_reason(candidate, &target, Some(&config), &mut existing)
                            .await
                        {
                            Some(skip) => Ok(Some(skip)),
                            None => {
                                let done = async {
                                    fsx::mkdirp(&fsx::dirname(&target))
                                        .await
                                        .map_err(|error| error.message)?;
                                    import_dir(&candidate.source, &target, selection.mode).await?;
                                    add_plugin_dir(&config, &target)
                                        .await
                                        .map_err(|error| error.message)
                                }
                                .await;
                                match done {
                                    Ok(()) => {
                                        existing.add(&config, &candidate.id.to_lowercase());
                                        Ok(None)
                                    }
                                    Err(error) => {
                                        log::warn(format!(
                                            "[settings-sync] import plugins failed path={}: {error}",
                                            candidate.source
                                        ));
                                        Err(())
                                    }
                                }
                            }
                        }
                    }
                    _ => Ok(Some("targetExists")),
                }
            }
            Category::McpServers => {
                let scope = selection.target_scope.unwrap_or(candidate.scope);
                match paths.config_path(scope) {
                    None => Ok(Some("sameNameExists")),
                    Some(config) => {
                        match mcp_skip_reason(candidate, paths, scope, &mut existing).await {
                            Some(skip) => Ok(Some(skip)),
                            None => {
                                match add_mcp_server(&config, &candidate.name, &candidate.config)
                                    .await
                                {
                                    Ok(()) => {
                                        existing.add(scope.as_str(), &candidate.name_key);
                                        Ok(None)
                                    }
                                    Err(error) => {
                                        log::warn(format!(
                                            "[settings-sync] import mcpServers failed path={}: {}",
                                            candidate.source, error.message
                                        ));
                                        Err(())
                                    }
                                }
                            }
                        }
                    }
                }
            }
        };
        match outcome {
            Ok(None) => {
                imported += 1;
                results.push(item_result(category, candidate, "imported", None));
            }
            Ok(Some(skip)) => {
                skipped += 1;
                results.push(item_result(category, candidate, "skipped", Some(skip)));
            }
            Err(()) => {
                failed += 1;
                results.push(item_result(category, candidate, "failed", None));
            }
        }
    }
    let status = if failed > 0 {
        "failed"
    } else if imported > 0 {
        "success"
    } else {
        "skipped"
    };
    let mut result = Map::new();
    result.insert("agent".into(), json!(selection.agent));
    result.insert("category".into(), json!(category.id()));
    result.insert("status".into(), json!(status));
    result.insert("importedCount".into(), json!(imported));
    result.insert("skippedCount".into(), json!(skipped));
    result.insert("failedCount".into(), json!(failed));
    result.insert(category.results_key().into(), Value::Array(results));
    Value::Object(result)
}

/// `importSelected`：逐条 selection 顺序执行并汇总。
pub async fn import_selected(paths: &Paths<'_>, selections: &[Value]) -> Value {
    let mut task_results = Vec::new();
    for raw in selections {
        let selection = Selection::parse(raw);
        let category = match selection.category.as_str() {
            "skills" => Some(Category::Skills),
            "commands" => Some(Category::Commands),
            "plugins" => Some(Category::Plugins),
            "mcpServers" => Some(Category::McpServers),
            _ => None,
        };
        let result = match category {
            Some(category) => import_category(category, &selection, paths).await,
            None => {
                // 未知类别（例如 providers）：记一条 skipped；缺省字段与 JSON.stringify 一样省略。
                let mut skipped = Map::new();
                for key in ["agent", "category"] {
                    if let Some(value) = raw.get(key) {
                        skipped.insert(key.into(), value.clone());
                    }
                }
                skipped.insert("status".into(), json!("skipped"));
                skipped.insert("importedCount".into(), json!(0));
                skipped.insert("skippedCount".into(), json!(1));
                skipped.insert("failedCount".into(), json!(0));
                Value::Object(skipped)
            }
        };
        task_results.push(result);
    }
    let sum = |key: &str| -> u64 {
        task_results
            .iter()
            .map(|result| result[key].as_u64().unwrap_or(0))
            .sum()
    };
    json!({
        "successCount": sum("importedCount"),
        "skippedCount": sum("skippedCount"),
        "failedCount": sum("failedCount"),
        "taskResults": task_results,
    })
}
