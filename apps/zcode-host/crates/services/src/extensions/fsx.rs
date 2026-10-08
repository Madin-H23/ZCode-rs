//! 扩展服务共用的路径与文件系统工具：Node `path.join`、`fs.access` / `realpath` / `cp` 的语义，
//! 以及 Node 风格的错误文案（UI 会直接展示这些文案）。
use std::io::ErrorKind;
use std::path::{MAIN_SEPARATOR, MAIN_SEPARATOR_STR, Path};

use futures_util::future::BoxFuture;
use tokio::fs;
use zcode_host_kit::RpcError;

use crate::support::node_error;
use crate::support::path as node_path;

/// `new Error(message)` 跨 RPC 的形态。
pub fn error(message: impl Into<String>) -> RpcError {
    RpcError::new("Error", message.into())
}

/// Node fs 调用失败时的 `Error`：`ENOENT: no such file or directory, open '/x'`。
pub fn fs_error(source: &std::io::Error, syscall: &str, path: &str) -> RpcError {
    let mut error = RpcError::new("Error", node_error::fs_message(source, syscall, path));
    if let Some((code, _)) = node_error::code_of(source) {
        error = error.with_code(code);
    }
    error
}

pub fn fs_message(source: &std::io::Error, syscall: &str, path: &str) -> String {
    node_error::fs_message(source, syscall, path)
}

/// `path.join(...)`：拼接后按词法规范化（不访问文件系统）。
pub fn join(base: &str, parts: &[&str]) -> String {
    let mut joined = base.to_owned();
    for part in parts {
        if part.is_empty() {
            continue;
        }
        if !joined.is_empty() && !joined.ends_with(['/', MAIN_SEPARATOR]) {
            joined.push(MAIN_SEPARATOR);
        }
        joined.push_str(part);
    }
    normalize(&joined)
}

/// `path.normalize`（不保留末尾分隔符）。
pub fn normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }
    node_path::resolve(path, &[])
}

/// `path.resolve(...)`：相对路径相对进程 cwd。
pub fn resolve(base: &str, parts: &[&str]) -> String {
    let resolved = node_path::resolve(base, parts);
    if node_path::is_absolute(&resolved) {
        return resolved;
    }
    let cwd = std::env::current_dir()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_else(|_| MAIN_SEPARATOR_STR.to_owned());
    let mut segments = vec![resolved.as_str()];
    segments.retain(|segment| *segment != ".");
    node_path::resolve(&cwd, &segments)
}

pub fn relative(from: &str, to: &str) -> String {
    node_path::relative(&resolve(from, &[]), &resolve(to, &[]))
}

pub fn is_absolute(path: &str) -> bool {
    node_path::is_absolute(path)
}

/// `path.dirname`。
pub fn dirname(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', MAIN_SEPARATOR]);
    if trimmed.is_empty() {
        return if path.is_empty() {
            ".".to_owned()
        } else {
            MAIN_SEPARATOR_STR.to_owned()
        };
    }
    match trimmed.rfind(['/', MAIN_SEPARATOR]) {
        Some(0) => MAIN_SEPARATOR_STR.to_owned(),
        Some(index) => trimmed[..index].to_owned(),
        None => ".".to_owned(),
    }
}

/// `path.basename`。
pub fn basename(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', MAIN_SEPARATOR]);
    match trimmed.rfind(['/', MAIN_SEPARATOR]) {
        Some(index) => trimmed[index + 1..].to_owned(),
        None => trimmed.to_owned(),
    }
}

/// `fs.access`（存在且可访问）。
pub async fn exists(path: &str) -> bool {
    fs::metadata(path).await.is_ok()
}

/// `lstat` 能成功（软链本身存在即可）。
pub async fn lexists(path: &str) -> bool {
    fs::symlink_metadata(path).await.is_ok()
}

pub async fn is_dir(path: &str) -> bool {
    fs::metadata(path).await.is_ok_and(|meta| meta.is_dir())
}

/// `realpath(path).catch(() => path)`。
pub async fn realpath_or_self(path: &str) -> String {
    node_path::realpath(path)
        .await
        .unwrap_or_else(|_| path.to_owned())
}

pub async fn realpath(path: &str) -> std::io::Result<String> {
    node_path::realpath(path).await
}

/// `readFile(path, "utf-8")`：非法 UTF-8 按替换字符解码（与 Node 相同）。
pub async fn read_text(path: &str) -> std::io::Result<String> {
    let bytes = fs::read(path).await?;
    Ok(String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned()))
}

pub async fn read_text_opt(path: &str) -> Option<String> {
    read_text(path).await.ok()
}

/// 目录项（`readdir(withFileTypes)` 的子集）。
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub is_file: bool,
    pub is_symlink: bool,
}

/// `readdir(path, { withFileTypes: true })`。libuv 在 Unix 上用 `scandir(3)` 并按 `strcmp` 排序，
/// 遍历顺序会进入结果（命令列表、去重的先到先得），所以这里同样按字节序排序；Windows 上两者
/// 都直接用文件系统给出的顺序。
pub async fn read_dir(path: &str) -> std::io::Result<Vec<Entry>> {
    let mut reader = fs::read_dir(path).await?;
    let mut entries = Vec::new();
    while let Some(entry) = reader.next_entry().await? {
        let file_type = entry.file_type().await?;
        entries.push(Entry {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir: file_type.is_dir(),
            is_file: file_type.is_file(),
            is_symlink: file_type.is_symlink(),
        });
    }
    #[cfg(unix)]
    entries.sort_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
    Ok(entries)
}

/// `fs.cp(source, target, { recursive: true, errorOnExist, force })`：目录递归、文件复制（保留
/// 权限位）、软链按 Node 默认（非 verbatim）把相对目标解析为绝对路径后重建。
pub fn copy_recursive<'a>(
    source: &'a str,
    target: &'a str,
    overwrite: bool,
) -> BoxFuture<'a, Result<(), RpcError>> {
    Box::pin(async move {
        let meta = fs::symlink_metadata(source)
            .await
            .map_err(|error| fs_error(&error, "lstat", source))?;
        if meta.is_symlink() {
            let link = fs::read_link(source)
                .await
                .map_err(|error| fs_error(&error, "readlink", source))?;
            let mut link_target = link.to_string_lossy().into_owned();
            if !is_absolute(&link_target) {
                link_target = resolve(&dirname(source), &[&link_target]);
            }
            if lexists(target).await {
                if !overwrite {
                    return Err(error(format!(
                        "Target already exists: cp returned EEXIST ({target} already exists)"
                    )));
                }
                let _ = fs::remove_file(target).await;
            }
            symlink(&link_target, target)
                .await
                .map_err(|error| fs_error(&error, "symlink", target))?;
            return Ok(());
        }
        if meta.is_dir() {
            if let Err(error) = fs::create_dir(target).await
                && error.kind() != ErrorKind::AlreadyExists
            {
                return Err(fs_error(&error, "mkdir", target));
            }
            let entries = read_dir(source)
                .await
                .map_err(|error| fs_error(&error, "opendir", source))?;
            for entry in entries {
                let child_source = join(source, &[&entry.name]);
                let child_target = join(target, &[&entry.name]);
                copy_recursive(&child_source, &child_target, overwrite).await?;
            }
            return Ok(());
        }
        if lexists(target).await && !overwrite {
            return Err(error(format!(
                "Target already exists: cp returned EEXIST ({target} already exists)"
            )));
        }
        fs::copy(source, target)
            .await
            .map_err(|error| fs_error(&error, "copyfile", source))?;
        Ok(())
    })
}

#[cfg(unix)]
async fn symlink(target: &str, link: &str) -> std::io::Result<()> {
    fs::symlink(target, link).await
}

#[cfg(windows)]
async fn symlink(target: &str, link: &str) -> std::io::Result<()> {
    if Path::new(target).is_dir() {
        fs::symlink_dir(target, link).await
    } else {
        fs::symlink_file(target, link).await
    }
}

/// `rm(path, { recursive: true, force: true })`：软链只删链接本身。
pub async fn remove_all(path: &str) -> std::io::Result<()> {
    match fs::symlink_metadata(path).await {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path).await,
        Ok(_) => fs::remove_file(path).await,
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// `mkdir(path, { recursive: true })`。
pub async fn mkdirp(path: &str) -> Result<(), RpcError> {
    fs::create_dir_all(path)
        .await
        .map_err(|error| fs_error(&error, "mkdir", path))
}

pub fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// 随机 UUID v4 文本（`crypto.randomUUID()`）。
pub fn random_uuid() -> String {
    crate::agent::service::uuid_v4()
}

/// sha256 十六进制。
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn path_helpers_match_node() {
        assert_eq!(join("/a/b/", &["../c", "d"]), "/a/c/d");
        assert_eq!(join("/a", &["", "b"]), "/a/b");
        assert_eq!(dirname("/a/b/SKILL.md"), "/a/b");
        assert_eq!(dirname("/a"), "/");
        assert_eq!(dirname("a"), ".");
        assert_eq!(basename("/a/b/"), "b");
        assert_eq!(relative("/a/b", "/a/b/c/d"), "c/d");
        assert_eq!(sha256_hex(b"abc").len(), 64);
    }
}
