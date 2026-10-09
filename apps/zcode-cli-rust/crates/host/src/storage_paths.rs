//! The Node session database and artifact root, read from the storage
//! configuration without writing config.
use std::path::{Path, PathBuf};
pub struct StoragePaths {
    pub database: PathBuf,
    pub artifacts: PathBuf,
}
/// `~` / `~/…` against home (bare `~` included), everything else relative to `cwd`.
fn expand(path: &str, cwd: &Path, home: &Path) -> PathBuf {
    cwd.join(zcode_kit::paths::expand_home(path, home))
}
/// Locate the session database the Node runtime would use for `cwd`.
/// Storage paths come from the same layered configuration as Node (`storage.dir`,
/// `storage.sessionDbPath`, including `ZCODE_*` overrides).
/// `home` comes from `zcode_kit::paths::home_dir` at startup (never empty).
pub fn resolve(
    cwd: &Path,
    home: &Path,
    config: &crate::domain::config::ConfigSnapshot,
) -> StoragePaths {
    let database = config
        .str("storage", "sessionDbPath")
        .unwrap_or("~/.zcode/cli/db/db.sqlite");
    let root = config.str("storage", "dir").unwrap_or("~/.zcode");
    StoragePaths {
        database: expand(database, cwd, home),
        artifacts: expand(root, cwd, home).join("cli/artifacts"),
    }
}

#[cfg(test)]
#[path = "storage_paths_tests.rs"]
mod tests;
