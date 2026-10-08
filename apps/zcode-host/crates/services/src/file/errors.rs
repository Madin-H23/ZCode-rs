//! Node 风格的文件系统错误（Renderer 按 `code === "ENOENT"` 或消息里的
//! `no such file or directory` 判断“文件不存在”，见 PreviewPane.tsx:125-135）。
use std::io::ErrorKind;
use std::path::Path;

use zcode_host_kit::RpcError;

fn errno_parts(error: &std::io::Error) -> (&'static str, &'static str) {
    #[cfg(unix)]
    if let Some(code) = error.raw_os_error() {
        match code {
            libc::ENOENT => return ("ENOENT", "no such file or directory"),
            libc::EACCES => return ("EACCES", "permission denied"),
            libc::EPERM => return ("EPERM", "operation not permitted"),
            libc::ENOTDIR => return ("ENOTDIR", "not a directory"),
            libc::EISDIR => return ("EISDIR", "illegal operation on a directory"),
            libc::EEXIST => return ("EEXIST", "file already exists"),
            libc::ELOOP => return ("ELOOP", "too many symbolic links encountered"),
            libc::ENAMETOOLONG => return ("ENAMETOOLONG", "name too long"),
            _ => {}
        }
    }
    match error.kind() {
        ErrorKind::NotFound => ("ENOENT", "no such file or directory"),
        ErrorKind::PermissionDenied => ("EACCES", "permission denied"),
        ErrorKind::NotADirectory => ("ENOTDIR", "not a directory"),
        ErrorKind::IsADirectory => ("EISDIR", "illegal operation on a directory"),
        ErrorKind::AlreadyExists => ("EEXIST", "file already exists"),
        _ => ("EIO", "i/o error"),
    }
}

/// `ENOENT: no such file or directory, stat '/x'`，并带 `code`。
pub fn node_error(error: &std::io::Error, syscall: &str, path: &Path) -> RpcError {
    let (code, description) = errno_parts(error);
    RpcError::new(
        "Error",
        format!("{code}: {description}, {syscall} '{}'", path.display()),
    )
    .with_code(code)
}

pub fn plain_error(message: impl Into<String>) -> RpcError {
    RpcError::new("Error", message)
}
