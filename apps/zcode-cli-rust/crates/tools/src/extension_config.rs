use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};
use tokio::io::AsyncReadExt;

/// `~` / `~/…` against `home`, other relative paths against `base`; `.` and
/// `..` components are folded. `home` comes from `zcode_kit::paths::home_dir`.
pub(super) fn resolve(base: &Path, path: &str, home: &Path) -> PathBuf {
    // B4 缺陷修复：旧实现在 HOME 为空时把 `~/x` 解析成相对路径 `x`（落到进程 cwd 下）；
    // 现在 home 由调用方经 home_dir() 取得，绝不为空，`~` 展开与其他入口一致。
    let raw = base.join(zcode_kit::paths::expand_home(path, home));
    let mut result = PathBuf::new();
    for part in raw.components() {
        match part {
            Component::CurDir => (),
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other),
        }
    }
    result
}
pub(super) async fn json_file(path: &Path) -> Result<Value> {
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(e) => return Err(e.into()),
    };
    let mut bytes = vec![];
    file.take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(
        bytes.len() <= 4 * 1024 * 1024,
        "Extension configuration exceeds size limit"
    );
    Ok(serde_json::from_slice(&bytes)?)
}
pub(super) async fn project_directories(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![];
    for dir in cwd.ancestors() {
        dirs.push(dir.to_owned());
        if tokio::fs::metadata(dir.join(".git")).await.is_ok() {
            return dirs;
        }
    }
    vec![cwd.to_owned()]
}
pub(super) fn strings(value: &Value) -> Vec<&str> {
    if let Some(s) = value.as_str() {
        vec![s]
    } else {
        value
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }
}

/// Plugin discovery for the runtime (skills, MCP, agents; spec rust-m10-plugins §3.5).
pub(super) async fn plugins(
    cwd: &Path,
    config: &Value,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<zcode_cli_plugins::Outcome> {
    let storage = zcode_cli_plugins::records::storage_root(config, &zcode_kit::paths::home_dir()?);
    let lookup = |name: &str| std::env::var(name).ok();
    zcode_cli_plugins::discover(&zcode_cli_plugins::Request {
        config,
        storage: &storage,
        cwd,
        env: &lookup,
        cancel,
    })
    .await
}

/// `path` is inside `root` and no component on the way is a symlink.
pub(super) async fn contained_file(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let mut current = root.to_owned();
    for part in std::iter::once(None).chain(relative.components().map(Some)) {
        if let Some(part) = part {
            current.push(part);
        }
        if !tokio::fs::symlink_metadata(&current)
            .await
            .is_ok_and(|m| !m.file_type().is_symlink())
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
#[path = "extension_config_tests.rs"]
mod tests;
