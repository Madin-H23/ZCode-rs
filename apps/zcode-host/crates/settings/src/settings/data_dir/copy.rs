//! `copyDataDirectory`（packages/services/src/paths.ts）：
//! `fs.cp(<old>/.zcode/v2, <new>/.zcode/v2, {recursive: true, force: false, filter})`。
//!
//! 按 Node 24 `lib/internal/fs/cp/cp.js` 逐步移植（checkPaths → checkParentPaths → checkParentDir →
//! getStatsForCopy → onDir / onFile），顺序执行、遇错即停、不回滚；错误的 name / code / message 与
//! Node 相同。见 docs/specs/rust-host-setting.md §2.4。
use std::fs::{FileType, Metadata};
use std::io::{self, ErrorKind};
use std::path::{MAIN_SEPARATOR, Path};

use futures_util::future::BoxFuture;
use tokio::fs;
use zcode_host_kit::RpcError;

use crate::support::path as node_path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyError {
    /// `Error`（系统调用失败）或 `SystemError`（cp 自身的 `ERR_FS_CP_*`）。
    pub name: &'static str,
    pub code: String,
    pub message: String,
}

impl From<CopyError> for RpcError {
    fn from(error: CopyError) -> Self {
        RpcError::new(error.name, error.message).with_code(error.code)
    }
}

type Result<T> = std::result::Result<T, CopyError>;

/// libuv 的错误名与描述（`uv_err_name` / `uv_strerror`）。
fn uv_code(error: &io::Error) -> (&'static str, &'static str) {
    #[cfg(unix)]
    if let Some(raw) = error.raw_os_error() {
        let known = match raw {
            libc::ENOENT => Some(("ENOENT", "no such file or directory")),
            libc::EACCES => Some(("EACCES", "permission denied")),
            libc::EPERM => Some(("EPERM", "operation not permitted")),
            libc::ENOTDIR => Some(("ENOTDIR", "not a directory")),
            libc::EISDIR => Some(("EISDIR", "illegal operation on a directory")),
            libc::EEXIST => Some(("EEXIST", "file already exists")),
            libc::ELOOP => Some(("ELOOP", "too many symbolic links encountered")),
            libc::ENAMETOOLONG => Some(("ENAMETOOLONG", "name too long")),
            libc::ENOSPC => Some(("ENOSPC", "no space left on device")),
            libc::EROFS => Some(("EROFS", "read-only file system")),
            libc::EBUSY => Some(("EBUSY", "resource busy or locked")),
            libc::EMFILE => Some(("EMFILE", "too many open files")),
            libc::EXDEV => Some(("EXDEV", "cross-device link not permitted")),
            libc::EIO => Some(("EIO", "i/o error")),
            _ => None,
        };
        if let Some(code) = known {
            return code;
        }
    }
    if error.kind() == ErrorKind::NotFound {
        return ("ENOENT", "no such file or directory");
    }
    crate::support::node_error::code_of(error).unwrap_or(("EIO", "i/o error"))
}

/// 系统调用失败：`ENOENT: no such file or directory, lstat '/x'`。
fn sys_error(error: &io::Error, syscall: &str, path: &str) -> CopyError {
    let (code, description) = uv_code(error);
    CopyError {
        name: "Error",
        code: code.to_owned(),
        message: format!("{code}: {description}, {syscall} '{path}'"),
    }
}

/// `copyfile` 带两个路径：`EACCES: permission denied, copyfile '/a' -> '/b'`。
fn copyfile_error(error: &io::Error, src: &str, dest: &str) -> CopyError {
    let (code, description) = uv_code(error);
    CopyError {
        name: "Error",
        code: code.to_owned(),
        message: format!("{code}: {description}, copyfile '{src}' -> '{dest}'"),
    }
}

#[derive(Clone, Copy)]
enum CpFault {
    Einval,
    DirToNonDir,
    NonDirToDir,
    Socket,
    FifoPipe,
    Unknown,
}

/// cp 自身的 SystemError：`<标题>: cp returned <errno 名> (<细节>) <路径>`。
fn cp_error(fault: CpFault, detail: String, path: &str) -> CopyError {
    let (code, title, errno) = match fault {
        CpFault::Einval => ("ERR_FS_CP_EINVAL", "Invalid src or dest", "EINVAL"),
        CpFault::DirToNonDir => (
            "ERR_FS_CP_DIR_TO_NON_DIR",
            "Cannot overwrite non-directory with directory",
            "EISDIR",
        ),
        CpFault::NonDirToDir => (
            "ERR_FS_CP_NON_DIR_TO_DIR",
            "Cannot overwrite directory with non-directory",
            "ENOTDIR",
        ),
        CpFault::Socket => ("ERR_FS_CP_SOCKET", "Cannot copy a socket file", "EINVAL"),
        CpFault::FifoPipe => ("ERR_FS_CP_FIFO_PIPE", "Cannot copy a FIFO pipe", "EINVAL"),
        CpFault::Unknown => (
            "ERR_FS_CP_UNKNOWN",
            "Cannot copy an unknown file type",
            "EINVAL",
        ),
    };
    CopyError {
        name: "SystemError",
        code: code.to_owned(),
        message: format!("{title}: cp returned {errno} ({detail}) {path}"),
    }
}

/// `path.join(base, ".zcode", "v2")`：词法规范化，不解析软链。
pub fn data_dir_of(base: &str) -> String {
    node_path::resolve(base, &[".zcode", "v2"])
}

/// `path.dirname`。
fn dirname(path: &str) -> String {
    match Path::new(path).parent() {
        None => path.to_owned(),
        Some(parent) if parent.as_os_str().is_empty() => ".".to_owned(),
        Some(parent) => parent.to_string_lossy().into_owned(),
    }
}

fn child(parent: &str, name: &std::ffi::OsStr) -> String {
    Path::new(parent).join(name).to_string_lossy().into_owned()
}

/// `filter`：bootstrap 文件（`setting.json` 与原子写的中间态）和软链都不迁移。
async fn keep(source: &str) -> bool {
    let name = Path::new(source)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    // setting.json.lock 与 setting.json.*.tmp 由原子写短暂创建 / 删除，复制时扫到已消失的锁会 ENOENT；
    // 它们都属于 bootstrap 写入中间态，不能迁移到新数据根（同 Node）。
    if name == "setting.json" || name.starts_with("setting.json.") {
        return false;
    }
    // Windows 非提权环境下复制软链会 EPERM；lstat 失败时放行，让后续步骤自行处理。
    !matches!(fs::symlink_metadata(source).await, Ok(meta) if meta.file_type().is_symlink())
}

/// `areIdentical`：同一 inode（`ino`、`dev` 都非零时才比较）。
#[cfg(unix)]
async fn identical(_left_path: &str, left: &Metadata, _right_path: &str, right: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    right.ino() != 0 && right.dev() != 0 && left.ino() == right.ino() && left.dev() == right.dev()
}

/// Windows：标准库没有稳定的 file index，比较规范化后的路径；lstat 语义下软链自成一个对象。
#[cfg(not(unix))]
async fn identical(left_path: &str, left: &Metadata, right_path: &str, right: &Metadata) -> bool {
    if left.file_type().is_symlink() || right.file_type().is_symlink() {
        return false;
    }
    match (
        fs::canonicalize(left_path).await,
        fs::canonicalize(right_path).await,
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

#[cfg(unix)]
fn special_kind(file_type: FileType) -> (bool, bool, bool) {
    use std::os::unix::fs::FileTypeExt;
    (
        file_type.is_char_device() || file_type.is_block_device(),
        file_type.is_socket(),
        file_type.is_fifo(),
    )
}

#[cfg(not(unix))]
fn special_kind(_file_type: FileType) -> (bool, bool, bool) {
    (false, false, false)
}

/// `setDestMode`：chmod 成源的 mode（Windows 上只影响只读位，同 Node）。
async fn set_dest_mode(dest: &str, src_stat: &Metadata) -> Result<()> {
    fs::set_permissions(dest, src_stat.permissions())
        .await
        .map_err(|error| sys_error(&error, "chmod", dest))
}

struct Copier {
    /// `process.cwd()`：`path.resolve` 处理相对路径时用。
    cwd: String,
}

impl Copier {
    fn resolve(&self, path: &str) -> String {
        node_path::resolve(&self.cwd, &[path])
    }

    /// `isSrcSubdir`：按路径段判断 dest 是否位于 src 之下（含相等）。
    fn is_src_subdir(&self, src: &str, dest: &str) -> bool {
        let split = |path: &str| -> Vec<String> {
            self.resolve(path)
                .split(MAIN_SEPARATOR)
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let (src, dest) = (split(src), split(dest));
        src.iter()
            .enumerate()
            .all(|(index, part)| dest.get(index) == Some(part))
    }

    /// `cpFn`。
    async fn cp(&self, src: &str, dest: &str) -> Result<()> {
        let Some((src_stat, dest_stat)) = self.check_paths(src, dest).await? else {
            return Ok(());
        };
        self.check_parent_paths(src, &src_stat, dest).await?;
        self.check_parent_dir(dest_stat, src, dest).await
    }

    /// `checkPaths`：filter 跳过时返回 None。
    async fn check_paths(
        &self,
        src: &str,
        dest: &str,
    ) -> Result<Option<(Metadata, Option<Metadata>)>> {
        if !keep(src).await {
            return Ok(None);
        }
        let src_stat = fs::symlink_metadata(src)
            .await
            .map_err(|error| sys_error(&error, "lstat", src))?;
        let dest_stat = match fs::symlink_metadata(dest).await {
            Ok(stat) => Some(stat),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => return Err(sys_error(&error, "lstat", dest)),
        };
        if let Some(dest_stat) = &dest_stat {
            if identical(src, &src_stat, dest, dest_stat).await {
                return Err(cp_error(
                    CpFault::Einval,
                    "src and dest cannot be the same".to_owned(),
                    dest,
                ));
            }
            if src_stat.is_dir() && !dest_stat.is_dir() {
                return Err(cp_error(
                    CpFault::DirToNonDir,
                    format!("cannot overwrite non-directory {dest} with directory {src}"),
                    dest,
                ));
            }
            if !src_stat.is_dir() && dest_stat.is_dir() {
                return Err(cp_error(
                    CpFault::NonDirToDir,
                    format!("cannot overwrite directory {dest} with non-directory {src}"),
                    dest,
                ));
            }
        }
        if src_stat.is_dir() && self.is_src_subdir(src, dest) {
            return Err(cp_error(
                CpFault::Einval,
                format!("cannot copy {src} to a subdirectory of self {dest}"),
                dest,
            ));
        }
        Ok(Some((src_stat, dest_stat)))
    }

    /// `checkParentPaths`：dest 的各级已存在父目录（stat，跟随软链）不能就是 src。
    async fn check_parent_paths(&self, src: &str, src_stat: &Metadata, dest: &str) -> Result<()> {
        let src_parent = self.resolve(&dirname(src));
        let mut dest = dest.to_owned();
        loop {
            let dest_parent = self.resolve(&dirname(&dest));
            if dest_parent == src_parent || Path::new(&dest_parent).parent().is_none() {
                return Ok(());
            }
            let dest_stat = match fs::metadata(&dest_parent).await {
                Ok(stat) => stat,
                Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(sys_error(&error, "stat", &dest_parent)),
            };
            if identical(src, src_stat, &dest_parent, &dest_stat).await {
                return Err(cp_error(
                    CpFault::Einval,
                    format!("cannot copy {src} to a subdirectory of self {dest}"),
                    &dest,
                ));
            }
            dest = dest_parent;
        }
    }

    /// `checkParentDir`：dest 的父目录不存在时 `mkdir -p`。
    async fn check_parent_dir(
        &self,
        dest_stat: Option<Metadata>,
        src: &str,
        dest: &str,
    ) -> Result<()> {
        let dest_parent = dirname(dest);
        match fs::metadata(&dest_parent).await {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {
                fs::create_dir_all(&dest_parent)
                    .await
                    .map_err(|error| sys_error(&error, "mkdir", &dest_parent))?;
            }
            Err(error) => return Err(sys_error(&error, "stat", &dest_parent)),
        }
        self.get_stats_for_copy(dest_stat, src.to_owned(), dest.to_owned())
            .await
    }

    /// `getStatsForCopy`：按源的类型分派（目录递归经它进入，所以返回装箱的 future）。
    fn get_stats_for_copy(
        &self,
        dest_stat: Option<Metadata>,
        src: String,
        dest: String,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let src_stat = fs::symlink_metadata(&src)
                .await
                .map_err(|error| sys_error(&error, "lstat", &src))?;
            let file_type = src_stat.file_type();
            let (device, socket, fifo) = special_kind(file_type);
            if file_type.is_dir() {
                return self.on_dir(&src_stat, dest_stat, &src, &dest).await;
            }
            if file_type.is_file() || device {
                return self.on_file(&src_stat, dest_stat, &src, &dest).await;
            }
            if file_type.is_symlink() {
                // filter 已跳过所有软链；放行后才变成软链的竞态条目同样跳过（见规格偏差表）。
                return Ok(());
            }
            if socket {
                return Err(cp_error(
                    CpFault::Socket,
                    format!("cannot copy a socket file: {dest}"),
                    &dest,
                ));
            }
            if fifo {
                return Err(cp_error(
                    CpFault::FifoPipe,
                    format!("cannot copy a FIFO pipe: {dest}"),
                    &dest,
                ));
            }
            Err(cp_error(
                CpFault::Unknown,
                format!("cannot copy an unknown file type: {dest}"),
                &dest,
            ))
        })
    }

    /// `onDir`：新建的目录在子项复制完后才 chmod（只读目录先放子项再收紧权限）。
    async fn on_dir(
        &self,
        src_stat: &Metadata,
        dest_stat: Option<Metadata>,
        src: &str,
        dest: &str,
    ) -> Result<()> {
        if dest_stat.is_some() {
            return self.copy_dir(src, dest).await;
        }
        fs::create_dir(dest)
            .await
            .map_err(|error| sys_error(&error, "mkdir", dest))?;
        self.copy_dir(src, dest).await?;
        set_dest_mode(dest, src_stat).await
    }

    /// `copyDir`：逐项 checkPaths（含 filter 与类型冲突），再按类型复制。
    async fn copy_dir(&self, src: &str, dest: &str) -> Result<()> {
        let mut entries = fs::read_dir(src)
            .await
            .map_err(|error| sys_error(&error, "opendir", src))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|error| sys_error(&error, "readdir", src))?
        {
            let name = entry.file_name();
            let src_item = child(src, &name);
            let dest_item = child(dest, &name);
            let Some((_, dest_stat)) = self.check_paths(&src_item, &dest_item).await? else {
                continue;
            };
            self.get_stats_for_copy(dest_stat, src_item, dest_item)
                .await?;
        }
        Ok(())
    }

    /// `onFile`：`force: false` 且 `errorOnExist: false`，已存在的目标静默跳过，不覆盖。
    async fn on_file(
        &self,
        src_stat: &Metadata,
        dest_stat: Option<Metadata>,
        src: &str,
        dest: &str,
    ) -> Result<()> {
        if dest_stat.is_some() {
            return Ok(());
        }
        fs::copy(src, dest)
            .await
            .map_err(|error| copyfile_error(&error, src, dest))?;
        set_dest_mode(dest, src_stat).await
    }
}

/// `copyDataDirectory(oldBaseDir, newBaseDir)`。
pub async fn copy_data_directory(old_base: &str, new_base: &str) -> Result<()> {
    #[expect(clippy::disallowed_methods, reason = "存量，U3 改为显式传入")]
    let cwd = std::env::current_dir()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_default();
    Copier { cwd }
        .cp(&data_dir_of(old_base), &data_dir_of(new_base))
        .await
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[tokio::test]
    async fn read_only_directory_is_tightened_after_its_children() {
        let root = tempfile::tempdir().unwrap();
        let src = root.path().join("old/.zcode/v2/locked");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), "a").unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o555)).unwrap();

        let old = root.path().join("old");
        let new = root.path().join("new");
        copy_data_directory(old.to_str().unwrap(), new.to_str().unwrap())
            .await
            .unwrap();

        let dest = new.join(".zcode/v2/locked");
        assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), "a");
        assert_eq!(mode(&dest), 0o555);
        for dir in [&src, &dest] {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[tokio::test]
    async fn existing_files_are_kept_and_bootstrap_files_skipped() {
        let root = tempfile::tempdir().unwrap();
        let v2 = root.path().join("old/.zcode/v2");
        std::fs::create_dir_all(v2.join("setting.json.lock")).unwrap();
        std::fs::write(v2.join("setting.json"), "{}").unwrap();
        std::fs::write(v2.join("data.json"), "new").unwrap();
        let target = root.path().join("new/.zcode/v2");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("data.json"), "old").unwrap();

        copy_data_directory(
            root.path().join("old").to_str().unwrap(),
            root.path().join("new").to_str().unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(target.join("data.json")).unwrap(),
            "old"
        );
        assert!(!target.join("setting.json").exists());
        assert!(!target.join("setting.json.lock").exists());
    }

    #[test]
    fn errors_keep_node_shape() {
        let error: RpcError = sys_error(
            &io::Error::from_raw_os_error(libc::ENOENT),
            "lstat",
            "/x/.zcode/v2",
        )
        .into();
        assert_eq!(error.name, "Error");
        assert_eq!(error.code_str(), Some("ENOENT"));
        assert_eq!(
            error.message,
            "ENOENT: no such file or directory, lstat '/x/.zcode/v2'"
        );
        let error: RpcError = cp_error(
            CpFault::Einval,
            "src and dest cannot be the same".to_owned(),
            "/x",
        )
        .into();
        assert_eq!(error.name, "SystemError");
        assert_eq!(error.code_str(), Some("ERR_FS_CP_EINVAL"));
        assert_eq!(
            error.message,
            "Invalid src or dest: cp returned EINVAL (src and dest cannot be the same) /x"
        );
    }
}
