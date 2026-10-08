//! `skill-sync` channel（packages/services/src/skill-sync/skillSyncService.ts）：用户级技能导出为
//! tar.gz 供远端导入，或导入远端传来的归档（只新增，不覆盖）。
use std::collections::{HashMap, HashSet};

use futures_util::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};
use zcode_host_kit::{Reply, RpcError, ServiceBuilder};

use super::archive::{self, Entry, Flavor, SKILL_SIZE_LIMIT_CODE};
use super::walk::{self, SKILL_FILE_NAME};
use super::{Roots, bind, bytes_param, bytes_value, check_write_access, fsx, parse_params, yaml};
use crate::support::collate::locale_compare;

pub const CHANNEL: &str = "skill-sync";
pub const DEFAULT_MAX_ARCHIVE_BYTES: u64 = 20 * 1024 * 1024;
const SYNC_PATH_LABEL: &str = "unsafe skill sync path";

#[derive(Debug, Clone)]
struct Candidate {
    id: String,
    name: String,
    directory_name: String,
    description: String,
    path: String,
    size_bytes: u64,
}

impl Candidate {
    fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "directoryName": self.directory_name,
            "description": self.description,
            "path": self.path,
            "sizeBytes": self.size_bytes,
        })
    }
}

/// `/^---\r?\n([\s\S]*?)\r?\n---(?:\r?\n|$)/u` 的 frontmatter 文本。
fn frontmatter_text(content: &str) -> Option<&str> {
    let start = if content.starts_with("---\n") {
        4
    } else if content.starts_with("---\r\n") {
        5
    } else {
        return None;
    };
    let bytes = content.as_bytes();
    let mut position = start;
    while position < bytes.len() {
        let after_newline = match bytes[position] {
            b'\r' if bytes.get(position + 1) == Some(&b'\n') => position + 2,
            b'\n' => position + 1,
            _ => {
                position += 1;
                continue;
            }
        };
        if content[after_newline..].starts_with("---") {
            let rest = &content[after_newline + 3..];
            if rest.is_empty() || rest.starts_with('\n') || rest.starts_with("\r\n") {
                return Some(&content[start..position]);
            }
        }
        position += 1;
    }
    None
}

/// `parseSkillMetadata`：name 缺省为 fallback，description 缺省为空串。
fn skill_metadata(content: &str, fallback: &str) -> (String, String) {
    let Some(text) = frontmatter_text(content) else {
        return (fallback.to_owned(), String::new());
    };
    let Ok(parsed) = yaml::parse(text) else {
        return (fallback.to_owned(), String::new());
    };
    let trim = zcode_cli_domain::js_string::trim;
    let name = parsed
        .get("name")
        .and_then(Value::as_str)
        .map(trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(fallback)
        .to_owned();
    let description = parsed
        .get("description")
        .and_then(Value::as_str)
        .map(|text| trim(text).to_owned())
        .unwrap_or_default();
    (name, description)
}

fn name_key(name: &str) -> String {
    zcode_cli_domain::js_string::trim(name).to_lowercase()
}

/// `readSkillFileOrNull`：只有 ENOENT 视为不存在，其余错误抛出。
async fn read_skill_file(path: &str) -> Result<Option<String>, RpcError> {
    match fsx::read_text(path).await {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(fsx::fs_error(&error, "open", path)),
    }
}

fn recursive_size<'a>(path: &'a str, visited: &'a mut HashSet<String>) -> BoxFuture<'a, u64> {
    Box::pin(async move {
        let Ok(meta) = tokio::fs::symlink_metadata(path).await else {
            return 0;
        };
        if meta.is_symlink() {
            let Ok(target) = tokio::fs::metadata(path).await else {
                return 0;
            };
            if target.is_file() {
                return target.len();
            }
            if target.is_dir() {
                return directory_size(path, visited).await;
            }
            return 0;
        }
        if meta.is_file() {
            return meta.len();
        }
        if !meta.is_dir() {
            return 0;
        }
        directory_size(path, visited).await
    })
}

async fn directory_size(path: &str, visited: &mut HashSet<String>) -> u64 {
    let canonical = fsx::realpath_or_self(path).await;
    if !visited.insert(canonical) {
        return 0;
    }
    let Ok(children) = fsx::read_dir(path).await else {
        return 0;
    };
    let mut total = 0;
    for child in children {
        total += recursive_size(&fsx::join(path, &[&child.name]), visited).await;
    }
    total
}

fn size_error(actual: u64, max: u64, phase: &str) -> RpcError {
    let mut error = fsx::error(format!(
        "skill sync size limit exceeded: {actual}/{max} ({phase})"
    ))
    .with_code(SKILL_SIZE_LIMIT_CODE)
    .with_data(json!({ "actualBytes": actual, "maxBytes": max, "phase": phase }));
    error.name = "SkillSyncSizeLimitError".to_owned();
    error
}

#[derive(Clone)]
pub struct SkillSyncService {
    roots: Roots,
    max_archive_bytes: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusParams {
    directory_names: Vec<String>,
    #[serde(default)]
    skills: Vec<RequestedSkill>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RequestedSkill {
    directory_name: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportParams {
    skill_ids: Vec<String>,
}

impl SkillSyncService {
    pub fn new(roots: Roots) -> Self {
        Self {
            roots,
            max_archive_bytes: DEFAULT_MAX_ARCHIVE_BYTES,
        }
    }

    fn zcode_root(&self) -> String {
        fsx::join(&self.roots.home, &[".zcode", "skills"])
    }

    fn agents_root(&self) -> String {
        fsx::join(&self.roots.home, &[".agents", "skills"])
    }

    async fn candidates_in_root(&self, root: &str) -> Result<Vec<Candidate>, RpcError> {
        if !std::path::Path::new(root).exists() {
            return Ok(Vec::new());
        }
        let (paths, _) = walk::walk(root).await;
        let mut candidates = Vec::new();
        for skill_path in paths {
            let directory = fsx::dirname(&skill_path);
            let relative = fsx::relative(root, &directory).replace('\\', "/");
            if relative.is_empty() {
                continue;
            }
            if let Some(content) = read_skill_file(&skill_path).await? {
                let (name, description) = skill_metadata(&content, &fsx::basename(&relative));
                let mut visited = HashSet::new();
                candidates.push(Candidate {
                    id: fsx::sha256_hex(directory.as_bytes()),
                    name,
                    directory_name: relative,
                    description,
                    path: skill_path,
                    size_bytes: recursive_size(&directory, &mut visited).await,
                });
            }
        }
        candidates
            .sort_by(|left, right| locale_compare(&left.directory_name, &right.directory_name));
        Ok(candidates)
    }

    async fn covered_directory_names(&self, root: &str) -> Result<HashSet<String>, RpcError> {
        let mut names = HashSet::new();
        if !std::path::Path::new(root).exists() {
            return Ok(names);
        }
        let mut stack = vec![(root.to_owned(), String::new(), 0usize)];
        while let Some((directory, relative, depth)) = stack.pop() {
            let skill_file = fsx::join(&directory, &[SKILL_FILE_NAME]);
            if !relative.is_empty()
                && read_skill_file(&skill_file)
                    .await?
                    .is_some_and(|content| !content.is_empty())
            {
                names.insert(relative.clone());
            }
            if depth >= walk::MAX_SCAN_DEPTH {
                continue;
            }
            let entries = match fsx::read_dir(&directory).await {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(fsx::fs_error(&error, "scandir", &directory)),
            };
            for entry in entries {
                if !walk::should_walk(&entry.name) {
                    continue;
                }
                let path = fsx::join(&directory, &[&entry.name]);
                let is_dir = entry.is_dir
                    || (entry.is_symlink
                        && tokio::fs::metadata(&path)
                            .await
                            .is_ok_and(|meta| meta.is_dir()));
                if !is_dir {
                    continue;
                }
                let child = if relative.is_empty() {
                    entry.name.clone()
                } else {
                    format!("{relative}/{}", entry.name)
                };
                stack.push((path, child, depth + 1));
            }
        }
        Ok(names)
    }

    async fn dedupe(candidates: Vec<Candidate>, seen: &mut HashSet<String>) -> Vec<Candidate> {
        let mut result = Vec::new();
        for candidate in candidates {
            if seen.insert(fsx::realpath_or_self(&candidate.path).await) {
                result.push(candidate);
            }
        }
        result
    }

    async fn candidates(&self) -> Result<Vec<Candidate>, RpcError> {
        let mut seen = HashSet::new();
        let zcode_root = self.zcode_root();
        let raw_zcode = self.candidates_in_root(&zcode_root).await?;
        let covered = self.covered_directory_names(&zcode_root).await?;
        let zcode_names: HashSet<String> = raw_zcode.iter().map(|c| name_key(&c.name)).collect();
        let zcode = Self::dedupe(raw_zcode, &mut seen).await;
        let agents_raw: Vec<Candidate> = self
            .candidates_in_root(&self.agents_root())
            .await?
            .into_iter()
            .filter(|c| {
                !covered.contains(&c.directory_name) && !zcode_names.contains(&name_key(&c.name))
            })
            .collect();
        let agents = Self::dedupe(agents_raw, &mut seen).await;
        let mut all: Vec<Candidate> = zcode.into_iter().chain(agents).collect();
        all.sort_by(|left, right| locale_compare(&left.directory_name, &right.directory_name));
        Ok(all)
    }

    async fn directory_by_name(&self, root: &str) -> Result<HashMap<String, String>, RpcError> {
        let mut result = HashMap::new();
        if !std::path::Path::new(root).exists() {
            return Ok(result);
        }
        let (paths, _) = walk::walk(root).await;
        for skill_path in paths {
            let Some(content) = read_skill_file(&skill_path).await? else {
                continue;
            };
            let directory = fsx::dirname(&skill_path);
            let (name, _) = skill_metadata(&content, &fsx::basename(&directory));
            result.entry(name_key(&name)).or_insert(directory);
        }
        Ok(result)
    }

    async fn user_directory_by_name(&self) -> Result<HashMap<String, String>, RpcError> {
        let mut result = self.directory_by_name(&self.zcode_root()).await?;
        for (key, path) in self.directory_by_name(&self.agents_root()).await? {
            result.entry(key).or_insert(path);
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
        let params: StatusParams = parse_params(params)?;
        let root = self.zcode_root();
        let existing = self.user_directory_by_name().await?;
        // directoryName 来自 RPC：必须在服务端限制为 skills 根内的安全相对路径。
        let directory_names = params
            .directory_names
            .iter()
            .map(|name| archive::normalize_relative(name, SYNC_PATH_LABEL))
            .collect::<Result<Vec<_>, _>>()?;
        let mut requested: HashMap<String, String> = HashMap::new();
        for skill in params.skills {
            requested.insert(
                archive::normalize_relative(&skill.directory_name, SYNC_PATH_LABEL)?,
                skill.name,
            );
        }
        let mut statuses = Vec::new();
        for directory_name in directory_names {
            let path = archive::resolve_within(&root, &directory_name, SYNC_PATH_LABEL)?;
            if std::path::Path::new(&path).exists() {
                statuses
                    .push(json!({ "directoryName": directory_name, "exists": true, "path": path }));
                continue;
            }
            let existing_path = requested
                .get(&directory_name)
                .and_then(|name| existing.get(&name_key(name)));
            statuses.push(match existing_path {
                Some(path) => {
                    json!({ "directoryName": directory_name, "exists": true, "path": path })
                }
                None => json!({ "directoryName": directory_name, "exists": false }),
            });
        }
        Ok(json!({ "statuses": statuses }))
    }

    pub async fn export_archive(&self, params: Value) -> Result<Value, RpcError> {
        let params: ExportParams = parse_params(params)?;
        let candidates = self.candidates().await?;
        let mut selected = Vec::new();
        for id in &params.skill_ids {
            let candidate = candidates
                .iter()
                .find(|candidate| &candidate.id == id)
                .ok_or_else(|| fsx::error(format!("skill sync candidate not found: {id}")))?;
            selected.push(candidate.clone());
        }
        let mut entries = Vec::new();
        for candidate in &selected {
            let directory = fsx::dirname(&candidate.path);
            let source = fsx::realpath(&directory)
                .await
                .map_err(|error| fsx::fs_error(&error, "realpath", &directory))?;
            entries.push(Entry::Source {
                source_path: source,
                archive_path: candidate.directory_name.clone(),
            });
        }
        let selected_bytes: u64 = selected.iter().map(|candidate| candidate.size_bytes).sum();
        if selected_bytes > self.max_archive_bytes {
            return Err(size_error(
                selected_bytes,
                self.max_archive_bytes,
                "selected-content",
            ));
        }
        let archive = archive::create(Flavor::Skill, entries).await?;
        if archive.len() as u64 > self.max_archive_bytes {
            return Err(size_error(
                archive.len() as u64,
                self.max_archive_bytes,
                "archive",
            ));
        }
        Ok(json!({
            "archive": bytes_value(&archive),
            "archiveBytes": archive.len(),
            "skills": selected.iter().map(|candidate| json!({
                "id": candidate.id,
                "name": candidate.name,
                "directoryName": candidate.directory_name,
            })).collect::<Vec<_>>(),
        }))
    }

    pub async fn import_archive(&self, params: Value) -> Result<Value, RpcError> {
        let overwrite = params.get("overwrite").is_some_and(truthy);
        if overwrite {
            return Err(fsx::error("skill sync overwrite is not supported"));
        }
        let archive_bytes = bytes_param(params.get("archive"))?;
        if archive_bytes.len() as u64 > self.max_archive_bytes {
            return Err(size_error(
                archive_bytes.len() as u64,
                self.max_archive_bytes,
                "archive",
            ));
        }
        let target_root = self.zcode_root();
        fsx::mkdirp(&target_root).await?;
        let temp = fsx::join(
            &target_root,
            &[&format!(".sync-tmp-{}", fsx::random_uuid())],
        );
        fsx::mkdirp(&temp).await?;
        let result = self.import_into(&archive_bytes, &target_root, &temp).await;
        let _ = fsx::remove_all(&temp).await;
        result
    }

    async fn import_into(
        &self,
        archive_bytes: &[u8],
        target_root: &str,
        temp: &str,
    ) -> Result<Value, RpcError> {
        // 先解到临时目录并校验 SKILL.md，再逐个复制，避免路径穿越或半成品目录污染。
        archive::extract(
            Flavor::Skill,
            archive_bytes,
            temp,
            Some(self.max_archive_bytes),
        )
        .await?;
        let extracted = extracted_directories(temp).await?;
        let mut existing = self.user_directory_by_name().await?;
        let mut results = Vec::new();
        for (directory_name, source) in extracted {
            let target = archive::resolve_within(target_root, &directory_name, SYNC_PATH_LABEL)?;
            let Some(content) = read_skill_file(&fsx::join(&source, &[SKILL_FILE_NAME])).await?
            else {
                results.push(json!({
                    "name": fsx::basename(&directory_name),
                    "directoryName": directory_name,
                    "status": "failed",
                    "error": "SKILL.md is missing",
                }));
                continue;
            };
            let (name, _) = skill_metadata(&content, &directory_name);
            let key = name_key(&name);
            if std::path::Path::new(&target).exists() {
                results.push(json!({ "name": name, "directoryName": directory_name, "status": "skipped", "path": target }));
                continue;
            }
            if let Some(path) = existing.get(&key) {
                results.push(json!({ "name": name, "directoryName": directory_name, "status": "skipped", "path": path }));
                continue;
            }
            let copied: Result<(), RpcError> = async {
                fsx::mkdirp(&fsx::dirname(&target)).await?;
                fsx::copy_recursive(&source, &target, false).await
            }
            .await;
            match copied {
                Ok(()) => {
                    results.push(json!({ "name": name, "directoryName": directory_name, "status": "synced", "path": target }));
                    existing.insert(key, target);
                }
                Err(error) => results.push(json!({
                    "name": name,
                    "directoryName": directory_name,
                    "status": "failed",
                    "path": target,
                    "error": error.message,
                })),
            }
        }
        Ok(json!({ "results": results }))
    }
}

pub(crate) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Value::String(text) => !text.is_empty(),
        _ => true,
    }
}

async fn extracted_directories(root: &str) -> Result<Vec<(String, String)>, RpcError> {
    let mut directories = Vec::new();
    let mut stack = vec![(root.to_owned(), String::new())];
    while let Some((directory, relative)) = stack.pop() {
        if !relative.is_empty()
            && std::path::Path::new(&fsx::join(&directory, &[SKILL_FILE_NAME])).exists()
        {
            directories.push((relative.clone(), directory.clone()));
        }
        let entries = fsx::read_dir(&directory)
            .await
            .map_err(|error| fsx::fs_error(&error, "scandir", &directory))?;
        for entry in entries {
            if !entry.is_dir || !walk::should_walk(&entry.name) {
                continue;
            }
            let child = if relative.is_empty() {
                entry.name.clone()
            } else {
                format!("{relative}/{}", entry.name)
            };
            stack.push((fsx::join(&directory, &[&entry.name]), child));
        }
    }
    directories.sort_by(|left, right| locale_compare(&left.0, &right.0));
    Ok(directories)
}

pub fn service(sync: SkillSyncService) -> ServiceBuilder {
    let builder = ServiceBuilder::new(CHANNEL);
    let builder = bind(
        builder,
        "listLocalUserSkillCandidates",
        &sync,
        |s, _| async move { s.list_local_candidates().await.map(Reply::Value) },
    );
    let builder = bind(
        builder,
        "listRemoteUserSkillStatuses",
        &sync,
        |s, p| async move { s.list_remote_statuses(p).await.map(Reply::Value) },
    );
    let builder = bind(builder, "exportSkillsArchive", &sync, |s, p| async move {
        s.export_archive(p).await.map(Reply::Value)
    });
    let builder = bind(
        builder,
        "checkRemoteUserSkillWriteAccess",
        &sync,
        |s, _| async move { Ok(Reply::Value(check_write_access(&s.zcode_root()).await)) },
    );
    bind(builder, "importSkillsArchive", &sync, |s, p| async move {
        s.import_archive(p).await.map(Reply::Value)
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn metadata_regex_handles_crlf() {
        assert_eq!(
            skill_metadata("---\r\nname: a\r\ndescription: d\r\n---\r\nbody", "f"),
            ("a".to_owned(), "d".to_owned())
        );
        assert_eq!(skill_metadata("no", "f"), ("f".to_owned(), String::new()));
        assert_eq!(skill_metadata("---\nname: [x\n---\n", "f").0, "f");
    }

    #[tokio::test]
    async fn export_then_import_skips_existing() {
        let home = tempfile::tempdir().unwrap();
        let skill = home.path().join(".zcode/skills/group/demo");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\nname: demo\n---\nbody").unwrap();
        let roots = Roots::new(home.path(), home.path());
        let service = SkillSyncService::new(roots);
        let list = service.list_local_candidates().await.unwrap();
        let id = list["candidates"][0]["id"].as_str().unwrap().to_owned();
        assert_eq!(list["candidates"][0]["directoryName"], "group/demo");
        let exported = service
            .export_archive(json!({ "skillIds": [id] }))
            .await
            .unwrap();
        let remote_home = tempfile::tempdir().unwrap();
        let remote = SkillSyncService::new(Roots::new(remote_home.path(), remote_home.path()));
        let imported = remote
            .import_archive(json!({ "archive": exported["archive"] }))
            .await
            .unwrap();
        assert_eq!(imported["results"][0]["status"], "synced");
        let again = remote
            .import_archive(json!({ "archive": exported["archive"] }))
            .await
            .unwrap();
        assert_eq!(again["results"][0]["status"], "skipped");
        let statuses = remote
            .list_remote_statuses(json!({ "directoryNames": ["group/demo", "x"] }))
            .await
            .unwrap();
        assert_eq!(statuses["statuses"][0]["exists"], true);
        assert_eq!(statuses["statuses"][1]["exists"], false);
        assert!(
            remote
                .list_remote_statuses(json!({ "directoryNames": ["../x"] }))
                .await
                .is_err()
        );
    }
}
