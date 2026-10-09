//! 用户目录（docs/specs/rust-u0-foundation.md §3.2，缺陷 B4）。

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

/// 当前用户的 home 目录：`HOME` → `USERPROFILE` → 系统用户数据库（Unix `getpwuid_r`）。
///
/// 空值按未设置处理；都拿不到时返回错误，绝不返回空路径。
pub fn home_dir() -> io::Result<PathBuf> {
    home_dir_with(|name| std::env::var_os(name))
}

/// 与 [`home_dir`] 相同的解析顺序，环境变量从 `lookup` 读取。用于按启动时捕获的环境快照
/// 构造配置的 composition root（例如 Host 的 `HostEnvironment`），它们不直接读进程环境。
pub fn home_dir_with(lookup: impl Fn(&str) -> Option<OsString>) -> io::Result<PathBuf> {
    // B4 缺陷修复：旧实现在 HOME 为空、USERPROFILE 未设置时返回空 PathBuf，之后的
    // `home.join(".zcode")` 变成相对路径，数据落到当前工作目录下。现在空值视为未设置，
    // 再退回系统用户数据库；仍拿不到就报错，由调用方让启动失败。
    for name in ["HOME", "USERPROFILE"] {
        if let Some(value) = lookup(name).filter(|value| !value.is_empty()) {
            return Ok(PathBuf::from(value));
        }
    }
    user_database_home()
}

/// `~` 与 `~/…`（Windows 也接受 `~\…`）展开为 `home` 下的路径；其余（含 `~user`）原样返回。
///
/// `~` 之后多余的分隔符会被去掉，`~//x` 得到 `home/x`，而不是根目录下的 `/x`。
pub fn expand_home(input: &str, home: &Path) -> PathBuf {
    if input == "~" {
        return home.to_path_buf();
    }
    let separators: &[char] = if cfg!(windows) { &['/', '\\'] } else { &['/'] };
    match input
        .strip_prefix('~')
        .filter(|rest| rest.starts_with(separators))
    {
        Some(rest) => home.join(rest.trim_start_matches(separators)),
        None => PathBuf::from(input),
    }
}

#[cfg(unix)]
#[allow(
    unsafe_code,
    reason = "getpwuid_r 是读取系统用户数据库的唯一途径（与 Node os.homedir() 的回退一致）"
)]
fn user_database_home() -> io::Result<PathBuf> {
    use std::ffi::{CStr, OsStr};
    use std::mem::MaybeUninit;
    use std::os::unix::ffi::OsStrExt;

    // 条目里的字符串都放在调用方提供的缓冲区中；ERANGE 时加倍重试，上限防止异常的 NSS 实现无限增长。
    const MAX_BUFFER: usize = 1 << 20;
    let mut buffer = vec![0_u8; 1024];
    loop {
        let mut entry = MaybeUninit::<libc::passwd>::uninit();
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: getuid 没有前置条件；entry、buffer、found 都是本函数持有的有效可写内存，
        // 传入的长度就是 buffer 的实际长度。
        let code = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut found,
            )
        };
        if code == libc::ERANGE && buffer.len() < MAX_BUFFER {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if code != 0 {
            return Err(unavailable(Some(io::Error::from_raw_os_error(code))));
        }
        if found.is_null() {
            return Err(unavailable(None));
        }
        // SAFETY: 返回 0 且 found 非空时，found 指向已初始化的 entry；pw_dir 为空指针或指向
        // buffer 中以 NUL 结尾的字符串，buffer 在本作用域内一直有效。
        let directory = unsafe {
            let directory = (*found).pw_dir;
            if directory.is_null() {
                return Err(unavailable(None));
            }
            CStr::from_ptr(directory).to_bytes()
        };
        if directory.is_empty() {
            return Err(unavailable(None));
        }
        return Ok(PathBuf::from(OsStr::from_bytes(directory)));
    }
}

#[cfg(not(unix))]
fn user_database_home() -> io::Result<PathBuf> {
    Err(unavailable(None))
}

fn unavailable(cause: Option<io::Error>) -> io::Error {
    let message = "home directory unavailable: HOME and USERPROFILE are unset or empty, \
                   and the user database has no home directory for the current user";
    match cause {
        Some(cause) => io::Error::new(cause.kind(), format!("{message}: {cause}")),
        None => io::Error::new(io::ErrorKind::NotFound, message),
    }
}

#[cfg(test)]
#[path = "paths_tests.rs"]
mod tests;
