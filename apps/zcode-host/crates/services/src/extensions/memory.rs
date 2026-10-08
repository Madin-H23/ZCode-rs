//! `memory` channel（packages/services/src/memory/memoryService.ts、projectMemoryStableRead.ts）：
//! 只读地列出与预览 `{dataBaseDir}/.zcode/cli/memories/projects/<id>/memory/*.md`。
use std::io::ErrorKind;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::fs;
use tokio::io::AsyncReadExt;
use zcode_host_kit::{Reply, RpcError, ServiceBuilder};

use super::{bind, fsx};
use crate::support::collate::locale_compare;

pub const CHANNEL: &str = "memory";
const INDEX_FILE: &str = "MEMORY.md";
const PREVIEW_MAX_BYTES: u64 = 5 * 1024 * 1024;
pub const PREVIEW_LIMIT_CODE: &str = "PROJECT_MEMORY_PREVIEW_LIMIT_EXCEEDED";
pub const FILE_CHANGED_CODE: &str = "PROJECT_MEMORY_FILE_CHANGED";

#[derive(Clone)]
pub struct MemoryService {
    projects_root: String,
}

fn not_found(error: &std::io::Error) -> bool {
    error.kind() == ErrorKind::NotFound
}

fn is_memory_file_name(name: &str) -> bool {
    name == INDEX_FILE || name.ends_with(".md")
}

fn valid_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
}

/// `/^(.*)-[a-f0-9]{16}$/i` 的 slug（trim 后非空），否则用 id 本身。
fn workspace_label(id: &str) -> String {
    let bytes = id.as_bytes();
    if bytes.len() >= 17 {
        let suffix = &id[id.len() - 16..];
        if id.is_char_boundary(id.len() - 17)
            && bytes[bytes.len() - 17] == b'-'
            && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            let slug = zcode_cli_domain::js_string::trim(&id[..id.len() - 17]);
            if !slug.is_empty() {
                return slug.to_owned();
            }
        }
    }
    id.to_owned()
}

fn mtime_ms(meta: &std::fs::Metadata) -> f64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as f64 * 1000.0 + duration.subsec_nanos() as f64 / 1e6)
        .unwrap_or(0.0)
}

fn mtime_ns(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

async fn require_plain_directory(path: &str) -> Result<(), RpcError> {
    let meta = fs::symlink_metadata(path)
        .await
        .map_err(|error| fsx::fs_error(&error, "lstat", path))?;
    if !meta.is_dir() || meta.is_symlink() {
        return Err(fsx::error(format!(
            "Project Memory directory is not a regular directory: {path}"
        )));
    }
    Ok(())
}

async fn is_plain_directory(path: &str) -> Result<bool, RpcError> {
    match fs::symlink_metadata(path).await {
        Ok(meta) => Ok(meta.is_dir() && !meta.is_symlink()),
        Err(error) if not_found(&error) => Ok(false),
        Err(error) => Err(fsx::fs_error(&error, "lstat", path)),
    }
}

/// 文件身份快照（dev、ino、size、mtime、ctime 纳秒）。
#[derive(PartialEq, Eq)]
struct Snapshot {
    dev: u64,
    ino: u64,
    size: u64,
    mtime_ns: u128,
    ctime_ns: i128,
}

#[cfg(unix)]
fn snapshot(meta: &std::fs::Metadata) -> Snapshot {
    use std::os::unix::fs::MetadataExt;
    Snapshot {
        dev: meta.dev(),
        ino: meta.ino(),
        size: meta.size(),
        mtime_ns: mtime_ns(meta),
        ctime_ns: i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec()),
    }
}

#[cfg(not(unix))]
fn snapshot(meta: &std::fs::Metadata) -> Snapshot {
    Snapshot {
        dev: 0,
        ino: 0,
        size: meta.len(),
        mtime_ns: mtime_ns(meta),
        ctime_ns: 0,
    }
}

fn changed(file_name: &str) -> RpcError {
    fsx::error(format!(
        "Project Memory file changed during preview: {file_name}"
    ))
    .with_code(FILE_CHANGED_CODE)
}

fn too_large(file_name: &str) -> RpcError {
    fsx::error(format!(
        "Project Memory file exceeds the 5 MiB preview limit: {file_name}"
    ))
    .with_code(PREVIEW_LIMIT_CODE)
}

impl MemoryService {
    pub fn new(data_root: &str) -> Self {
        Self {
            projects_root: fsx::join(data_root, &["cli", "memories", "projects"]),
        }
    }

    async fn require_projects_root(&self) -> Result<String, RpcError> {
        require_plain_directory(&self.projects_root).await?;
        Ok(self.projects_root.clone())
    }

    pub async fn list_project_memories(&self) -> Result<Value, RpcError> {
        let root = self.projects_root.clone();
        match fs::symlink_metadata(&root).await {
            Err(error) if not_found(&error) => return Ok(json!([])),
            Err(error) => return Err(fsx::fs_error(&error, "lstat", &root)),
            Ok(meta) if !meta.is_dir() || meta.is_symlink() => {
                return Err(fsx::error(format!(
                    "Project Memory directory is not a regular directory: {root}"
                )));
            }
            Ok(_) => {}
        }
        let projects = match fsx::read_dir(&root).await {
            Ok(entries) => entries,
            Err(error) if not_found(&error) => return Ok(json!([])),
            Err(error) => return Err(fsx::fs_error(&error, "scandir", &root)),
        };
        let mut workspaces: Vec<(f64, String, Value)> = Vec::new();
        for project in projects {
            if !project.is_dir || project.is_symlink {
                continue;
            }
            let workspace_root = fsx::join(&root, &[&project.name]);
            let memory_root = fsx::join(&workspace_root, &["memory"]);
            if !is_plain_directory(&workspace_root).await? || !is_plain_directory(&memory_root).await?
            {
                continue;
            }
            let entries = match fsx::read_dir(&memory_root).await {
                Ok(entries) => entries,
                Err(error) if not_found(&error) => continue,
                Err(error) => return Err(fsx::fs_error(&error, "scandir", &memory_root)),
            };
            let mut files: Vec<(bool, String, f64, Value)> = Vec::new();
            for entry in entries {
                if !entry.is_file || entry.is_symlink || !is_memory_file_name(&entry.name) {
                    continue;
                }
                let path = fsx::join(&memory_root, &[&entry.name]);
                let meta = match fs::symlink_metadata(&path).await {
                    Ok(meta) => meta,
                    Err(error) if not_found(&error) => continue,
                    Err(error) => return Err(fsx::fs_error(&error, "lstat", &path)),
                };
                if !meta.is_file() || meta.is_symlink() {
                    continue;
                }
                let index = entry.name == INDEX_FILE;
                let updated_at = mtime_ms(&meta);
                files.push((
                    index,
                    entry.name.clone(),
                    updated_at,
                    json!({
                        "name": entry.name,
                        "path": path,
                        "kind": if index { "index" } else { "item" },
                        "size": meta.len(),
                        "updatedAt": super::json::number(updated_at),
                    }),
                ));
            }
            if files.is_empty() {
                continue;
            }
            files.sort_by(|left, right| {
                right
                    .0
                    .cmp(&left.0)
                    .then_with(|| locale_compare(&left.1, &right.1))
            });
            let updated_at = files
                .iter()
                .map(|file| file.2)
                .fold(f64::NEG_INFINITY, f64::max);
            workspaces.push((
                updated_at,
                project.name.clone(),
                json!({
                    "id": project.name,
                    "label": workspace_label(&project.name),
                    "updatedAt": super::json::number(updated_at),
                    "files": files.into_iter().map(|file| file.3).collect::<Vec<_>>(),
                }),
            ));
        }
        workspaces.sort_by(|left, right| {
            right
                .0
                .partial_cmp(&left.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| locale_compare(&left.1, &right.1))
        });
        Ok(Value::Array(
            workspaces.into_iter().map(|entry| entry.2).collect(),
        ))
    }

    async fn require_exact_file(&self, memory_root: &str, name: &str) -> Result<String, RpcError> {
        let entries = fsx::read_dir(memory_root)
            .await
            .map_err(|error| fsx::fs_error(&error, "scandir", memory_root))?;
        let requested = fsx::join(memory_root, &[name]);
        let Some(entry) = entries.iter().find(|entry| entry.name == name) else {
            // 文件确实不存在时透传原始 ENOENT；只有大小写别名能命中时才拒绝读取。
            fs::symlink_metadata(&requested)
                .await
                .map_err(|error| fsx::fs_error(&error, "lstat", &requested))?;
            return Err(fsx::error(format!(
                "Project Memory file name does not match exactly: {name}"
            )));
        };
        if !entry.is_file || entry.is_symlink {
            return Err(fsx::error(format!(
                "Project Memory file is not a regular file: {name}"
            )));
        }
        Ok(requested)
    }

    async fn validate(
        &self,
        projects_root: &str,
        workspace_root: &str,
        memory_root: &str,
        name: &str,
        file_path: &str,
    ) -> Result<(), RpcError> {
        self.require_projects_root().await?;
        require_plain_directory(workspace_root).await?;
        require_plain_directory(memory_root).await?;
        self.require_exact_file(memory_root, name).await?;
        let root_real = fsx::realpath(projects_root)
            .await
            .map_err(|error| fsx::fs_error(&error, "realpath", projects_root))?;
        let target_real = fsx::realpath(file_path)
            .await
            .map_err(|error| fsx::fs_error(&error, "realpath", file_path))?;
        let relative = fsx::relative(&root_real, &target_real);
        let sep = std::path::MAIN_SEPARATOR;
        if relative == ".." || relative.starts_with(&format!("..{sep}")) || fsx::is_absolute(&relative)
        {
            return Err(fsx::error(format!(
                "Project Memory path is outside the local profile: {file_path}"
            )));
        }
        Ok(())
    }

    pub async fn read_project_memory_file(
        &self,
        workspace_id: &str,
        file_name: &str,
    ) -> Result<Value, RpcError> {
        if !valid_segment(workspace_id) || !valid_segment(file_name) || !is_memory_file_name(file_name)
        {
            return Err(fsx::error("Invalid Project Memory path"));
        }
        let projects_root = self.require_projects_root().await?;
        let workspace_root = fsx::join(&projects_root, &[workspace_id]);
        let memory_root = fsx::join(&workspace_root, &["memory"]);
        require_plain_directory(&workspace_root).await?;
        require_plain_directory(&memory_root).await?;
        let file_path = self.require_exact_file(&memory_root, file_name).await?;

        let pre = fs::symlink_metadata(&file_path)
            .await
            .map_err(|error| fsx::fs_error(&error, "lstat", &file_path))?;
        if !pre.is_file() || pre.is_symlink() {
            return Err(fsx::error(format!(
                "Project Memory file is not a regular file: {file_path}"
            )));
        }
        // 路径检查后再按路径读取会重新解析路径；只读句柄（O_NOFOLLOW）打开后全程从句柄读取。
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW);
        let mut handle = options
            .open(&file_path)
            .await
            .map_err(|error| fsx::fs_error(&error, "open", &file_path))?;
        let opened = handle
            .metadata()
            .await
            .map_err(|error| fsx::fs_error(&error, "fstat", &file_path))?;
        self.validate(
            &projects_root,
            &workspace_root,
            &memory_root,
            file_name,
            &file_path,
        )
        .await?;
        let post = fs::symlink_metadata(&file_path)
            .await
            .map_err(|error| fsx::fs_error(&error, "lstat", &file_path))?;
        if !opened.is_file()
            || !post.is_file()
            || post.is_symlink()
            || snapshot(&opened) != snapshot(&pre)
            || snapshot(&opened) != snapshot(&post)
        {
            return Err(changed(file_name));
        }
        if opened.len() > PREVIEW_MAX_BYTES {
            return Err(too_large(file_name));
        }
        let mut content = Vec::new();
        (&mut handle)
            .take(PREVIEW_MAX_BYTES + 1)
            .read_to_end(&mut content)
            .await
            .map_err(|error| fsx::fs_error(&error, "read", &file_path))?;
        let last = handle
            .metadata()
            .await
            .map_err(|error| fsx::fs_error(&error, "fstat", &file_path))?;
        // 原子 rename 失败时写入会退化为同 inode 原地覆盖，只比文件身份无法识别内容变化。
        if snapshot(&opened) != snapshot(&last) {
            return Err(changed(file_name));
        }
        if content.len() as u64 > PREVIEW_MAX_BYTES || last.len() > PREVIEW_MAX_BYTES {
            return Err(too_large(file_name));
        }
        Ok(json!({
            "content": String::from_utf8_lossy(&content),
            "updatedAt": super::json::number(mtime_ns(&last) as f64 / 1_000_000.0),
        }))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadParams {
    workspace_id: String,
    file_name: String,
}

pub fn service(memory: MemoryService) -> ServiceBuilder {
    let builder = ServiceBuilder::new(CHANNEL);
    let builder = bind(builder, "listProjectMemories", &memory, |s, _| async move {
        s.list_project_memories().await.map(Reply::Value)
    });
    bind(builder, "readProjectMemoryFile", &memory, |s, p| async move {
        let params: ReadParams = super::parse_params(p)?;
        s.read_project_memory_file(&params.workspace_id, &params.file_name)
            .await
            .map(Reply::Value)
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn labels_strip_the_hash_suffix() {
        assert_eq!(workspace_label("my-repo-0123456789abcdef"), "my-repo");
        assert_eq!(workspace_label("-0123456789abcdef"), "-0123456789abcdef");
        assert_eq!(workspace_label("plain"), "plain");
    }

    #[tokio::test]
    async fn lists_and_reads_memories() {
        let dir = tempfile::tempdir().unwrap();
        let data_root = dir.path().display().to_string();
        let memory_dir = dir
            .path()
            .join("cli/memories/projects/repo-0123456789abcdef/memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::write(memory_dir.join("MEMORY.md"), "# index").unwrap();
        std::fs::write(memory_dir.join("b.md"), "b").unwrap();
        std::fs::write(memory_dir.join("skip.txt"), "x").unwrap();
        std::os::unix::fs::symlink(memory_dir.join("b.md"), memory_dir.join("link.md")).unwrap();
        let service = MemoryService::new(&data_root);
        let list = service.list_project_memories().await.unwrap();
        assert_eq!(list[0]["label"], "repo");
        let names: Vec<&str> = list[0]["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["MEMORY.md", "b.md"]);
        let read = service
            .read_project_memory_file("repo-0123456789abcdef", "MEMORY.md")
            .await
            .unwrap();
        assert_eq!(read["content"], "# index");
        let error = service
            .read_project_memory_file("repo-0123456789abcdef", "memory.md")
            .await
            .unwrap_err();
        assert!(
            error.message.contains("does not match exactly") || error.message.contains("ENOENT")
        );
        assert!(
            service
                .read_project_memory_file("..", "MEMORY.md")
                .await
                .is_err()
        );
        let link = service
            .read_project_memory_file("repo-0123456789abcdef", "link.md")
            .await
            .unwrap_err();
        assert!(link.message.contains("not a regular file"));
    }
}
