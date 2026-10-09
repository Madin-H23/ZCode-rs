//! `skills` channel（packages/services/src/skills/skillsService.ts）。
//!
//! 每次调用重新扫描：workspace 向上到 worktree 根每层的 `.zcode/skills`、`.agents/skills`，用户级
//! 两个根，启用插件的技能目录；启用覆盖只存在 `config.json` 的 `skills.<SKILL.md 路径>.enable`。
use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;
use zcode_host_kit::{Reply, RpcError, ServiceBuilder};
use zcode_kit::js::string::trim as js_trim;

use super::walk::{self, SKILL_FILE_NAME};
use super::{Roots, bind, fsx, json as js, parse_params, plugin_roots, yaml};
use super::{read_cli_config_lenient, update_cli_config_lenient};
use crate::support::collate::locale_compare;

pub const CHANNEL: &str = "skills";
const MAX_DESCRIPTION_LENGTH: usize = 1024;

// ── frontmatter（readFrontmatter） ──

#[derive(Debug, Clone, PartialEq)]
pub struct Frontmatter {
    pub has_frontmatter: bool,
    pub name: String,
    pub description: String,
    pub body: String,
}

/// JS `String(value)`。
pub fn js_to_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(_) => js::stringify(value),
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                other => js_to_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_owned(),
    }
}

/// `readFrontmatterString`：null / undefined 为空串，其余 `String(value)`。
fn frontmatter_string(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(other) => js_to_string(other),
    }
}

/// `/^---\n([\s\S]*?)\n---(?:\n|$)/`：返回（frontmatter 文本，整段匹配长度）。
fn match_frontmatter(text: &str) -> Option<(&str, usize)> {
    if !text.starts_with("---\n") {
        return None;
    }
    let mut from = 4;
    while let Some(offset) = text[from..].find("\n---") {
        let at = from + offset;
        let after = at + 4;
        if after == text.len() {
            return Some((&text[4..at], after));
        }
        if text.as_bytes()[after] == b'\n' {
            return Some((&text[4..at], after + 1));
        }
        from = at + 1;
    }
    None
}

fn starts_with_heading(line: &str) -> bool {
    let hashes = line.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&hashes) {
        return false;
    }
    let rest = &line[hashes..];
    let spaces = rest.len() - rest.trim_start().len();
    spaces > 0
        && rest
            .trim_start()
            .chars()
            .next()
            .is_some_and(|c| !c.is_whitespace())
}

fn loose_inline(value: &str) -> String {
    let trimmed = js_trim(value);
    let trimmed = trimmed.strip_prefix(['"', '\'']).unwrap_or(trimmed);
    trimmed
        .strip_suffix(['"', '\''])
        .unwrap_or(trimmed)
        .to_owned()
}

fn loose_fields(text: &str) -> (String, String) {
    let mut name = String::new();
    let mut description = String::new();
    let lines: Vec<&str> = text.split('\n').collect();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if let Some(colon) = line.find(':') {
            let key = js_trim(&line[..colon]);
            let raw = js_trim(&line[colon + 1..]);
            if key == "name" {
                name = loose_inline(raw);
            } else if key == "description" {
                match raw.chars().next() {
                    Some(style @ ('|' | '>')) => {
                        let mut block: Vec<String> = Vec::new();
                        let mut next = index + 1;
                        while next < lines.len()
                            && (lines[next].starts_with(' ') || lines[next].starts_with('\t'))
                        {
                            block.push(strip_up_to_two_ws(lines[next]).to_owned());
                            index = next;
                            next += 1;
                        }
                        description = if style == '>' {
                            block.join(" ")
                        } else {
                            block.join("\n")
                        };
                    }
                    _ => description = loose_inline(raw),
                }
            }
        }
        index += 1;
    }
    (name, description)
}

/// `.replace(/^\s{1,2}/, "")`。
fn strip_up_to_two_ws(line: &str) -> &str {
    let mut rest = line;
    for _ in 0..2 {
        match rest.chars().next() {
            Some(c) if c.is_whitespace() => rest = &rest[c.len_utf8()..],
            _ => break,
        }
    }
    rest
}

fn yaml_unsafe_loose_field(text: &str) -> bool {
    text.split('\n').any(|line| {
        let Some(colon) = line.find(':') else {
            return false;
        };
        let key = js_trim(&line[..colon]);
        if key != "name" && key != "description" {
            return false;
        }
        let raw = js_trim(&line[colon + 1..]);
        !(raw.is_empty() || raw.starts_with(['"', '\'', '|', '>'])) && raw.contains(": ")
    })
}

pub fn read_frontmatter(content: &str) -> Frontmatter {
    // SKILL.md 可能来自 Windows 或其他工具链：先统一换行，再交给 YAML 解析。
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let Some((frontmatter, matched)) = match_frontmatter(&normalized) else {
        return Frontmatter {
            has_frontmatter: false,
            name: String::new(),
            description: String::new(),
            body: js_trim(&normalized).to_owned(),
        };
    };
    // 正文标题写在 closing --- 之前的历史 skill：从第一个 Markdown 标题起挪回正文。
    let lines: Vec<&str> = frontmatter.split('\n').collect();
    let leaked_at = lines
        .iter()
        .enumerate()
        .position(|(index, line)| index > 0 && starts_with_heading(line));
    let (metadata, leaked) = match leaked_at {
        Some(at) => (
            lines[..at].join("\n").trim_end().to_owned(),
            js_trim(&lines[at..].join("\n")).to_owned(),
        ),
        None => (frontmatter.to_owned(), String::new()),
    };
    let after = js_trim(&normalized[matched..]).to_owned();
    let body = js_trim(
        &[leaked, after]
            .into_iter()
            .filter(|part| !js_trim(part).is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
    )
    .to_owned();
    let (loose_name, loose_description) = loose_fields(&metadata);
    let loose = || Frontmatter {
        has_frontmatter: true,
        name: loose_name.clone(),
        description: loose_description.clone(),
        body: body.clone(),
    };
    if yaml_unsafe_loose_field(&metadata) {
        return loose();
    }
    match yaml::parse(&metadata) {
        Ok(Value::Object(parsed)) => {
            let name = frontmatter_string(parsed.get("name"));
            let description = frontmatter_string(parsed.get("description"));
            Frontmatter {
                has_frontmatter: true,
                name: if name.is_empty() { loose_name } else { name },
                description: if description.is_empty() {
                    loose_description
                } else {
                    description
                },
                body,
            }
        }
        _ => loose(),
    }
}

// ── 发现 ──

#[derive(Debug, Clone)]
struct RootDescriptor {
    scope: &'static str,
    root_path: String,
    plugin_name: Option<String>,
    plugin_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub scope: &'static str,
    pub path: String,
    pub source_path: String,
    pub body: String,
    pub value: Map<String, Value>,
}

fn normalize_name_key(name: &str) -> String {
    js_trim(name).to_lowercase()
}

fn normalize_scan_root(path: &str) -> String {
    path.replace('\\', "/").trim_end_matches('/').to_owned()
}

fn diagnostic(code: &str, severity: &str, message: String, path: &str) -> Value {
    json!({ "code": code, "severity": severity, "message": message, "path": path })
}

async fn read_name_key(skill_path: &str) -> String {
    let fallback = fsx::basename(&fsx::dirname(skill_path));
    match fsx::read_text(skill_path).await {
        Ok(content) => {
            let parsed = read_frontmatter(&content);
            let raw = if parsed.has_frontmatter {
                js_trim(&parsed.name).to_owned()
            } else {
                fallback.clone()
            };
            normalize_name_key(if raw.is_empty() { &fallback } else { &raw })
        }
        Err(_) => normalize_name_key(&fallback),
    }
}

/// `collectSkillMarkdownPaths`：去重后按 localeCompare 排序；扫描错误记为 warning 诊断。
async fn collect_skill_paths(root: &str, diagnostics: &mut Vec<Value>) -> Vec<String> {
    let (found, errors) = walk::walk(root).await;
    for error in errors {
        diagnostics.push(diagnostic(
            "skill_scan_failed",
            "warning",
            error.message,
            &error.path,
        ));
    }
    let mut unique: Vec<String> = Vec::new();
    for path in found {
        if !unique.contains(&path) {
            unique.push(path);
        }
    }
    unique.sort_by(|left, right| locale_compare(left, right));
    unique
}

async fn read_metadata(skill_path: &str) -> Option<Value> {
    let path = fsx::join(&fsx::dirname(skill_path), &["_meta.json"]);
    let raw = fsx::read_text_opt(&path).await?;
    if raw.is_empty() {
        return None;
    }
    let parsed = js::parse_record(&raw)?;
    let mut metadata = Map::new();
    for key in ["slug", "version", "ownerId"] {
        if let Some(text) = parsed.get(key).and_then(Value::as_str) {
            let trimmed = js_trim(text);
            if !trimmed.is_empty() {
                metadata.insert(key.into(), json!(trimmed));
            }
        }
    }
    if let Some(number) = parsed.get("publishedAt").filter(|value| value.is_number()) {
        metadata.insert("publishedAt".into(), number.clone());
    }
    (!metadata.is_empty()).then_some(Value::Object(metadata))
}

#[derive(Clone)]
pub struct SkillsService {
    roots: Roots,
    write_queue: std::sync::Arc<Mutex<()>>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct WorkspaceParams {
    #[serde(default)]
    workspace_path: String,
    #[serde(default)]
    workspace_identity: Option<String>,
    #[serde(default)]
    skill_id: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    prompt: Option<String>,
}

impl SkillsService {
    pub fn new(roots: Roots) -> Self {
        Self {
            roots,
            write_queue: Default::default(),
        }
    }

    pub fn user_zcode_root(&self) -> String {
        fsx::join(&self.roots.home, &[".zcode", "skills"])
    }

    pub fn user_agents_root(&self) -> String {
        fsx::join(&self.roots.home, &[".agents", "skills"])
    }

    fn workspace_common_root(workspace: &str) -> String {
        fsx::join(workspace, &[".zcode", "skills"])
    }

    /// 从 workspace 向上到 worktree 根，每层的 `.zcode/skills` 与 `.agents/skills`。
    pub async fn ancestor_roots(workspace: &str) -> Vec<String> {
        let mut worktree = None;
        let mut current = workspace.to_owned();
        loop {
            if fsx::exists(&fsx::join(&current, &[".git"])).await {
                worktree = Some(current.clone());
                break;
            }
            let parent = fsx::dirname(&current);
            if parent == current {
                break;
            }
            current = parent;
        }
        let mut bases = Vec::new();
        match worktree {
            None => bases.push(workspace.to_owned()),
            Some(root) => {
                let mut current = workspace.to_owned();
                loop {
                    bases.push(current.clone());
                    let parent = fsx::dirname(&current);
                    if current == root || parent == current {
                        break;
                    }
                    current = parent;
                }
            }
        }
        let mut roots = Vec::new();
        for base in bases {
            roots.push(fsx::join(&base, &[".zcode", "skills"]));
            roots.push(fsx::join(&base, &[".agents", "skills"]));
        }
        roots
    }

    /// `dedupeScanRootsByRealpath`（先去掉被已存在的更深根覆盖的祖先根）。
    async fn dedupe_scan_roots(paths: &[String]) -> Vec<String> {
        let mut existing: HashSet<String> = HashSet::new();
        for path in paths {
            if fsx::exists(path).await {
                existing.insert(normalize_scan_root(path));
            }
        }
        let normalized: Vec<String> = paths.iter().map(|path| normalize_scan_root(path)).collect();
        let filtered: Vec<&String> = paths
            .iter()
            .enumerate()
            .filter(|(index, _)| {
                let current = &normalized[*index];
                if !existing.contains(current) {
                    return true;
                }
                !normalized.iter().enumerate().any(|(other_index, other)| {
                    other_index != *index
                        && existing.contains(other)
                        && other.starts_with(&format!("{current}/"))
                })
            })
            .map(|(_, path)| path)
            .collect();
        let mut seen = HashSet::new();
        let mut result = Vec::new();
        for path in filtered {
            let mut canonical = normalize_scan_root(path);
            if fsx::exists(path).await {
                canonical = normalize_scan_root(&fsx::realpath_or_self(path).await);
            }
            if seen.insert(canonical) {
                result.push(path.clone());
            }
        }
        result
    }

    async fn discover(&self, workspace: &str) -> (Vec<Skill>, Vec<Value>) {
        let mut roots: Vec<RootDescriptor> = Vec::new();
        let mut workspace_roots: Vec<String> = Vec::new();
        for root in Self::ancestor_roots(workspace).await {
            if !workspace_roots.contains(&root) {
                workspace_roots.push(root);
            }
        }
        for root_path in workspace_roots {
            roots.push(RootDescriptor {
                scope: "workspace",
                root_path,
                plugin_name: None,
                plugin_id: None,
            });
        }
        // 用户级是全局资源：`.zcode/skills` 有技能时也不能截断 `.agents/skills`。
        for root_path in [self.user_zcode_root(), self.user_agents_root()] {
            roots.push(RootDescriptor {
                scope: "user",
                root_path,
                plugin_name: None,
                plugin_id: None,
            });
        }
        let config = read_cli_config_lenient(&self.roots).await;
        for component in plugin_roots::enabled_component_roots(&self.roots, &config, "skills").await
        {
            roots.push(RootDescriptor {
                scope: "plugin",
                root_path: component.root_path,
                plugin_name: Some(component.plugin_name),
                plugin_id: Some(component.plugin_id),
            });
        }

        let mut diagnostics = Vec::new();
        let mut skills = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let user_zcode = self.user_zcode_root();
        let user_agents = self.user_agents_root();
        let mut zcode_name_keys: HashSet<String> = HashSet::new();
        if fsx::exists(&user_zcode).await {
            let mut ignored = Vec::new();
            for path in collect_skill_paths(&user_zcode, &mut ignored).await {
                zcode_name_keys.insert(read_name_key(&path).await);
            }
        }
        let scan_paths = Self::dedupe_scan_roots(
            &roots
                .iter()
                .map(|root| root.root_path.clone())
                .collect::<Vec<_>>(),
        )
        .await;
        // `new Map(roots.map(...))`：同一路径后出现的描述覆盖先出现的。
        let mut by_path: HashMap<String, RootDescriptor> = HashMap::new();
        for root in &roots {
            by_path.insert(root.root_path.clone(), root.clone());
        }
        for root_path in scan_paths {
            let Some(root) = by_path.get(&root_path) else {
                continue;
            };
            if !fsx::exists(&root.root_path).await {
                continue;
            }
            for skill_path in collect_skill_paths(&root.root_path, &mut diagnostics).await {
                if root.root_path == user_agents {
                    let folder = fsx::basename(&fsx::dirname(&skill_path));
                    if fsx::exists(&fsx::join(&user_zcode, &[&folder, SKILL_FILE_NAME])).await
                        || zcode_name_keys.contains(&read_name_key(&skill_path).await)
                    {
                        continue;
                    }
                }
                let canonical = fsx::realpath_or_self(&skill_path).await;
                if !seen.insert(canonical.clone()) {
                    continue;
                }
                if !fsx::exists(&skill_path).await {
                    continue;
                }
                let markdown = match fsx::read_text(&skill_path).await {
                    Ok(markdown) => markdown,
                    Err(error) => {
                        diagnostics.push(diagnostic(
                            "skill_read_failed",
                            "warning",
                            fsx::fs_message(&error, "open", &skill_path),
                            &skill_path,
                        ));
                        continue;
                    }
                };
                let parsed = read_frontmatter(&markdown);
                let folder = fsx::basename(&fsx::dirname(&skill_path));
                let raw_name = if parsed.has_frontmatter {
                    js_trim(&parsed.name).to_owned()
                } else {
                    folder.clone()
                };
                let name = if raw_name.is_empty() {
                    folder
                } else {
                    raw_name
                };
                if name.is_empty() {
                    diagnostics.push(diagnostic(
                        "skill_missing_name",
                        "error",
                        format!("Skill frontmatter must include a name: {skill_path}"),
                        &skill_path,
                    ));
                    continue;
                }
                let description = if parsed.has_frontmatter {
                    js_trim(&parsed.description).to_owned()
                } else {
                    String::new()
                };
                if description.encode_utf16().count() > MAX_DESCRIPTION_LENGTH {
                    let mut entry = diagnostic(
                        "skill_description_too_long",
                        "error",
                        format!(
                            "Skill description is too long (>{MAX_DESCRIPTION_LENGTH}): {name}"
                        ),
                        &skill_path,
                    );
                    entry["skillName"] = json!(name);
                    diagnostics.push(entry);
                    continue;
                }
                let body = js_trim(&parsed.body).to_owned();
                let id = format!(
                    "glm:{}:{name}:{}",
                    root.scope,
                    &fsx::sha256_hex(canonical.as_bytes())[..12]
                );
                let mut value = Map::new();
                value.insert("id".into(), json!(id));
                value.insert("name".into(), json!(name));
                value.insert("description".into(), json!(description));
                value.insert("body".into(), json!(body));
                value.insert("path".into(), json!(canonical));
                value.insert("sourcePath".into(), json!(skill_path));
                value.insert("scope".into(), json!(root.scope));
                value.insert("enabled".into(), json!(true));
                if let Some(plugin_name) = &root.plugin_name {
                    value.insert("pluginName".into(), json!(plugin_name));
                }
                if let Some(plugin_id) = &root.plugin_id {
                    value.insert("pluginId".into(), json!(plugin_id));
                }
                if let Some(metadata) = read_metadata(&skill_path).await {
                    value.insert("metadata".into(), metadata);
                }
                skills.push(Skill {
                    id,
                    name,
                    scope: root.scope,
                    path: canonical,
                    source_path: skill_path,
                    body,
                    value,
                });
            }
        }
        skills.sort_by(|left, right| {
            locale_compare(&left.name, &right.name)
                .then_with(|| locale_compare(left.scope, right.scope))
                .then_with(|| locale_compare(&left.path, &right.path))
        });
        (skills, diagnostics)
    }

    /// 启用状态：`skills.<路径（/ 分隔）>.enable`，缺省启用。
    fn enabled_map(config: &Map<String, Value>) -> HashMap<String, bool> {
        match config.get("skills") {
            Some(Value::Object(entries)) => entries
                .iter()
                .filter_map(|(path, entry)| {
                    entry
                        .as_object()
                        .and_then(|entry| entry.get("enable"))
                        .and_then(Value::as_bool)
                        .map(|enabled| (path.replace('\\', "/"), enabled))
                })
                .collect(),
            _ => HashMap::new(),
        }
    }

    pub async fn list_skills(&self, workspace: &str) -> (Vec<Skill>, Vec<Value>) {
        let (mut skills, diagnostics) = self.discover(workspace).await;
        let enabled = Self::enabled_map(&read_cli_config_lenient(&self.roots).await);
        for skill in &mut skills {
            let flag = enabled
                .get(&skill.path.replace('\\', "/"))
                .copied()
                .unwrap_or(true);
            skill.value.insert("enabled".into(), json!(flag));
        }
        (skills, diagnostics)
    }

    pub async fn list(&self, params: Value) -> Result<Value, RpcError> {
        let params: WorkspaceParams = parse_params(params)?;
        let (skills, diagnostics) = self.list_skills(&params.workspace_path).await;
        Ok(json!({
            "skills": skills.into_iter().map(|skill| Value::Object(skill.value)).collect::<Vec<_>>(),
            "capability": { "userScopeAvailable": true },
            "diagnostics": diagnostics,
        }))
    }

    #[expect(clippy::await_holding_invalid_type, reason = "存量，U2 改为 actor")]
    pub async fn set_enabled(&self, params: Value) -> Result<(), RpcError> {
        let params: WorkspaceParams = parse_params(params)?;
        let skill_id = params.skill_id.unwrap_or_default();
        let enabled = params.enabled.unwrap_or(true);
        let _queue = self.write_queue.lock().await;
        let (skills, _) = self.discover(&params.workspace_path).await;
        let skill = skills
            .into_iter()
            .find(|skill| skill.id == skill_id)
            .ok_or_else(|| fsx::error(format!("Skill not found: {skill_id}")))?;
        let key = skill.path.replace('\\', "/");
        update_cli_config_lenient(&self.roots, move |config| {
            let mut map = Self::enabled_map(config);
            map.insert(key, enabled);
            write_enabled_map(config, map);
            (true, ())
        })
        .await
    }

    pub async fn build_prompt_context(&self, params: Value) -> Result<Value, RpcError> {
        let params: WorkspaceParams = parse_params(params)?;
        let prompt = params.prompt.unwrap_or_default();
        let mentioned = mentioned_skill_names(&prompt);
        let unchanged = || json!({ "prompt": prompt, "activatedSkillNames": [] });
        if mentioned.is_empty() {
            return Ok(unchanged());
        }
        let (skills, _) = self.list_skills(&params.workspace_path).await;
        let activated: Vec<&Skill> = skills
            .iter()
            .filter(|skill| skill.value["enabled"] == true && mentioned.contains(&skill.name))
            .collect();
        if activated.is_empty() {
            return Ok(unchanged());
        }
        let names: Vec<&str> = activated.iter().map(|skill| skill.name.as_str()).collect();
        self.append_audit_log(
            &params.workspace_path,
            params.workspace_identity.as_deref(),
            &names,
        )
        .await?;
        let mut block = vec!["<available_skills>".to_owned()];
        for skill in &activated {
            block.push(format!(
                "<activated_skill name=\"{}\" path=\"{}\">\n{}\n</activated_skill>",
                escape_attribute(&skill.name),
                escape_attribute(&skill.path),
                skill.body
            ));
        }
        block.push("</available_skills>".to_owned());
        Ok(json!({
            "prompt": format!("{prompt}\n\n{}", block.join("\n")),
            "activatedSkillNames": names,
        }))
    }

    async fn append_audit_log(
        &self,
        workspace: &str,
        identity: Option<&str>,
        names: &[&str],
    ) -> Result<(), RpcError> {
        let dir = fsx::join(&self.roots.home, &[".zcode", "v2"]);
        fsx::mkdirp(&dir).await?;
        let path = fsx::join(&dir, &["skills-audit.log"]);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_default();
        let line = format!(
            "{}\n",
            js::stringify(&json!({
                "createdAt": now,
                "workspacePath": workspace,
                "workspaceIdentity": identity,
                "activatedSkillNames": names,
            }))
        );
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|error| fsx::fs_error(&error, "open", &path))?;
        file.write_all(line.as_bytes())
            .await
            .map_err(|error| fsx::fs_error(&error, "write", &path))
    }

    async fn find(&self, params: &WorkspaceParams) -> Result<Skill, RpcError> {
        let skill_id = params.skill_id.clone().unwrap_or_default();
        let (skills, _) = self.list_skills(&params.workspace_path).await;
        skills
            .into_iter()
            .find(|skill| skill.id == skill_id)
            .ok_or_else(|| fsx::error(format!("Skill not found: {skill_id}")))
    }

    pub async fn copy_to_common(&self, params: Value) -> Result<Value, RpcError> {
        let params: WorkspaceParams = parse_params(params)?;
        let skill = self.find(&params).await?;
        let source_dir = fsx::dirname(&skill.path);
        let common_root = if skill.scope == "workspace" {
            Self::workspace_common_root(&params.workspace_path)
        } else {
            self.user_zcode_root()
        };
        let leaf = fsx::basename(&source_dir);
        let target = fsx::join(&common_root, &[&leaf]);
        if fsx::exists(&target).await {
            return Err(fsx::error(format!("通用目录已存在同名技能: {leaf}")));
        }
        fsx::mkdirp(&common_root).await?;
        fsx::copy_recursive(&source_dir, &target, true).await?;
        Ok(json!({ "newPath": fsx::join(&target, &[SKILL_FILE_NAME]) }))
    }

    pub async fn remove_from_common(&self, params: Value) -> Result<(), RpcError> {
        let params: WorkspaceParams = parse_params(params)?;
        let skill = self.find(&params).await?;
        let normalize = |path: &str| path.replace('\\', "/").to_lowercase();
        let path = normalize(&skill.path);
        let in_user = path.contains(&format!("{}/", normalize(&self.user_zcode_root())));
        let in_workspace = path.contains(&format!(
            "{}/",
            normalize(&Self::workspace_common_root(&params.workspace_path))
        ));
        if !in_user && !in_workspace {
            return Err(fsx::error("该技能不在通用目录中"));
        }
        let dir = fsx::dirname(&skill.path);
        fsx::remove_all(&dir)
            .await
            .map_err(|error| fsx::fs_error(&error, "rm", &dir))
    }

    pub async fn delete_skill(&self, params: Value) -> Result<(), RpcError> {
        let params: WorkspaceParams = parse_params(params)?;
        let skill = self.find(&params).await?;
        if skill.scope == "plugin" {
            return Err(fsx::error("插件提供的技能不可单独删除，请卸载对应插件"));
        }
        // 用扫描到的原始路径定位目录项：软链导入的技能 path 是 realpath，sourcePath 才是链接本体。
        let skill_dir = fsx::dirname(&skill.source_path);
        let leaf = fsx::basename(&skill_dir);
        // 只解析父目录：叶子是软链时只删链接；祖先软链展开后越界校验才能拦住穿越。
        let Ok(parent) = fsx::realpath(&fsx::dirname(&skill_dir)).await else {
            return Err(fsx::error(format!("该技能不可删除: {}", skill.path)));
        };
        let mut allowed = Self::ancestor_roots(&params.workspace_path).await;
        allowed.push(self.user_zcode_root());
        allowed.push(self.user_agents_root());
        let mut contained = false;
        for root in allowed {
            let canonical = fsx::realpath_or_self(&root).await;
            let relative = fsx::relative(&canonical, &parent);
            if relative.is_empty() || (!relative.starts_with("..") && !fsx::is_absolute(&relative))
            {
                contained = true;
                break;
            }
        }
        if !contained {
            return Err(fsx::error(format!("该技能不可删除: {}", skill.path)));
        }
        let target = fsx::join(&parent, &[&leaf]);
        fsx::remove_all(&target)
            .await
            .map_err(|error| fsx::fs_error(&error, "rm", &target))
    }
}

/// `writeSkillEnabledMap`：按路径排序写入；启用是默认值（删除键），对象为空时删除 `skills`。
fn write_enabled_map(config: &mut Map<String, Value>, map: HashMap<String, bool>) {
    let mut entries: Vec<(String, bool)> = map.into_iter().collect();
    entries.sort_by(|left, right| locale_compare(&left.0, &right.0));
    let mut skills = js::record(config.get("skills"));
    for (path, enabled) in entries {
        let path = path.replace('\\', "/");
        if enabled {
            js::delete(&mut skills, &path);
        } else {
            skills.insert(path, json!({ "enable": enabled }));
        }
    }
    if skills.is_empty() {
        js::delete(config, "skills");
    } else {
        config.insert("skills".into(), Value::Object(skills));
    }
}

/// `/\$([a-z0-9]+(?:-[a-z0-9]+)*)/g`。
pub fn mentioned_skill_names(prompt: &str) -> HashSet<String> {
    let bytes = prompt.as_bytes();
    let word = |at: usize| {
        bytes[at..]
            .iter()
            .take_while(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            .count()
    };
    let mut names = HashSet::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'$' {
            index += 1;
            continue;
        }
        let start = index + 1;
        let mut end = start + word(start);
        if end == start {
            index += 1;
            continue;
        }
        while end < bytes.len() && bytes[end] == b'-' {
            let next = word(end + 1);
            if next == 0 {
                break;
            }
            end += 1 + next;
        }
        names.insert(prompt[start..end].to_owned());
        index = end;
    }
    names
}

fn escape_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn service(skills: SkillsService) -> ServiceBuilder {
    let builder = ServiceBuilder::new(CHANNEL);
    let builder = bind(builder, "list", &skills, |s, p| async move {
        s.list(p).await.map(Reply::Value)
    });
    let builder = bind(builder, "setEnabled", &skills, |s, p| async move {
        s.set_enabled(p).await.map(|()| Reply::Undefined)
    });
    let builder = bind(builder, "buildPromptContext", &skills, |s, p| async move {
        s.build_prompt_context(p).await.map(Reply::Value)
    });
    let builder = bind(builder, "copyToCommon", &skills, |s, p| async move {
        s.copy_to_common(p).await.map(Reply::Value)
    });
    let builder = bind(builder, "removeFromCommon", &skills, |s, p| async move {
        s.remove_from_common(p).await.map(|()| Reply::Undefined)
    });
    bind(builder, "deleteSkill", &skills, |s, p| async move {
        s.delete_skill(p).await.map(|()| Reply::Undefined)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_matches_typescript_rules() {
        let parsed =
            read_frontmatter("---\r\nname: demo\r\ndescription: Does things\r\n---\r\nBody\r\n");
        assert_eq!(parsed.name, "demo");
        assert_eq!(parsed.description, "Does things");
        assert_eq!(parsed.body, "Body");
        let loose = read_frontmatter("---\nname: 中文\ndescription: 触发场景: 写代码\n---\nx");
        assert_eq!(loose.description, "触发场景: 写代码");
        let block = read_frontmatter("---\nname: a\ndescription: >\n  one\n  two\n---\n");
        // 折叠块保留结尾换行（yaml 的 clip），由调用方 trim。
        assert_eq!(block.description, "one two\n");
        let leaked = read_frontmatter("---\nname: a\n# Title\ntext\n---\nmore");
        assert_eq!(leaked.name, "a");
        assert_eq!(leaked.body, "# Title\ntext\n\nmore");
        let none = read_frontmatter("just text");
        assert!(!none.has_frontmatter);
        assert_eq!(none.body, "just text");
        let numeric = read_frontmatter("---\nname: 123\n---\n");
        assert_eq!(numeric.name, "123");
        assert_eq!(match_frontmatter("---\n---\n"), None);
        assert_eq!(match_frontmatter("---\n\n---"), Some(("", 8)));
    }

    #[test]
    fn mentions_follow_the_regex() {
        let names = mentioned_skill_names("use $pdf-tools and $$x, $a- $Up $b-2-c");
        let mut sorted: Vec<_> = names.into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, ["a", "b-2-c", "pdf-tools", "x"]);
    }

    #[test]
    fn enabled_map_round_trip() {
        let mut config =
            js::parse_record(r#"{"skills":{"/b/SKILL.md":{"enable":false,"x":1}}}"#).unwrap();
        let mut map = SkillsService::enabled_map(&config);
        map.insert("/a/SKILL.md".into(), false);
        write_enabled_map(&mut config, map);
        assert_eq!(
            js::stringify(&Value::Object(config.clone())),
            r#"{"skills":{"/b/SKILL.md":{"enable":false},"/a/SKILL.md":{"enable":false}}}"#
        );
        let mut map = SkillsService::enabled_map(&config);
        map.insert("/a/SKILL.md".into(), true);
        map.insert("/b/SKILL.md".into(), true);
        write_enabled_map(&mut config, map);
        assert_eq!(js::stringify(&Value::Object(config)), "{}");
    }
}
