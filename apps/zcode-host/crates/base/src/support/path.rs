//! Node `path` 模块的子集（`resolve` / `relative` / `isAbsolute`），只做词法处理，分隔符用平台的；
//! [`realpath`] 在 Windows 上去掉 `\\?\` 前缀，与 Node `fs.realpath` 的返回形态一致。
use std::path::{Component, MAIN_SEPARATOR_STR, Path, PathBuf};

/// `path.isAbsolute`：Windows 上 `\foo`、`/foo` 也算绝对路径（Rust 的 `is_absolute` 要求盘符）。
pub fn is_absolute(path: &str) -> bool {
    if !cfg!(windows) {
        return path.starts_with('/');
    }
    let bytes = path.as_bytes();
    matches!(bytes.first(), Some(b'/' | b'\\'))
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
}

/// 拆成（根，各级名称）；`.` 与 `..` 已按词法消去，根之上的 `..` 丢弃。
fn split(path: &Path) -> (String, Vec<String>) {
    let mut root = String::new();
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => root = prefix.as_os_str().to_string_lossy().into_owned(),
            Component::RootDir => root.push_str(MAIN_SEPARATOR_STR),
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.last().is_some_and(|last| last != "..") {
                    parts.pop();
                } else if root.is_empty() {
                    parts.push("..".to_owned());
                }
            }
            Component::Normal(name) => parts.push(name.to_string_lossy().into_owned()),
        }
    }
    (root, parts)
}

fn join(root: &str, parts: &[String]) -> String {
    let joined = format!("{root}{}", parts.join(MAIN_SEPARATOR_STR));
    if joined.is_empty() {
        ".".to_owned()
    } else {
        joined
    }
}

/// `path.resolve(base, ...segments)`；绝对的 segment 会替换之前的部分。
pub fn resolve(base: &str, segments: &[&str]) -> String {
    let mut path = PathBuf::from(base);
    for segment in segments {
        path.push(segment);
    }
    let (root, parts) = split(&path);
    join(&root, &parts)
}

fn same_name(left: &str, right: &str) -> bool {
    if cfg!(windows) {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

/// `path.relative(from, to)`：两者都先 resolve；根不同（Windows 跨盘）时返回 `to` 本身。
pub fn relative(from: &str, to: &str) -> String {
    let (from_root, from_parts) = split(Path::new(&resolve(from, &[])));
    let (to_root, to_parts) = split(Path::new(&resolve(to, &[])));
    if !same_name(&from_root, &to_root) {
        return join(&to_root, &to_parts);
    }
    let common = from_parts
        .iter()
        .zip(&to_parts)
        .take_while(|(left, right)| same_name(left, right))
        .count();
    let mut parts: Vec<String> = vec!["..".to_owned(); from_parts.len() - common];
    parts.extend(to_parts[common..].iter().cloned());
    parts.join(MAIN_SEPARATOR_STR)
}

/// `fs.realpath`：跟随符号链接得到规范路径。
pub async fn realpath(path: &str) -> std::io::Result<String> {
    let real = tokio::fs::canonicalize(path).await?;
    Ok(strip_verbatim(&real.to_string_lossy()))
}

/// `\\?\C:\x` → `C:\x`，`\\?\UNC\server\share` → `\\server\share`；其他形态原样返回。
pub fn strip_verbatim(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    match path.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest.to_owned(),
        _ => path.to_owned(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn resolve_normalizes_like_node() {
        assert_eq!(resolve("/repo", &["a", "b"]), "/repo/a/b");
        assert_eq!(resolve("/repo/sub", &["../x/./y/"]), "/repo/x/y");
        assert_eq!(resolve("/repo", &["/abs"]), "/abs");
        assert_eq!(resolve("/", &[".."]), "/");
    }

    #[test]
    fn relative_matches_node() {
        assert_eq!(relative("/repo", "/repo/a/b"), "a/b");
        assert_eq!(relative("/repo", "/repo"), "");
        assert_eq!(relative("/repo/sub", "/repo/x"), "../x");
        assert_eq!(relative("/repo", "/other"), "../other");
    }

    #[test]
    fn verbatim_prefix_is_stripped() {
        assert_eq!(strip_verbatim(r"\\?\C:\repo"), r"C:\repo");
        assert_eq!(strip_verbatim(r"\\?\UNC\srv\share\x"), r"\\srv\share\x");
        assert_eq!(strip_verbatim("/tmp/x"), "/tmp/x");
    }
}
