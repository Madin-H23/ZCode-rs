//! Node 风格的系统错误文案：UI 会按 `code` 或消息里的 `ENOENT` 等字样分支，必须保持同一形态。
use std::io::{Error, ErrorKind};

/// libuv 的错误码名与描述（`uv_err_name` / `uv_strerror`）；认不出的返回 None。
pub fn code_of(error: &Error) -> Option<(&'static str, &'static str)> {
    // Windows 的 ErrorKind 太粗（拒绝访问、共享冲突都不是 Node 的同名错误码），先按原始错误码映射。
    #[cfg(windows)]
    if let Some(code) = error.raw_os_error().and_then(windows_code) {
        return Some(code);
    }
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

/// Windows 系统错误码 → libuv 错误名与描述（libuv `uv_translate_sys_error`）。
/// 例如 ERROR_ACCESS_DENIED 在 Node 里是 EPERM，被占用的文件（共享 / 锁冲突）是 EBUSY。
pub fn windows_code(raw: i32) -> Option<(&'static str, &'static str)> {
    Some(match raw {
        2 | 3 | 15 | 123 | 126 | 4392 => ("ENOENT", "no such file or directory"),
        5 | 1314 => ("EPERM", "operation not permitted"),
        998 | 740 | 1920 => ("EACCES", "permission denied"),
        32 | 33 => ("EBUSY", "resource busy or locked"),
        80 | 183 => ("EEXIST", "file already exists"),
        4 => ("EMFILE", "too many open files"),
        17 => ("EXDEV", "cross-device link not permitted"),
        39 | 112 => ("ENOSPC", "no space left on device"),
        145 => ("ENOTEMPTY", "directory not empty"),
        206 => ("ENAMETOOLONG", "name too long"),
        267 => ("ENOTDIR", "not a directory"),
        1921 => ("ELOOP", "too many symbolic links encountered"),
        _ => return None,
    })
}

#[cfg(test)]
mod windows_tests {
    use super::*;

    #[test]
    fn windows_codes_follow_libuv() {
        assert_eq!(windows_code(5).map(|code| code.0), Some("EPERM"));
        assert_eq!(windows_code(32).map(|code| code.0), Some("EBUSY"));
        assert_eq!(windows_code(3).map(|code| code.0), Some("ENOENT"));
        assert_eq!(windows_code(9999), None);
    }
}
