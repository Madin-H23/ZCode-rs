//! Rust Host 的业务 service（docs/specs/rust-host.md §5）。
//!
//! [`install`] 在收到 `InitLocal` 时调用：按 Main 给的路径和环境构建已实现的 service，覆盖契约
//! stub，并登记它们关心的 Main → Host 控制消息。没实现的成员仍由 stub 立即应答。
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use zcode_host_control::{ControlRouter, ControlSink, InitLocal};
use zcode_host_kit::ChannelRegistry;

pub mod account;
pub mod broadcast;
pub mod main_bridge;
pub mod providers;
pub mod settings;
pub mod store;

use main_bridge::MainBridge;

/// 进程环境快照，由 composition root 读取后传入；service 不直接读 `std::env`。
#[derive(Debug, Clone, Default)]
pub struct HostEnvironment {
    /// 设置文件所在的 home：`ZCODE_DESKTOP_HOME_DIR`，否则 `HOME` / `USERPROFILE`。
    pub home_dir: PathBuf,
    /// 业务数据根：`ZCODE_DATA_BASE_DIR`，否则 `HOME`（`~/.zcode/v2` 在它下面）。
    pub data_base_dir: PathBuf,
    /// Host 自身的环境变量（Main 启动 Host 时给的），子进程环境以它为底。
    pub vars: BTreeMap<String, String>,
}

impl HostEnvironment {
    pub fn capture(vars: impl IntoIterator<Item = (String, String)>) -> Self {
        let vars: BTreeMap<String, String> = vars.into_iter().collect();
        let non_empty = |name: &str| {
            vars.get(name)
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        let user_home = non_empty("HOME")
            .or_else(|| non_empty("USERPROFILE"))
            .unwrap_or_default();
        Self {
            home_dir: non_empty("ZCODE_DESKTOP_HOME_DIR").unwrap_or_else(|| user_home.clone()),
            data_base_dir: non_empty("ZCODE_DATA_BASE_DIR").unwrap_or(user_home),
            vars,
        }
    }

    /// `<data_base_dir>/.zcode/v2`（`getAppConfigDir`）。
    pub fn app_config_dir(&self) -> PathBuf {
        self.data_base_dir.join(".zcode").join("v2")
    }
}

pub struct ServiceContext {
    pub environment: HostEnvironment,
    pub init: InitLocal,
    pub control: Arc<dyn ControlSink>,
}

pub fn install(
    context: Arc<ServiceContext>,
    registry: &mut ChannelRegistry,
    router: &mut ControlRouter,
) {
    let bridge = MainBridge::new(context.control.clone());
    let settings = settings::SettingStore::new(&context.environment.home_dir);
    registry.implement(broadcast::service(bridge.clone(), router));
    registry.implement(settings::service(settings.clone()));

    let providers = providers::ProviderRuntime::new(
        context
            .init
            .zcode_builtin_provider_config_file_path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
            .map(PathBuf::from),
        context
            .environment
            .app_config_dir()
            .join("provider_config.json"),
    );
    providers.start_polling();
    registry.implement(providers::provider_settings_service(providers.clone()));
    registry.implement(providers::model_selection_service(providers.clone()));

    registry.implement(account::oauth_service(&context.environment.vars));
    registry.implement(account::onboarding_record_service());
    registry.implement(account::usage_stats_service());
    registry.implement(account::coding_plan_service(providers.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_prefers_explicit_overrides() {
        let env = HostEnvironment::capture([
            ("HOME".to_owned(), "/home/u".to_owned()),
            ("ZCODE_DESKTOP_HOME_DIR".to_owned(), "/tmp/desk".to_owned()),
        ]);
        assert_eq!(env.home_dir, PathBuf::from("/tmp/desk"));
        assert_eq!(env.data_base_dir, PathBuf::from("/home/u"));
        assert_eq!(env.app_config_dir(), PathBuf::from("/home/u/.zcode/v2"));
    }
}
