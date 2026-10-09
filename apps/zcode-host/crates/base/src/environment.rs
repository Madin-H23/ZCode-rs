//! Host 进程环境快照（composition root 在启动时捕获一次）。
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use zcode_host_control::{ControlSink, InitLocal};

/// 进程环境快照，由 composition root 读取后传入；service 不直接读 `std::env`。
#[derive(Debug, Clone, Default)]
pub struct HostEnvironment {
    /// 设置文件所在的 home：`ZCODE_DESKTOP_HOME_DIR`，否则 `HOME` / `USERPROFILE`。
    pub home_dir: PathBuf,
    /// 业务数据根：`ZCODE_DATA_BASE_DIR`，否则 `HOME`（`~/.zcode/v2` 在它下面）。
    pub data_base_dir: PathBuf,
    /// Node `os.homedir()`：`HOME`，Windows 上为 `USERPROFILE`；不受上面两个覆盖影响。
    pub user_home: PathBuf,
    /// Host 自身的环境变量（Main 启动 Host 时给的），子进程环境以它为底。
    pub vars: BTreeMap<String, String>,
}

impl HostEnvironment {
    /// `vars` 中没有可用的 `HOME` / `USERPROFILE` 时，`user_home` 取系统用户数据库中的 home
    /// （与 Node `os.homedir()` 的回退一致）；仍拿不到就返回错误，Host 启动失败。
    pub fn capture(vars: impl IntoIterator<Item = (String, String)>) -> std::io::Result<Self> {
        let vars: BTreeMap<String, String> = vars.into_iter().collect();
        let non_empty = |name: &str| {
            vars.get(name)
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
        };
        // B4 缺陷修复：旧实现在 HOME、USERPROFILE 都缺失时得到空路径，设置与数据会写到 Host 的
        // 当前工作目录下。改用 zcode-kit 的解析：先读快照里的变量，再退回系统用户数据库，绝不为空。
        let user_home = zcode_kit::paths::home_dir_with(|name| non_empty(name).map(Into::into))?;
        let override_or_home =
            |name: &str| non_empty(name).map_or_else(|| user_home.clone(), PathBuf::from);
        Ok(Self {
            home_dir: override_or_home("ZCODE_DESKTOP_HOME_DIR"),
            data_base_dir: override_or_home("ZCODE_DATA_BASE_DIR"),
            user_home,
            vars,
        })
    }

    /// `<data_base_dir>/.zcode/v2`（`getAppConfigDir`）。
    pub fn app_config_dir(&self) -> PathBuf {
        self.data_base_dir.join(".zcode").join("v2")
    }
}

/// service 构造时共用的上下文：环境快照、`InitLocal` 与到 Main 的控制通道。
pub struct ServiceContext {
    pub environment: HostEnvironment,
    pub init: InitLocal,
    pub control: Arc<dyn ControlSink>,
}

#[cfg(test)]
#[path = "environment_tests.rs"]
mod tests;
