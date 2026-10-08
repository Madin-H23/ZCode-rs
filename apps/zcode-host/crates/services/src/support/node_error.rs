//! Node 风格的系统错误文案：UI 会按 `code` 或消息里的 `ENOENT` 等字样分支，必须保持同一形态。
use std::io::{Error, ErrorKind};

/// libuv 的错误码名与描述（`uv_err_name` / `uv_strerror`）；认不出的返回 None。
pub fn code_of(error: &Error) -> Option<(&'static str, &'static str)> {
    Some(match error.kind() {
        ErrorKind::NotFound => ("ENOENT", "no such file or directory"),
        ErrorKind::PermissionDenied => {
            // EPERM 与 EACCES 在 Rust 里都映射为 PermissionDenied，按原始 errno 区分。
            #[cfg(unix)]
            if error.raw_os_error() == Some(libc::EPERM) {
                return Some(("EPERM", "operation not permitted"));
            }
            ("EACCES", "permission denied")
        }
        ErrorKind::NotADirectory => ("ENOTDIR", "not a directory"),
        ErrorKind::IsADirectory => ("EISDIR", "illegal operation on a directory"),
        ErrorKind::AlreadyExists => ("EEXIST", "file already exists"),
        ErrorKind::StorageFull => ("ENOSPC", "no space left on device"),
        _ => return None,
    })
}

/// `fs` 调用失败时 Node 的消息：`"ENOENT: no such file or directory, watch '/x'"`。
pub fn fs_message(error: &Error, syscall: &str, path: &str) -> String {
    match code_of(error) {
        Some((code, description)) => format!("{code}: {description}, {syscall} '{path}'"),
        None => format!("{error}, {syscall} '{path}'"),
    }
}

/// `child_process.spawn` 失败时 Node 的消息：`"spawn git ENOENT"`。
pub fn spawn_message(file: &str, error: &Error) -> String {
    match code_of(error) {
        Some((code, _)) => format!("spawn {file} {code}"),
        None => format!("spawn {file} {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_node() {
        let missing = Error::from(ErrorKind::NotFound);
        assert_eq!(
            fs_message(&missing, "watch", "/x"),
            "ENOENT: no such file or directory, watch '/x'"
        );
        assert_eq!(spawn_message("git", &missing), "spawn git ENOENT");
    }
}
