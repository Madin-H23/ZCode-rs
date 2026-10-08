//! skill-sync / plugin-sync 的 tar.gz 归档（skillSyncArchive.ts、pluginSyncArchive.ts）。
//!
//! 格式与 Node 相同：ustar 头（owner/group 名 `zcode`、uid/gid 0、mode & 0o777、mtime 秒），
//! 目录项带末尾 `/`，子项按 localeCompare 排序，长路径拆到 prefix 字段，末尾两个全零块，再 gzip。
//! 两种归档只在错误文案、头字段溢出处理与解包时的权限恢复上不同（[`Flavor`]）。
use std::io::{Read, Write};
use std::path::MAIN_SEPARATOR;

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use futures_util::future::BoxFuture;
use serde_json::json;
use tokio::fs;
use zcode_host_kit::RpcError;

use super::fsx;
use crate::support::collate::locale_compare;

const BLOCK: usize = 512;
const END_BLOCKS: usize = BLOCK * 2;
pub const SKILL_SIZE_LIMIT_CODE: &str = "SKILL_SYNC_SIZE_LIMIT_EXCEEDED";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Skill,
    Plugin,
}

impl Flavor {
    fn noun(self) -> &'static str {
        match self {
            Flavor::Skill => "skill",
            Flavor::Plugin => "plugin",
        }
    }

    fn unsafe_label(self) -> String {
        format!("unsafe {} archive path", self.noun())
    }
}

/// `normalize{Skill,Plugin}SyncRelativePath`：只接受相对、无 `.`/`..`/空段、无反斜杠的路径。
pub fn normalize_relative(path: &str, label: &str) -> Result<String, RpcError> {
    let replaced = path.replace('\\', "/");
    let normalized = replaced.trim_start_matches('/').trim_end_matches('/');
    let drive =
        path.len() >= 2 && path.as_bytes()[0].is_ascii_alphabetic() && path.as_bytes()[1] == b':';
    if normalized.is_empty()
        || fsx::is_absolute(path)
        || path.starts_with('/')
        || path.contains('\\')
        || drive
        || normalized
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(fsx::error(format!("{label}: {path}")));
    }
    Ok(normalized.to_owned())
}

/// `resolve{Skill,Plugin}SyncPathWithin`：结果必须严格落在 root 之下。
pub fn resolve_within(root: &str, path: &str, label: &str) -> Result<String, RpcError> {
    let normalized_root = fsx::resolve(root, &[]);
    let normalized = normalize_relative(path, label)?;
    let parts: Vec<&str> = normalized.split('/').collect();
    let target = fsx::resolve(&normalized_root, &parts);
    let relative = fsx::relative(&normalized_root, &target);
    if relative.is_empty()
        || relative.starts_with(&format!("..{MAIN_SEPARATOR}"))
        || relative == ".."
        || fsx::is_absolute(&relative)
    {
        return Err(fsx::error(format!("{label}: {path}")));
    }
    Ok(target)
}

/// 归档条目：磁盘路径（递归）或内存内容。
pub enum Entry {
    Source {
        source_path: String,
        archive_path: String,
    },
    Content {
        archive_path: String,
        content: Vec<u8>,
    },
}

struct Builder {
    flavor: Flavor,
    out: Vec<u8>,
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64() * 1000.0)
        .unwrap_or_default()
}

fn mtime_seconds(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode()
}

#[cfg(not(unix))]
fn mode_of(meta: &std::fs::Metadata) -> u32 {
    if meta.is_dir() {
        0o755
    } else if meta.permissions().readonly() {
        0o444
    } else {
        0o666
    }
}

impl Builder {
    fn write_string(
        &self,
        header: &mut [u8],
        value: &str,
        offset: usize,
        length: usize,
    ) -> Result<(), RpcError> {
        let bytes = value.as_bytes();
        if bytes.len() > length {
            if self.flavor == Flavor::Plugin {
                return Err(fsx::error(format!(
                    "plugin archive header value is too long: {value}"
                )));
            }
            // Buffer.write 截断到可写长度。
            header[offset..offset + length].copy_from_slice(&bytes[..length]);
            return Ok(());
        }
        header[offset..offset + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }

    fn write_octal(
        &self,
        header: &mut [u8],
        value: u64,
        offset: usize,
        length: usize,
    ) -> Result<(), RpcError> {
        let encoded = format!("{value:o}");
        let width = length - 1;
        let padded = format!("{encoded:0>width$}");
        let text = match self.flavor {
            Flavor::Skill => padded[padded.len() - width..].to_owned(),
            Flavor::Plugin => padded,
        };
        self.write_string(header, &text, offset, width)?;
        header[offset + length - 1] = 0;
        Ok(())
    }

    fn split_path(&self, entry_path: &str) -> Result<(String, String), RpcError> {
        if entry_path.len() <= 100 {
            return Ok((entry_path.to_owned(), String::new()));
        }
        let segments: Vec<&str> = entry_path.split('/').collect();
        for index in (1..segments.len()).rev() {
            let prefix = segments[..index].join("/");
            let name = segments[index..].join("/");
            if prefix.len() <= 155 && name.len() <= 100 {
                return Ok((name, prefix));
            }
        }
        Err(fsx::error(format!(
            "{} archive path is too long: {entry_path}",
            self.flavor.noun()
        )))
    }

    fn header(
        &self,
        entry_path: &str,
        mode: u32,
        mtime: u64,
        size: u64,
        directory: bool,
    ) -> Result<[u8; BLOCK], RpcError> {
        let mut header = [0u8; BLOCK];
        let (name, prefix) = self.split_path(entry_path)?;
        self.write_string(&mut header, &name, 0, 100)?;
        self.write_octal(&mut header, u64::from(mode & 0o777), 100, 8)?;
        self.write_octal(&mut header, 0, 108, 8)?;
        self.write_octal(&mut header, 0, 116, 8)?;
        self.write_octal(&mut header, size, 124, 12)?;
        self.write_octal(&mut header, mtime, 136, 12)?;
        header[148..156].fill(0x20);
        self.write_string(&mut header, if directory { "5" } else { "0" }, 156, 1)?;
        self.write_string(&mut header, "ustar", 257, 6)?;
        self.write_string(&mut header, "00", 263, 2)?;
        self.write_string(&mut header, "zcode", 265, 32)?;
        self.write_string(&mut header, "zcode", 297, 32)?;
        self.write_string(&mut header, &prefix, 345, 155)?;
        let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
        let encoded = format!("{checksum:06o}");
        let encoded = match self.flavor {
            Flavor::Skill => encoded[encoded.len().saturating_sub(6)..].to_owned(),
            Flavor::Plugin => encoded,
        };
        self.write_string(&mut header, &encoded, 148, 6)?;
        header[154] = 0;
        header[155] = 0x20;
        Ok(header)
    }

    fn append_file(
        &mut self,
        archive_path: &str,
        content: &[u8],
        mode: u32,
        mtime: u64,
    ) -> Result<(), RpcError> {
        let header = self.header(archive_path, mode, mtime, content.len() as u64, false)?;
        self.out.extend_from_slice(&header);
        self.out.extend_from_slice(content);
        let padding = content.len().div_ceil(BLOCK) * BLOCK - content.len();
        self.out.extend(std::iter::repeat_n(0u8, padding));
        Ok(())
    }

    fn append_source<'a>(
        &'a mut self,
        source: &'a str,
        archive_path: &'a str,
    ) -> BoxFuture<'a, Result<(), RpcError>> {
        Box::pin(async move {
            let meta = fs::symlink_metadata(source)
                .await
                .map_err(|error| fsx::fs_error(&error, "lstat", source))?;
            if meta.is_dir() {
                let directory_path = if archive_path.ends_with('/') {
                    archive_path.to_owned()
                } else {
                    format!("{archive_path}/")
                };
                let header = self.header(
                    &directory_path,
                    mode_of(&meta),
                    mtime_seconds(&meta),
                    0,
                    true,
                )?;
                self.out.extend_from_slice(&header);
                let mut children = fsx::read_dir(source)
                    .await
                    .map_err(|error| fsx::fs_error(&error, "scandir", source))?;
                children.sort_by(|left, right| locale_compare(&left.name, &right.name));
                for child in children {
                    let child_source = fsx::join(source, &[&child.name]);
                    let child_archive = normalize_relative(
                        &format!("{archive_path}/{}", child.name),
                        &self.flavor.unsafe_label(),
                    )?;
                    self.append_source(&child_source, &child_archive).await?;
                }
                return Ok(());
            }
            if !meta.is_file() {
                return Err(fsx::error(format!(
                    "unsupported {} archive source: {source}",
                    self.flavor.noun()
                )));
            }
            let content = fs::read(source)
                .await
                .map_err(|error| fsx::fs_error(&error, "open", source))?;
            self.append_file(archive_path, &content, mode_of(&meta), mtime_seconds(&meta))
        })
    }
}

/// 打包并 gzip。plugin 归档末尾自动带元数据文件由调用方作为 `Entry::Content` 传入。
pub async fn create(flavor: Flavor, entries: Vec<Entry>) -> Result<Vec<u8>, RpcError> {
    let mut builder = Builder {
        flavor,
        out: Vec::new(),
    };
    for entry in entries {
        match entry {
            Entry::Source {
                source_path,
                archive_path,
            } => {
                let archive_path = normalize_relative(&archive_path, &flavor.unsafe_label())?;
                builder.append_source(&source_path, &archive_path).await?;
            }
            Entry::Content {
                archive_path,
                content,
            } => {
                let archive_path = normalize_relative(&archive_path, &flavor.unsafe_label())?;
                let mtime = (now_ms() / 1000.0).floor() as u64;
                builder.append_file(&archive_path, &content, 0o644, mtime)?;
            }
        }
    }
    builder.out.extend(std::iter::repeat_n(0u8, END_BLOCKS));
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(&builder.out)
        .and_then(|()| encoder.finish())
        .map_err(|error| fsx::error(error.to_string()))
}

fn read_string(header: &[u8], offset: usize, length: usize) -> String {
    let slice = &header[offset..offset + length];
    let end = slice
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(slice.len());
    String::from_utf8_lossy(&slice[..end]).into_owned()
}

/// `Number.parseInt(raw.trim(), 8)`（空串为 0，无合法前缀为 `None`）。
fn read_octal(header: &[u8], offset: usize, length: usize) -> Option<u64> {
    let raw = read_string(header, offset, length);
    let raw = zcode_cli_domain::js_string::trim(&raw);
    if raw.is_empty() {
        return Some(0);
    }
    let digits: String = raw
        .chars()
        .take_while(|c| ('0'..='7').contains(c))
        .collect();
    if digits.is_empty() {
        return None;
    }
    u64::from_str_radix(&digits, 8).ok()
}

fn entry_path(header: &[u8]) -> String {
    let name = read_string(header, 0, 100);
    let prefix = read_string(header, 345, 155);
    if prefix.is_empty() {
        name
    } else {
        format!("{prefix}/{name}")
    }
}

fn skill_size_error(actual: u64, max: u64, phase: &str) -> RpcError {
    fsx::error(format!(
        "skill sync size limit exceeded: {actual}/{max} ({phase})"
    ))
    .with_code(SKILL_SIZE_LIMIT_CODE)
    .with_data(json!({ "actualBytes": actual, "maxBytes": max, "phase": phase }))
    .renamed("SkillSyncSizeLimitError")
}

/// 解压 gzip：skill 按流式读取到结束块；plugin 先整体解压，输出上限与 Node 相同。
fn gunzip(flavor: Flavor, archive: &[u8], max: Option<u64>) -> Result<Vec<u8>, RpcError> {
    let mut decoder = GzDecoder::new(archive);
    let mut out = Vec::new();
    match flavor {
        Flavor::Plugin => {
            let limit = max.map(|max| max + END_BLOCKS as u64 + (BLOCK * 512) as u64);
            let result = match limit {
                Some(limit) => {
                    let read = (&mut decoder).take(limit + 1).read_to_end(&mut out);
                    match read {
                        Ok(_) if out.len() as u64 > limit => {
                            Err(format!("Cannot create a Buffer larger than {limit} bytes"))
                        }
                        Ok(_) => Ok(()),
                        Err(error) => Err(error.to_string()),
                    }
                }
                None => decoder
                    .read_to_end(&mut out)
                    .map(|_| ())
                    .map_err(|error| error.to_string()),
            };
            result.map_err(|message| {
                fsx::error(format!("plugin sync archive exceeds limit: {message}"))
            })?;
        }
        Flavor::Skill => {
            decoder
                .read_to_end(&mut out)
                .map_err(|error| fsx::error(error.to_string()))?;
        }
    }
    Ok(out)
}

/// 解包到 `target_dir`；路径越界、未知类型、超过 `max_extracted` 都失败。
pub async fn extract(
    flavor: Flavor,
    archive: &[u8],
    target_dir: &str,
    max_extracted: Option<u64>,
) -> Result<(), RpcError> {
    let target_root = fsx::resolve(target_dir, &[]);
    fsx::mkdirp(&target_root).await?;
    let buffer = gunzip(flavor, archive, max_extracted)?;
    let label = flavor.unsafe_label();
    let mut offset = 0usize;
    let mut extracted: u64 = 0;
    while offset < buffer.len() {
        if flavor == Flavor::Plugin && offset + BLOCK > buffer.len() {
            break;
        }
        if offset + BLOCK > buffer.len() {
            return Err(fsx::error("truncated skill sync archive"));
        }
        let header = &buffer[offset..offset + BLOCK];
        offset += BLOCK;
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        let size = match read_octal(header, 124, 12) {
            Some(size) => size,
            None if flavor == Flavor::Skill => {
                return Err(fsx::error("invalid skill sync archive entry size"));
            }
            None => 0,
        };
        let size_usize = usize::try_from(size).unwrap_or(usize::MAX);
        let padded = size_usize.div_ceil(BLOCK).saturating_mul(BLOCK);
        let type_flag = {
            let flag = read_string(header, 156, 1);
            if flag.is_empty() {
                "0".to_owned()
            } else {
                flag
            }
        };
        let path = normalize_relative(&entry_path(header), &label)?;
        let target = resolve_within(&target_root, &path, &label)?;
        if type_flag == "5" {
            if flavor == Flavor::Skill && offset.saturating_add(padded) > buffer.len() {
                return Err(fsx::error("truncated skill sync archive"));
            }
            offset = offset.saturating_add(padded);
            fsx::mkdirp(&target).await?;
            continue;
        }
        if type_flag != "0" {
            return Err(fsx::error(format!(
                "unsupported {} archive entry type: {type_flag}",
                flavor.noun()
            )));
        }
        let next = extracted.saturating_add(size);
        if let Some(max) = max_extracted
            && next > max
        {
            return Err(match flavor {
                Flavor::Skill => skill_size_error(next, max, "extracted-content"),
                Flavor::Plugin => {
                    fsx::error(format!("plugin sync archive exceeds limit: {next}/{max}"))
                }
            });
        }
        if offset.saturating_add(size_usize) > buffer.len() {
            return Err(fsx::error(format!(
                "truncated {} sync archive entry",
                flavor.noun()
            )));
        }
        let data = &buffer[offset..offset + size_usize];
        if flavor == Flavor::Skill && offset.saturating_add(padded) > buffer.len() {
            return Err(fsx::error("truncated skill sync archive"));
        }
        offset = offset.saturating_add(padded);
        fsx::mkdirp(&fsx::dirname(&target)).await?;
        match flavor {
            Flavor::Skill => fs::write(&target, data)
                .await
                .map_err(|error| fsx::fs_error(&error, "open", &target))?,
            Flavor::Plugin => {
                let mode = match read_octal(header, 100, 8) {
                    Some(mode) if mode > 0 => (mode & 0o777) as u32,
                    _ => 0o644,
                };
                fs::write(&target, data)
                    .await
                    .map_err(|error| fsx::fs_error(&error, "open", &target))?;
                // 新建文件受 umask 影响；plugin 脚本的可执行位按归档头恢复。
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))
                        .await
                        .map_err(|error| fsx::fs_error(&error, "chmod", &target))?;
                }
                #[cfg(not(unix))]
                let _ = mode;
            }
        }
        extracted = next;
    }
    Ok(())
}

trait Renamed {
    fn renamed(self, name: &str) -> Self;
}

impl Renamed for RpcError {
    fn renamed(mut self, name: &str) -> Self {
        self.name = name.to_owned();
        self
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trips_and_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("skill");
        std::fs::create_dir_all(source.join("nested")).unwrap();
        std::fs::write(source.join("SKILL.md"), "---\nname: s\n---\n").unwrap();
        std::fs::write(source.join("nested/a.txt"), "abc").unwrap();
        let archive = create(
            Flavor::Skill,
            vec![Entry::Source {
                source_path: source.display().to_string(),
                archive_path: "group/skill".into(),
            }],
        )
        .await
        .unwrap();
        let out = dir.path().join("out");
        extract(
            Flavor::Skill,
            &archive,
            &out.display().to_string(),
            Some(1 << 20),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(out.join("group/skill/nested/a.txt")).unwrap(),
            "abc"
        );
        let limited = extract(Flavor::Skill, &archive, &out.display().to_string(), Some(2))
            .await
            .unwrap_err();
        assert_eq!(limited.code_str(), Some(SKILL_SIZE_LIMIT_CODE));
        assert!(normalize_relative("../x", "unsafe").is_err());
        assert!(normalize_relative("a\\b", "unsafe").is_err());
        assert!(normalize_relative("C:x", "unsafe").is_err());
        assert_eq!(normalize_relative("a/b/", "unsafe").unwrap(), "a/b");
    }
}
