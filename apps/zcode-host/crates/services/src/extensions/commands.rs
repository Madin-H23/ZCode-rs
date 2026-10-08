//! `commands` channel（packages/services/src/commands/commandsService.ts、commandFileParser.ts）。
//!
//! 用户命令来自 workspace 与用户级的 `.zcode/commands`、`.agents/commands`（`.zcode` 读到命令后
//! 同 scope 的 `.agents` 不再参与），插件命令只读；禁用状态存在 `config.json` 的
//! `command.<文件路径>.enable=false`。
use std::collections::HashMap;
use std::io::ErrorKind;

use futures_util::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::fs;
use zcode_cli_domain::js_string::trim as js_trim;
use zcode_host_kit::{Reply, RpcError, ServiceBuilder};

use super::fsx;
use super::json as js;
use super::plugin_roots::{self, ComponentRoot};
use super::{Roots, bind, read_cli_config_lenient, update_cli_config_lenient};
use crate::support::collate::locale_compare;

pub const CHANNEL: &str = "commands";
const AGENT_SOURCE: &str = "zcodeAgent";

/// 一个目录来源（ZCode 与兼容的 `.agents`）。
#[derive(Clone, Copy)]
struct Descriptor {
    directory_source: &'static str,
    segments: [&'static str; 2],
}

const ZCODE: Descriptor = Descriptor {
    directory_source: "zcode",
    segments: [".zcode", "commands"],
};
const AGENTS: Descriptor = Descriptor {
    directory_source: "agents",
    segments: [".agents", "commands"],
};

// ── 命令文件解析与生成（commandFileParser.ts） ──

struct Parts<'a> {
    frontmatter: Vec<&'a str>,
    content: Vec<&'a str>,
}

fn split_markdown(content: &str) -> Parts<'_> {
    let lines: Vec<&str> = content.split('\n').collect();
    let start = lines.iter().position(|line| js_trim(line) == "---");
    let end = start.and_then(|start| {
        lines
            .iter()
            .enumerate()
            .position(|(index, line)| index > start && js_trim(line) == "---")
    });
    match (start, end) {
        (Some(start), Some(end)) => Parts {
            frontmatter: lines[start + 1..end].to_vec(),
            content: lines[end + 1..].to_vec(),
        },
        _ => Parts {
            frontmatter: Vec::new(),
            content: lines,
        },
    }
}

/// 顶层 key（缩进行不是 key），小写。
fn frontmatter_key(line: &str) -> Option<String> {
    if line.starts_with(' ') || line.starts_with('\t') {
        return None;
    }
    let trimmed = js_trim(line);
    let key_len = trimmed
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        .count();
    if key_len == 0 {
        return None;
    }
    let rest = trimmed[key_len..].trim_start_matches(|c: char| c.is_whitespace());
    rest.starts_with(':')
        .then(|| trimmed[..key_len].to_ascii_lowercase())
}

fn multiline_value(lines: &[&str], target: &str) -> Option<String> {
    let mut collected: Vec<String> = Vec::new();
    let mut collecting = false;
    for line in lines {
        if let Some(key) = frontmatter_key(line) {
            if collecting {
                break;
            }
            if key == target {
                collecting = true;
                let value = line.split(':').skip(1).collect::<Vec<_>>().join(":");
                let value = js_trim(&value);
                if !value.is_empty() {
                    collected.push(value.to_owned());
                }
            }
            continue;
        }
        if collecting && (line.starts_with(' ') || line.starts_with('\t')) {
            collected.push(js_trim(line).to_owned());
        }
    }
    let value = js_trim(&collected.join(" ")).to_owned();
    (!value.is_empty()).then_some(value)
}

/// `parseMarkdownCommandFile` 的结果（键序同 TS 对象字面量）。
pub fn parse_command_file(content: &str, file_path: &str) -> Map<String, Value> {
    let parts = split_markdown(content);
    let file_name = file_path
        .split(['/', '\\'])
        .filter(|segment| !segment.is_empty())
        .next_back()
        .unwrap_or("");
    let stem = strip_md_suffix(file_name);
    let joined = parts.content.join("\n");
    let mut parsed = Map::new();
    parsed.insert("name".into(), json!(format!("/{stem}")));
    parsed.insert("prompt".into(), json!(js_trim(&joined)));
    parsed.insert("content".into(), json!(joined));
    parsed.insert("filePath".into(), json!(file_path));
    if let Some(description) = multiline_value(&parts.frontmatter, "description") {
        parsed.insert("description".into(), json!(description));
    }
    if let Some(hint) = multiline_value(&parts.frontmatter, "argument-hint") {
        parsed.insert("argumentHint".into(), json!(hint));
    }
    parsed
}

fn strip_md_suffix(name: &str) -> &str {
    if name.len() >= 3 && name[name.len() - 3..].eq_ignore_ascii_case(".md") {
        &name[..name.len() - 3]
    } else {
        name
    }
}

/// `generateMarkdownCommandFileContent`：保留未被替换的 frontmatter 行。
pub fn generate_command_file(config: &CommandConfig, existing: Option<&str>) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut skip = false;
    for line in split_markdown(existing.unwrap_or("")).frontmatter {
        if let Some(key) = frontmatter_key(line) {
            skip = key == "description" || key == "argument-hint";
        }
        if !skip {
            lines.push(line.to_owned());
        }
    }
    if let Some(description) = config.description.as_deref().map(js_trim)
        && !description.is_empty()
    {
        lines.push(format!("description: {description}"));
    }
    if let Some(hint) = config.argument_hint.as_deref().map(js_trim)
        && !hint.is_empty()
    {
        lines.push(format!("argument-hint: {hint}"));
    }
    if lines.is_empty() {
        return config.prompt.clone();
    }
    format!("---\n{}\n---\n\n{}", lines.join("\n"), config.prompt)
}

// ── 参数 ──

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandConfig {
    pub name: String,
    pub prompt: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub argument_hint: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ListParams {
    #[serde(default)]
    agent_source: Option<String>,
    #[serde(default)]
    workspace_path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WriteParams {
    config: CommandConfig,
    #[serde(default)]
    agent_source: Option<String>,
    #[serde(default)]
    storage_level: Option<String>,
    #[serde(default)]
    workspace_path: Option<String>,
    #[serde(default)]
    old_file_path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileParams {
    file_path: String,
    #[serde(default)]
    enabled: Option<bool>,
}

// ── 服务 ──

#[derive(Clone)]
pub struct CommandsService {
    roots: Roots,
}

fn overrides(config: &Map<String, Value>) -> HashMap<String, bool> {
    match config.get("command") {
        Some(Value::Object(entries)) => entries
            .iter()
            .filter_map(|(path, entry)| {
                entry
                    .get("enable")
                    .and_then(Value::as_bool)
                    .filter(|_| entry.is_object())
                    .map(|enabled| (path.clone(), enabled))
            })
            .collect(),
        _ => HashMap::new(),
    }
}

/// `setCommandEnabledOverride`：启用是默认值（删除键），对象为空时删除 `command`。
fn set_override(config: &mut Map<String, Value>, file_path: &str, enabled: bool) {
    let mut commands = js::record(config.get("command"));
    if enabled {
        js::delete(&mut commands, file_path);
    } else {
        commands.insert(file_path.to_owned(), json!({"enable": false}));
    }
    if commands.is_empty() {
        js::delete(config, "command");
    } else {
        config.insert("command".into(), Value::Object(commands));
    }
}

fn command_name(root: &str, file_path: &str) -> String {
    let relative = fsx::relative(root, file_path);
    let stem = strip_md_suffix(&relative);
    let segments: Vec<&str> = stem
        .split(['/', '\\'])
        .filter(|segment| !segment.is_empty())
        .collect();
    format!("/{}", segments.join("/"))
}

fn location(descriptor: Descriptor, root: &str, project_path: Option<&str>) -> Value {
    let mut location = Map::new();
    location.insert("source".into(), json!(descriptor.directory_source));
    location.insert(
        "scope".into(),
        json!(if project_path.is_some() {
            "project"
        } else {
            "user"
        }),
    );
    location.insert("directoryPath".into(), json!(root));
    if let Some(project) = project_path {
        location.insert("projectPath".into(), json!(project));
    }
    Value::Object(location)
}

/// `UserCommand`（键序同 TS 的展开顺序）。
fn user_command(
    mut parsed: Map<String, Value>,
    descriptor: Descriptor,
    root: &str,
    file_path: &str,
    project_path: Option<&str>,
    enabled: bool,
) -> Value {
    let scope = if project_path.is_some() {
        "project"
    } else {
        "global"
    };
    let name = command_name(root, file_path);
    let location = location(descriptor, root, project_path);
    parsed.insert("name".into(), json!(name));
    parsed.insert("agentSource".into(), json!(AGENT_SOURCE));
    parsed.insert("location".into(), location);
    parsed.insert(
        "id".into(),
        json!(format!(
            "{AGENT_SOURCE}:{}:{scope}:{name}",
            descriptor.directory_source
        )),
    );
    parsed.insert("source".into(), json!("user"));
    parsed.insert("enabled".into(), json!(enabled));
    parsed.insert("scope".into(), json!(scope));
    if let Some(project) = project_path {
        parsed.insert("projectPath".into(), json!(project));
    }
    Value::Object(parsed)
}

fn discover_user<'a>(
    root: &'a str,
    current: &'a str,
    descriptor: Descriptor,
    project_path: Option<&'a str>,
    overrides: &'a HashMap<String, bool>,
    out: &'a mut Vec<Value>,
) -> BoxFuture<'a, ()> {
    Box::pin(async move {
        let Ok(mut reader) = fs::read_dir(current).await else {
            return;
        };
        let mut names = Vec::new();
        while let Ok(Some(entry)) = reader.next_entry().await {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        for name in names {
            if name.starts_with('.') {
                continue;
            }
            let full = fsx::join(current, &[&name]);
            let Ok(meta) = fs::symlink_metadata(&full).await else {
                continue;
            };
            if meta.is_dir() {
                discover_user(root, &full, descriptor, project_path, overrides, out).await;
                continue;
            }
            if !name.to_lowercase().ends_with(".md") {
                continue;
            }
            let Ok(content) = fsx::read_text(&full).await else {
                continue;
            };
            let parsed = parse_command_file(&content, &full);
            let enabled = overrides.get(&full).copied().unwrap_or(true);
            out.push(user_command(
                parsed,
                descriptor,
                root,
                &full,
                project_path,
                enabled,
            ));
        }
    })
}

fn discover_plugin<'a>(
    root: &'a ComponentRoot,
    current: &'a str,
    overrides: &'a HashMap<String, bool>,
    seen: &'a mut Vec<String>,
    out: &'a mut Vec<Value>,
) -> BoxFuture<'a, ()> {
    Box::pin(async move {
        let Ok(mut reader) = fs::read_dir(current).await else {
            return;
        };
        let mut names = Vec::new();
        while let Ok(Some(entry)) = reader.next_entry().await {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        for name in names {
            if name.starts_with('.') {
                continue;
            }
            let full = fsx::join(current, &[&name]);
            let Ok(meta) = fs::symlink_metadata(&full).await else {
                continue;
            };
            if meta.is_dir() {
                discover_plugin(root, &full, overrides, seen, out).await;
                continue;
            }
            if !name.to_lowercase().ends_with(".md") {
                continue;
            }
            let Ok(content) = fsx::read_text(&full).await else {
                continue;
            };
            let mut command = parse_command_file(&content, &full);
            let name = command_name(&root.root_path, &full);
            let key = full.replace('\\', "/").to_lowercase();
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            command.insert(
                "enabled".into(),
                json!(overrides.get(&full).copied().unwrap_or(true)),
            );
            command.insert("filePath".into(), json!(full));
            command.insert(
                "id".into(),
                json!(format!(
                    "plugin:{}:{}:{name}:{full}",
                    root.marketplace, root.plugin_name
                )),
            );
            command.insert("name".into(), json!(name));
            command.insert("pluginEnabled".into(), json!(true));
            command.insert("pluginMarketplace".into(), json!(root.marketplace));
            command.insert("pluginName".into(), json!(root.plugin_name));
            command.insert("scope".into(), json!("global"));
            command.insert("source".into(), json!("plugin"));
            out.push(Value::Object(command));
        }
    })
}

fn name_of(command: &Value) -> &str {
    command["name"].as_str().unwrap_or("")
}

fn check_agent_source(source: Option<&str>) -> Result<(), RpcError> {
    match source {
        None | Some(AGENT_SOURCE) => Ok(()),
        Some(other) => Err(fsx::error(format!(
            "Unsupported command agent source: {other}"
        ))),
    }
}

impl CommandsService {
    pub fn new(roots: Roots) -> Self {
        Self { roots }
    }

    fn user_root(&self) -> String {
        fsx::join(&self.roots.home, &ZCODE.segments)
    }

    /// 合并一组目录来源：`.zcode` 读到命令后同 scope 的 `.agents` 不再参与。
    async fn discover_sources(
        &self,
        base: &str,
        project_path: Option<&str>,
        overrides: &HashMap<String, bool>,
        out: &mut Vec<Value>,
    ) {
        for descriptor in [ZCODE, AGENTS] {
            let root = fsx::join(base, &descriptor.segments);
            if !fsx::exists(&root).await {
                continue;
            }
            let before = out.len();
            discover_user(&root, &root, descriptor, project_path, overrides, out).await;
            if descriptor.directory_source == "zcode" && out.len() > before {
                break;
            }
        }
    }

    pub async fn list(&self, params: Value) -> Result<Value, RpcError> {
        let params: ListParams = serde_json::from_value(params).unwrap_or_default();
        check_agent_source(params.agent_source.as_deref())?;
        let config = read_cli_config_lenient(&self.roots).await;
        let overrides = overrides(&config);
        let mut users = Vec::new();
        if let Some(workspace) = params.workspace_path.as_deref().filter(|path| !path.is_empty()) {
            self.discover_sources(workspace, Some(workspace), &overrides, &mut users)
                .await;
        }
        let home = self.roots.home.clone();
        self.discover_sources(&home, None, &overrides, &mut users)
            .await;
        let mut deduped: Vec<Value> = Vec::new();
        for command in users {
            if !deduped.iter().any(|item| name_of(item) == name_of(&command)) {
                deduped.push(command);
            }
        }
        let mut plugins = Vec::new();
        let mut seen = Vec::new();
        for root in plugin_roots::enabled_component_roots(&self.roots, &config, "commands").await {
            let current = root.root_path.clone();
            discover_plugin(&root, &current, &overrides, &mut seen, &mut plugins).await;
        }
        plugins.sort_by(|left, right| locale_compare(name_of(left), name_of(right)));
        let mut all = deduped.clone();
        all.extend(plugins.iter().cloned());
        Ok(json!({
            "commands": all,
            "userCommands": deduped,
            "pluginCommands": plugins,
            "capability": { "userScopeAvailable": true },
        }))
    }

    fn storage_target(&self, params: &WriteParams) -> Result<(String, Option<String>), RpcError> {
        if params.storage_level.as_deref() == Some("project") {
            let workspace = params
                .workspace_path
                .clone()
                .filter(|path| !path.is_empty())
                .ok_or_else(|| fsx::error("Missing workspace path for project command"))?;
            return Ok((fsx::join(&workspace, &ZCODE.segments), Some(workspace)));
        }
        Ok((self.user_root(), None))
    }

    async fn set_override(&self, file_path: &str, enabled: bool) -> Result<(), RpcError> {
        update_cli_config_lenient(&self.roots, |config| {
            set_override(config, file_path, enabled);
            (true, ())
        })
        .await
    }

    pub async fn write_command_file(&self, params: Value) -> Result<Value, RpcError> {
        let params: WriteParams = parse(params)?;
        check_agent_source(params.agent_source.as_deref())?;
        let (root, project) = self.storage_target(&params)?;
        fsx::mkdirp(&root).await?;
        let file_name = format!("{}.md", strip_leading_slash(&params.config.name));
        let file_path = fsx::join(&root, &[&file_name]);
        match fs::metadata(&file_path).await {
            Ok(_) => {
                return Err(fsx::error(format!(
                    "Command file already exists: {file_name}"
                )));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(fsx::fs_error(&error, "access", &file_path)),
        }
        let content = generate_command_file(&params.config, None);
        fs::write(&file_path, &content)
            .await
            .map_err(|error| fsx::fs_error(&error, "open", &file_path))?;
        self.set_override(&file_path, true).await?;
        let parsed = parse_command_file(&content, &file_path);
        let command = user_command(parsed, ZCODE, &root, &file_path, project.as_deref(), true);
        Ok(json!({ "command": command }))
    }

    pub async fn update_command_file(&self, params: Value) -> Result<Value, RpcError> {
        let params: WriteParams = parse(params)?;
        check_agent_source(params.agent_source.as_deref())?;
        let (root, project) = self.storage_target(&params)?;
        let file_name = format!("{}.md", strip_leading_slash(&params.config.name));
        let new_path = fsx::join(&root, &[&file_name]);
        let before = overrides(&read_cli_config_lenient(&self.roots).await);
        let old_path = params.old_file_path.clone().filter(|path| !path.is_empty());
        let existing = match &old_path {
            Some(old) => fsx::read_text_opt(old).await,
            None => None,
        };
        if let Some(old) = &old_path
            && *old != new_path
        {
            // 旧文件可能已被移除，忽略删除失败（ENOENT 属正常情况）。
            let _ = fs::remove_file(old).await;
        }
        if old_path.as_deref() != Some(new_path.as_str()) {
            match fs::metadata(&new_path).await {
                Ok(_) => {
                    return Err(fsx::error(format!(
                        "Command file already exists: {file_name}"
                    )));
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(fsx::fs_error(&error, "access", &new_path)),
            }
        }
        let content = generate_command_file(&params.config, existing.as_deref());
        fsx::mkdirp(&fsx::dirname(&new_path)).await?;
        fs::write(&new_path, &content)
            .await
            .map_err(|error| fsx::fs_error(&error, "open", &new_path))?;
        if let Some(old) = &old_path
            && *old != new_path
            && let Some(previous) = before.get(old).copied()
        {
            // 禁用状态按文件路径存放；改名会换路径，必须迁移，否则刚禁用的命令改名后重新启用。
            update_cli_config_lenient(&self.roots, |config| {
                set_override(config, old, true);
                set_override(config, &new_path, previous);
                (true, ())
            })
            .await?;
        }
        let enabled = overrides(&read_cli_config_lenient(&self.roots).await)
            .get(&new_path)
            .copied()
            .unwrap_or(true);
        let parsed = parse_command_file(&content, &new_path);
        let command = user_command(parsed, ZCODE, &root, &new_path, project.as_deref(), enabled);
        Ok(json!({ "command": command }))
    }

    pub async fn delete_command_file(&self, params: Value) -> Result<(), RpcError> {
        let params: FileParams = parse(params)?;
        match fs::remove_file(&params.file_path).await {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(fsx::fs_error(&error, "rm", &params.file_path)),
        }
        self.set_override(&params.file_path, true).await
    }

    pub async fn set_command_enabled(&self, params: Value) -> Result<(), RpcError> {
        let params: FileParams = parse(params)?;
        let enabled = params
            .enabled
            .ok_or_else(|| fsx::error("invalid argument params: enabled is required"))?;
        self.set_override(&params.file_path, enabled).await
    }

    pub async fn primary_user_directory(&self) -> Result<Value, RpcError> {
        let path = self.user_root();
        fsx::mkdirp(&path).await?;
        Ok(json!({ "path": path }))
    }
}

fn strip_leading_slash(name: &str) -> &str {
    name.strip_prefix('/').unwrap_or(name)
}

fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, RpcError> {
    serde_json::from_value(value)
        .map_err(|error| zcode_host_kit::args::invalid_argument("params", &error.to_string()))
}

pub fn service(commands: CommandsService) -> ServiceBuilder {
    let builder = ServiceBuilder::new(CHANNEL);
    let builder = bind(builder, "list", &commands, |s, p| async move {
        s.list(p).await.map(Reply::Value)
    });
    let builder = bind(builder, "writeCommandFile", &commands, |s, p| async move {
        s.write_command_file(p).await.map(Reply::Value)
    });
    let builder = bind(builder, "updateCommandFile", &commands, |s, p| async move {
        s.update_command_file(p).await.map(Reply::Value)
    });
    let builder = bind(builder, "deleteCommandFile", &commands, |s, p| async move {
        s.delete_command_file(p).await.map(|()| Reply::Undefined)
    });
    let builder = bind(builder, "setCommandEnabled", &commands, |s, p| async move {
        s.set_command_enabled(p).await.map(|()| Reply::Undefined)
    });
    bind(
        builder,
        "getPrimaryUserCommandsDirectory",
        &commands,
        |s, _| async move { s.primary_user_directory().await.map(Reply::Value) },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_generates_like_typescript() {
        let content = "---\ndescription: Line one\n  continued\nargument-hint: <x>\nother: keep\n---\n\nDo it\n";
        let parsed = parse_command_file(content, "/r/sub/Deploy.MD");
        assert_eq!(parsed["name"], "/Deploy");
        assert_eq!(parsed["description"], "Line one continued");
        assert_eq!(parsed["argumentHint"], "<x>");
        assert_eq!(parsed["prompt"], "Do it");
        assert_eq!(parsed["content"], "\nDo it\n");
        let generated = generate_command_file(
            &CommandConfig {
                name: "/deploy".into(),
                prompt: "New".into(),
                description: Some(" New desc ".into()),
                argument_hint: None,
            },
            Some(content),
        );
        assert_eq!(generated, "---\nother: keep\ndescription: New desc\n---\n\nNew");
        assert_eq!(
            generate_command_file(
                &CommandConfig {
                    name: "x".into(),
                    prompt: "P".into(),
                    description: None,
                    argument_hint: Some("  ".into()),
                },
                None
            ),
            "P"
        );
    }

    #[test]
    fn override_removes_default_entries_in_place() {
        let mut config = js::parse_record(r#"{"a":1,"command":{"/x.md":{"enable":false}},"z":2}"#)
            .unwrap();
        set_override(&mut config, "/y.md", false);
        set_override(&mut config, "/x.md", true);
        assert_eq!(
            js::stringify(&Value::Object(config.clone())),
            r#"{"a":1,"command":{"/y.md":{"enable":false}},"z":2}"#
        );
        set_override(&mut config, "/y.md", true);
        assert_eq!(js::stringify(&Value::Object(config)), r#"{"a":1,"z":2}"#);
    }
}
