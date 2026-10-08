//! R5 扩展服务（docs/specs/rust-host-extensions.md）：skills、skill-sync、plugins、plugin-sync、
//! commands、memory、hooks、mcp-sync、subagents。
//!
//! 这些 channel 都是无状态的文件系统视图：每次调用重新扫描，Host 不缓存；与 Main、CLI、Node Host
//! 共享的配置文件一律经 [`cli_config::update`] 持跨进程锁读改写。
pub mod cli_config;
pub mod fsx;
pub mod json;
pub mod memory;
pub mod plugin_roots;
pub mod plugins;
pub mod yaml;

use std::path::Path;

/// 路径根：用户目录（`HOME` / `USERPROFILE`）与数据根（`{dataBaseDir}/.zcode`）。
#[derive(Debug, Clone)]
pub struct Roots {
    pub home: String,
    pub data_root: String,
}

impl Roots {
    pub fn new(home: &Path, data_base_dir: &Path) -> Self {
        Self {
            home: fsx::path_str(home),
            data_root: fsx::join(&fsx::path_str(data_base_dir), &[".zcode"]),
        }
    }

    /// `~/.zcode/cli/config.json`。
    pub fn cli_config_path(&self) -> String {
        fsx::join(&self.home, &[".zcode", "cli", "config.json"])
    }

    /// `resolveConfigPath`：`~/` 展开到用户目录；相对路径相对进程 cwd。
    pub fn resolve_config_path(&self, path: &str) -> String {
        let expanded = match path.strip_prefix("~/") {
            Some(rest) => fsx::join(&self.home, &[rest]),
            None => path.to_owned(),
        };
        if fsx::is_absolute(&expanded) {
            expanded
        } else {
            fsx::resolve(&expanded, &[])
        }
    }
}
