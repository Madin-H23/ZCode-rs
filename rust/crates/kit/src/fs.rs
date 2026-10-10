//! 原子写（docs/specs/rust-u0-foundation.md §3.2，缺陷 B6），以及轮询读文件前比元数据的
//! [`FileStamp`] / [`ChangeGate`]（docs/specs/rust-provider-config-poll.md）。
//!
//! 唯一的“临时文件 + rename”实现：同目录 `create_new` 临时文件 → 写入 → `sync_all` → rename
//! （目标被短暂占用时按调用方给的表重试）→ fsync 父目录（Unix，尽力而为）。任一步失败都删除
//! 临时文件；目标文件要么是旧内容，要么是完整的新内容。
//!
//! 持久性：Node 的 `atomicWriteText` / `atomicWritePrivateTextFile` 不 fsync 临时文件，也不 fsync
//! 父目录，断电后可能留下长度为 0 的目标文件或丢失这次 rename。这里两步都做，是有意的加强；
//! 文件内容、权限与临时文件命名仍与 Node 一致，Node 与 Rust 可以交替写同一个文件。

use std::ffi::{OsStr, OsString};
use std::hash::{BuildHasher, Hasher, RandomState};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod stamp;
pub use stamp::{ChangeGate, FileStamp};

/// 目标文件的权限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMode {
    /// 系统默认（`0o666 & !umask`）。
    Default,
    /// Unix 上恰好 `0o600`（不受 umask 影响）；Windows 上与 `Default` 相同。
    Private,
}

/// 临时文件名：`<name>.<pid>.<ms>.<rand>.tmp` 或在前面加 `.`。
///
/// Node `atomicWriteText` 用可见名，并清理同目录下过期的 `<name>.*.tmp`；凭据、配置等私密写入
/// 用隐藏名。同一个文件可能被 Node 与 Rust 交替写入，所以每个调用处必须沿用 Node 对应实现的命名，
/// 否则 Node 的清理规则会漏掉或误删。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TempName {
    Hidden,
    Visible,
}

/// 父目录不存在时的处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentDir {
    /// 调用方已经准备好目录（例如持锁前创建并设置了权限）；目录不存在时报错。
    Existing,
    /// 递归创建（`create_dir_all`，系统默认权限）。
    Create,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteOptions {
    pub mode: FileMode,
    pub temp_name: TempName,
    pub parent: ParentDir,
    /// rename 因目标被短暂占用而失败时（PermissionDenied、ResourceBusy，Windows 的共享 / 锁冲突），
    /// 依次等待这些时长后重试；用完后返回最后一次的错误。空切片表示不重试。
    pub rename_retry: &'static [Duration],
}

/// 用 `bytes` 原子地替换 `path`，见模块文档。
pub async fn atomic_write(path: &Path, bytes: &[u8], options: WriteOptions) -> io::Result<()> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("atomic write target has no file name: {}", path.display()),
        )
    })?;
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        Some(_) | None => PathBuf::from("."),
    };
    let temp = directory.join(temp_name(name, options.temp_name));
    // 用 std::fs 在一个 blocking 任务里完成：tokio File 的 write_all 只是排队，写入错误要等下一次
    // flush 才会返回，直接 sync_all 会把它吞掉。
    let staged = {
        let (directory, temp, bytes) = (directory.clone(), temp.clone(), bytes.to_vec());
        tokio::task::spawn_blocking(move || {
            stage(&directory, &temp, &bytes, options.mode, options.parent)
        })
        .await
        .map_err(io::Error::other)?
    };
    staged?;
    if let Err(error) = rename_with_retry(&temp, path, options.rename_retry).await {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(error);
    }
    sync_directory(directory).await;
    Ok(())
}

/// 创建并写好临时文件。打开失败时文件不是本次创建的（例如重名），不能删除；之后的失败删除它。
fn stage(
    directory: &Path,
    temp: &Path,
    bytes: &[u8],
    mode: FileMode,
    parent: ParentDir,
) -> io::Result<()> {
    match parent {
        ParentDir::Create => std::fs::create_dir_all(directory)?,
        ParentDir::Existing => {}
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if mode == FileMode::Private {
        // 从创建起就不对其他用户可读，避免写入前的窗口期。
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    }
    let mut file = options.open(temp)?;
    let written = restrict(&file, mode)
        .and_then(|()| file.write_all(bytes))
        .and_then(|()| file.sync_all());
    drop(file);
    if written.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    written
}

/// open 时给的 mode 会被 umask 削减；再显式设置一次，私密文件恰好是 0600。
#[cfg(unix)]
fn restrict(file: &std::fs::File, mode: FileMode) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    match mode {
        FileMode::Private => file.set_permissions(std::fs::Permissions::from_mode(0o600)),
        FileMode::Default => Ok(()),
    }
}

#[cfg(not(unix))]
fn restrict(_file: &std::fs::File, mode: FileMode) -> io::Result<()> {
    match mode {
        FileMode::Private | FileMode::Default => Ok(()),
    }
}

async fn rename_with_retry(temp: &Path, target: &Path, delays: &[Duration]) -> io::Result<()> {
    let mut delays = delays.iter();
    loop {
        let Err(error) = tokio::fs::rename(temp, target).await else {
            return Ok(());
        };
        match delays.next() {
            Some(delay) if retryable(&error) => tokio::time::sleep(*delay).await,
            Some(_) | None => return Err(error),
        }
    }
}

/// 与 Node 的 `EPERM` / `EACCES` / `EBUSY` 判定一致。libuv 把 Windows 的
/// `ERROR_SHARING_VIOLATION`（32）与 `ERROR_LOCK_VIOLATION`（33）映射为 `EBUSY`，std 不映射，单独判断。
fn retryable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::PermissionDenied | ErrorKind::ResourceBusy
    ) || (cfg!(windows) && matches!(error.raw_os_error(), Some(32 | 33)))
}

/// rename 之后 fsync 父目录，让目录项的变更落盘。尽力而为：rename 已经成功，有的文件系统不支持
/// 目录 fsync，失败不改变写入结果。Windows 没有对应操作。
async fn sync_directory(directory: PathBuf) {
    if cfg!(unix) {
        let _ =
            tokio::task::spawn_blocking(move || std::fs::File::open(directory)?.sync_all()).await;
    }
}

fn temp_name(name: &OsStr, style: TempName) -> OsString {
    // 进程内计数器保证同一进程的并发写互不冲突；RandomState 的随机种子区分 pid 复用后的进程。
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let mut temp = OsString::new();
    match style {
        TempName::Hidden => temp.push("."),
        TempName::Visible => {}
    }
    temp.push(name);
    temp.push(format!(
        ".{}.{millis}.{:016x}.tmp",
        std::process::id(),
        hasher.finish()
    ));
    temp
}

#[cfg(test)]
#[path = "fs_tests.rs"]
mod tests;
