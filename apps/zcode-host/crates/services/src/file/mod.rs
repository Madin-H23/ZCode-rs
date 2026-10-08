//! `file` channel（packages/services/src/file/fileService.ts）。
//!
//! 与 TS 一样不做路径归一化或 workspace 边界检查：Renderer 给的绝对路径原样使用。
//! fs 错误保持 Node 的 `code` 与消息前缀，Renderer 依赖它们区分“文件不存在 / 过大”。
mod collate;
mod errors;
pub mod ignore_rules;
mod index;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use zcode_host_kit::{Reply, RpcError, ServiceBuilder};

use collate::locale_compare;
use errors::{node_error, plain_error};
use index::{DISPLAY_CAP, IndexCache};

const KIB: f64 = 1024.0;
const MIB: f64 = 1024.0 * 1024.0;

/// fileService.ts:55-78：有限值按 trunc 取整，否则用默认值；再夹到 [1, max]。
fn clamp_length(requested: Option<f64>, default: f64, max: f64) -> u64 {
    let value = requested
        .filter(|value| value.is_finite())
        .map(f64::trunc)
        .unwrap_or(default);
    value.clamp(1.0, max) as u64
}

fn offset_of(requested: Option<f64>) -> u64 {
    requested
        .filter(|value| value.is_finite())
        .map(|value| value.trunc().max(0.0))
        .unwrap_or(0.0) as u64
}

/// isProbablyBinary：出现 NUL，或控制字节（1-8、14-31、127）占比超过 30%。
fn is_probably_binary(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    if bytes.contains(&0) {
        return true;
    }
    let suspicious = bytes
        .iter()
        .filter(|&&byte| matches!(byte, 1..=8 | 14..=31 | 127))
        .count();
    suspicious as f64 / bytes.len() as f64 > 0.3
}

fn media_type(path: &str) -> &'static str {
    let lower = path.replace('\\', "/").to_lowercase();
    let extension = lower
        .rsplit('/')
        .next()
        .and_then(|name| name.rfind('.').filter(|i| *i > 0).map(|i| &name[i..]))
        .unwrap_or("");
    let image = match extension {
        ".apng" => Some("image/apng"),
        ".avif" => Some("image/avif"),
        ".bmp" => Some("image/bmp"),
        ".gif" => Some("image/gif"),
        ".ico" => Some("image/x-icon"),
        ".jpeg" | ".jpg" => Some("image/jpeg"),
        ".png" => Some("image/png"),
        ".svg" => Some("image/svg+xml"),
        ".webp" => Some("image/webp"),
        _ => None,
    };
    if let Some(image) = image {
        return image;
    }
    const MEDIA: &[(&str, &str)] = &[
        (".mp4", "video/mp4"),
        (".mov", "video/quicktime"),
        (".webm", "video/webm"),
        (".m4v", "video/x-m4v"),
        (".mp3", "audio/mpeg"),
        (".wav", "audio/wav"),
        (".m4a", "audio/mp4"),
        (".ogg", "audio/ogg"),
        (".opus", "audio/opus"),
        (".flac", "audio/flac"),
        (".weba", "audio/webm"),
    ];
    MEDIA
        .iter()
        .find(|(ext, _)| lower.ends_with(ext))
        .map(|(_, media)| *media)
        .unwrap_or("application/octet-stream")
}

async fn stat_file(path: &Path) -> Result<std::fs::Metadata, RpcError> {
    let meta = fs::metadata(path)
        .await
        .map_err(|error| node_error(&error, "stat", path))?;
    if !meta.is_file() {
        return Err(plain_error(format!(
            "Path is not a file: {}",
            path.display()
        )));
    }
    Ok(meta)
}

async fn read_at(path: &Path, offset: u64, length: u64) -> Result<Vec<u8>, RpcError> {
    let mut file = fs::File::open(path)
        .await
        .map_err(|error| node_error(&error, "open", path))?;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|error| node_error(&error, "read", path))?;
    let mut buffer = Vec::with_capacity(length as usize);
    (&mut file)
        .take(length)
        .read_to_end(&mut buffer)
        .await
        .map_err(|error| node_error(&error, "read", path))?;
    Ok(buffer)
}

fn mtime_ms(meta: &std::fs::Metadata) -> Option<f64> {
    meta.modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs_f64() * 1000.0)
}

/// 目录项类型：真目录为 directory；符号链接跟随后判定（断链为 file）；其余为 file。
async fn entry_type(path: &Path, file_type: std::fs::FileType) -> &'static str {
    if file_type.is_dir() {
        return "directory";
    }
    if file_type.is_symlink() && fs::metadata(path).await.is_ok_and(|meta| meta.is_dir()) {
        return "directory";
    }
    "file"
}

async fn ensure_directory(path: &Path) -> Result<bool, RpcError> {
    let existed = fs::metadata(path).await.ok();
    if let Some(meta) = &existed
        && !meta.is_dir()
    {
        return Err(plain_error(format!(
            "Workspace path is not a directory: {}",
            path.display()
        )));
    }
    if existed.is_none() {
        fs::create_dir_all(path)
            .await
            .map_err(|error| node_error(&error, "mkdir", path))?;
    }
    let meta = fs::metadata(path)
        .await
        .map_err(|error| node_error(&error, "stat", path))?;
    if !meta.is_dir() {
        return Err(plain_error(format!(
            "Workspace path is not a directory: {}",
            path.display()
        )));
    }
    Ok(existed.is_none())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PathParams {
    path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReaddirParams {
    path: String,
    include_hidden: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RangeParams {
    path: String,
    offset: Option<f64>,
    length: Option<f64>,
    max_bytes: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchParams {
    root_path: String,
    workspace_identity: Option<String>,
    query: Value,
    limit: Option<Value>,
    refresh: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListRangeParams {
    root_path: String,
    offset: Option<f64>,
    length: Option<f64>,
}

pub struct FileServiceConfig {
    /// `<dataBaseDir>/.zcode/workspace/default`
    pub conversation_workspace: PathBuf,
    /// `os.homedir()`
    pub user_home: PathBuf,
}

macro_rules! handler {
    ($state:ident, |$config:ident, $args:ident| $body:expr) => {{
        let state = $state.clone();
        move |_, $args: zcode_host_kit::Args| {
            let $config = state.clone();
            async move { $body }
        }
    }};
}

struct State {
    config: FileServiceConfig,
    index: IndexCache,
}

pub fn service(config: FileServiceConfig) -> ServiceBuilder {
    let state = Arc::new(State {
        config,
        index: IndexCache::default(),
    });
    ServiceBuilder::new("file")
        .method(
            "ensureConversationWorkspace",
            handler!(state, |state, _args| {
                let path = &state.config.conversation_workspace;
                let created = ensure_directory(path).await?;
                Ok(Reply::Value(json!({
                    "path": path.display().to_string(),
                    "created": created,
                    "workspacePurpose": "conversation",
                })))
            }),
        )
        .method(
            "createDefaultWorkspace",
            handler!(state, |state, _args| {
                let path = state.config.user_home.join("ZCodeProject");
                ensure_directory(&path).await?;
                Ok(Reply::Value(json!({"path": path.display().to_string()})))
            }),
        )
        .method(
            "createScratchWorkspace",
            handler!(state, |state, args| {
                let params: Value = args.get(0, "params")?;
                let name = params["name"].as_str().unwrap_or("").trim().to_owned();
                if name.is_empty() {
                    return Err(plain_error("Workspace name is required."));
                }
                if name.contains(['/', '\\']) {
                    return Err(plain_error("Workspace name cannot contain path separators."));
                }
                let path = state.config.user_home.join("ZCodeProject").join(name);
                ensure_directory(&path).await?;
                Ok(Reply::Value(json!({"path": path.display().to_string()})))
            }),
        )
        .method(
            "readdir",
            handler!(state, |_state, args| {
                let params: ReaddirParams = args.get(0, "params")?;
                let dir = PathBuf::from(&params.path);
                let mut reader = fs::read_dir(&dir)
                    .await
                    .map_err(|error| node_error(&error, "scandir", &dir))?;
                let mut entries: Vec<(bool, String, Value)> = Vec::new();
                while let Some(item) = reader
                    .next_entry()
                    .await
                    .map_err(|error| node_error(&error, "scandir", &dir))?
                {
                    let name = item.file_name().to_string_lossy().into_owned();
                    if params.include_hidden != Some(true) && name.starts_with('.') {
                        continue;
                    }
                    let path = dir.join(&name);
                    let Ok(file_type) = item.file_type().await else {
                        continue;
                    };
                    let kind = entry_type(&path, file_type).await;
                    entries.push((
                        kind == "directory",
                        name.clone(),
                        json!({
                            "name": name,
                            "path": path.display().to_string(),
                            "type": kind,
                            "isSymbolicLink": file_type.is_symlink(),
                        }),
                    ));
                }
                entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| locale_compare(&a.1, &b.1)));
                Ok(Reply::Value(Value::Array(
                    entries.into_iter().map(|(_, _, entry)| entry).collect(),
                )))
            }),
        )
        .method(
            "stat",
            handler!(state, |_state, args| {
                let params: PathParams = args.get(0, "params")?;
                let path = PathBuf::from(&params.path);
                let meta = fs::metadata(&path)
                    .await
                    .map_err(|error| node_error(&error, "stat", &path))?;
                Ok(Reply::Value(if meta.is_dir() {
                    json!({"path": params.path, "type": "directory"})
                } else {
                    json!({"path": params.path, "type": "file", "size": meta.len(), "mtimeMs": mtime_ms(&meta)})
                }))
            }),
        )
        .method(
            "checkFilesExist",
            handler!(state, |_state, args| {
                let params: Value = args.get(0, "params")?;
                let paths: Vec<String> = params["paths"]
                    .as_array()
                    .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
                    .unwrap_or_default();
                if paths.len() > 15 {
                    return Err(plain_error("File existence check supports at most 15 paths."));
                }
                let mut result = Vec::with_capacity(paths.len());
                for path in paths {
                    let exists = fs::metadata(&path).await.is_ok_and(|meta| meta.is_file());
                    result.push(json!({"path": path, "exists": exists}));
                }
                Ok(Reply::Value(Value::Array(result)))
            }),
        )
        .method(
            "resolvePath",
            handler!(state, |_state, args| {
                let params: PathParams = args.get(0, "params")?;
                let path = PathBuf::from(&params.path);
                let resolved = fs::canonicalize(&path)
                    .await
                    .map_err(|error| node_error(&error, "realpath", &path))?;
                Ok(Reply::Value(Value::String(resolved.display().to_string())))
            }),
        )
        .method(
            "readTextFile",
            handler!(state, |_state, args| {
                let params: RangeParams = args.get(0, "params")?;
                let path = PathBuf::from(&params.path);
                let meta = stat_file(&path).await?;
                let size = meta.len();
                let offset = offset_of(params.offset);
                if offset >= size {
                    return Ok(Reply::Value(json!({
                        "path": params.path, "content": "", "offset": offset, "bytesRead": 0,
                        "totalBytes": size, "truncated": false, "isBinary": false,
                    })));
                }
                let length = clamp_length(params.length, 128.0 * KIB, 256.0 * KIB).min(size - offset);
                let bytes = read_at(&path, offset, length).await?;
                let binary = is_probably_binary(&bytes);
                let content = if binary {
                    String::new()
                } else {
                    String::from_utf8_lossy(&bytes).into_owned()
                };
                let read = bytes.len() as u64;
                Ok(Reply::Value(json!({
                    "path": params.path, "content": content, "offset": offset, "bytesRead": read,
                    "totalBytes": size, "truncated": offset + read < size, "isBinary": binary,
                })))
            }),
        )
        .method(
            "readFileRange",
            handler!(state, |_state, args| {
                let params: RangeParams = args.get(0, "params")?;
                let path = PathBuf::from(&params.path);
                let size = stat_file(&path).await?.len();
                let offset = offset_of(params.offset);
                if offset >= size {
                    return Ok(Reply::Bytes(Vec::new()));
                }
                let length = clamp_length(params.length, 256.0 * KIB, MIB).min(size - offset);
                Ok(Reply::Bytes(read_at(&path, offset, length).await?))
            }),
        )
        .method(
            "readMediaPreview",
            handler!(state, |_state, args| {
                let params: RangeParams = args.get(0, "params")?;
                read_preview(&params, 4.0 * MIB, 8.0 * MIB, true).await
            }),
        )
        .method(
            "readBinaryPreview",
            handler!(state, |_state, args| {
                let params: RangeParams = args.get(0, "params")?;
                read_preview(&params, 25.0 * MIB, 25.0 * MIB, false).await
            }),
        )
        .method(
            "searchWorkspaceFiles",
            handler!(state, |state, args| {
                let params: SearchParams = args.get(0, "params")?;
                let limit = match &params.limit {
                    None | Some(Value::Null) => Some(DISPLAY_CAP as f64),
                    Some(value) => value.as_f64().filter(|limit| limit.is_finite()),
                };
                let (Some(limit), Some(query)) = (limit, params.query.as_str()) else {
                    return Err(plain_error("Invalid workspace file search query or limit"));
                };
                let limit = (limit.trunc().max(0.0) as usize).min(DISPLAY_CAP);
                if limit == 0 {
                    return Ok(Reply::Value(json!([])));
                }
                let key = params
                    .workspace_identity
                    .as_deref()
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .unwrap_or(&params.root_path)
                    .to_owned();
                let root = PathBuf::from(&params.root_path);
                let index = state
                    .index
                    .get(&root, &key, params.refresh == Some(true))
                    .await
                    .map_err(|error| node_error(&error, "scandir", &root))?;
                Ok(Reply::Value(Value::Array(index.search(&params.root_path, query, limit))))
            }),
        )
        .method(
            "listWorkspaceFilesLength",
            handler!(state, |state, args| {
                let params: ListRangeParams = args.get(0, "params")?;
                let root = PathBuf::from(&params.root_path);
                let index = state
                    .index
                    .get(&root, &params.root_path, false)
                    .await
                    .map_err(|error| node_error(&error, "scandir", &root))?;
                Ok(Reply::Value(json!(index.packed_len())))
            }),
        )
        .method(
            "listWorkspaceFilesRange",
            handler!(state, |state, args| {
                let params: ListRangeParams = args.get(0, "params")?;
                let root = PathBuf::from(&params.root_path);
                let index = state
                    .index
                    .get(&root, &params.root_path, false)
                    .await
                    .map_err(|error| node_error(&error, "scandir", &root))?;
                let offset = offset_of(params.offset) as usize;
                let length = offset_of(params.length) as usize;
                Ok(Reply::Value(Value::String(index.packed_range(offset, length))))
            }),
        )
        .method(
            "readWorkspaceFileSearchIgnore",
            handler!(state, |_state, args| {
                let params: ListRangeParams = args.get(0, "params")?;
                let root = PathBuf::from(&params.root_path);
                let (content, source) = ignore_rules::read_for_settings(&root)
                    .await
                    .map_err(|error| node_error(&error, "open", &root))?;
                Ok(Reply::Value(json!({"content": content, "source": source})))
            }),
        )
        .method(
            "applyWorkspaceFileSearchIgnoreTransform",
            handler!(state, |_state, args| {
                let params: Value = args.get(0, "params")?;
                let root = PathBuf::from(params["rootPath"].as_str().unwrap_or_default());
                let transform = params["transform"].as_str().unwrap_or("reset-defaults");
                let content = ignore_rules::transform(&root, transform).await;
                Ok(Reply::Value(json!({"content": content})))
            }),
        )
        .method(
            "writeWorkspaceFileSearchIgnore",
            handler!(state, |_state, args| {
                let params: Value = args.get(0, "params")?;
                let root = PathBuf::from(params["rootPath"].as_str().unwrap_or_default());
                let content = params["content"].as_str().unwrap_or_default();
                let target = root.join(ignore_rules::FILE_NAME);
                ignore_rules::atomic_write(&target, content)
                    .await
                    .map_err(|error| node_error(&error, "open", &target))?;
                Ok(Reply::Undefined)
            }),
        )
}

async fn read_preview(
    params: &RangeParams,
    default: f64,
    max: f64,
    with_media_type: bool,
) -> Result<Reply, RpcError> {
    let path = PathBuf::from(&params.path);
    let size = stat_file(&path).await?.len();
    let limit = clamp_length(params.max_bytes, default, max);
    if size > limit {
        return Err(plain_error(format!(
            "File is too large to preview: {}",
            params.path
        )));
    }
    let bytes = fs::read(&path)
        .await
        .map_err(|error| node_error(&error, "open", &path))?;
    let encoded = zcode_host_kit::base64_encode(&bytes);
    Ok(Reply::Value(if with_media_type {
        json!({"path": params.path, "mediaType": media_type(&params.path), "dataBase64": encoded, "totalBytes": size})
    } else {
        json!({"path": params.path, "dataBase64": encoded, "totalBytes": size})
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_follow_typescript_rules() {
        assert_eq!(clamp_length(None, 10.0, 20.0), 10);
        assert_eq!(clamp_length(Some(0.0), 10.0, 20.0), 1);
        assert_eq!(clamp_length(Some(99.9), 10.0, 20.0), 20);
        assert_eq!(clamp_length(Some(f64::NAN), 10.0, 20.0), 10);
        assert_eq!(offset_of(Some(-5.0)), 0);
        assert!(is_probably_binary(b"a\0b"));
        assert!(!is_probably_binary(b"hello\n\tworld\r\x0c"));
        assert!(is_probably_binary(&[1, 2, 3, b'a']));
        assert_eq!(media_type("/x/A.PNG"), "image/png");
        assert_eq!(media_type("/x/clip.mov"), "video/quicktime");
        assert_eq!(media_type("/x/doc.pdf"), "application/octet-stream");
    }

    #[test]
    fn node_errors_keep_code_and_prefix() {
        let error = std::io::Error::from(std::io::ErrorKind::NotFound);
        let rpc = node_error(&error, "stat", Path::new("/missing"));
        assert_eq!(rpc.code_str(), Some("ENOENT"));
        assert!(
            rpc.message
                .starts_with("ENOENT: no such file or directory, stat '/missing'")
        );
    }
}
