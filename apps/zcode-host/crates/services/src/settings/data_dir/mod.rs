//! `setting.updateDataBaseDir(newDir?)`（packages/services/src/setting/settingService.ts）：
//! Windows 安装目录守卫 → 复制 `.zcode/v2` → 经 [`SettingStore`] 写 `dataBaseDir`。
//!
//! Host 自己的数据根（[`HostEnvironment::data_base_dir`]）在运行期不变，重启后才由 Main 按
//! `setting.json` 下发新目录（UI 显示需要重启）。见 docs/specs/rust-host-setting.md。
mod copy;
mod guard;
#[cfg(test)]
mod parity_tests;
mod win32_path;

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::json;
use zcode_host_kit::{Reply, RpcError, ServiceBuilder, log};

use super::SettingStore;
use super::schema::{self, Patch};
use crate::HostEnvironment;

pub use copy::{CopyError, copy_data_directory};
pub use guard::{FORBIDDEN_CODE, Forbidden, GuardInput, validate};

use guard::js_trim;

/// 数据目录迁移；`setting` channel 的 `updateDataBaseDir` 由它应答。
#[derive(Clone)]
pub struct DataDirMove {
    inner: Arc<Inner>,
}

struct Inner {
    settings: SettingStore,
    /// Host 启动时的数据根（`getDataBaseDir()`），运行期不变。
    current_base_dir: String,
    /// `os.homedir()`：`newDir` 为空时的目标。
    user_home: String,
    /// 守卫读的进程环境（`ProgramFiles`、`ZCODE_WINDOWS_APP_INSTALL_DIR`、`=X:` 等）。
    vars: BTreeMap<String, String>,
    /// `process.platform === "win32"`；生产为 `cfg!(windows)`，测试可以打开。
    windows: bool,
}

impl DataDirMove {
    pub fn new(settings: SettingStore, environment: &HostEnvironment) -> Self {
        Self::with_paths(
            settings,
            &environment.data_base_dir.to_string_lossy(),
            &environment.user_home.to_string_lossy(),
            environment.vars.clone(),
            cfg!(windows),
        )
    }

    fn with_paths(
        settings: SettingStore,
        current_base_dir: &str,
        user_home: &str,
        vars: BTreeMap<String, String>,
        windows: bool,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                settings,
                current_base_dir: current_base_dir.to_owned(),
                user_home: user_home.to_owned(),
                vars,
                windows,
            }),
        }
    }

    /// `updateDataBaseDir(newDir)`；`None` 即 TS 的 `undefined`（恢复默认，删除 `dataBaseDir`）。
    pub async fn update_data_base_dir(&self, new_dir: Option<String>) -> Result<(), RpcError> {
        let inner = &self.inner;
        let trimmed = new_dir.as_deref().map(js_trim).unwrap_or_default();
        let target = if trimmed.is_empty() {
            inner.user_home.clone()
        } else {
            trimmed.to_owned()
        };

        // 安装目录由安装器 / 自动更新管理，迁移前在 service 层拦截，绕过前端的 RPC 也拦得住。
        #[expect(clippy::disallowed_methods, reason = "存量，U3 改为显式传入")]
        let cwd = if inner.windows {
            std::env::current_dir()
                .map(|dir| dir.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            String::new()
        };
        let input = GuardInput {
            windows: inner.windows,
            env: &inner.vars,
            app_install_dir: None,
            cwd: &cwd,
        };
        if let Err(Forbidden { forbidden_dir }) = validate(&target, &input) {
            return Err(
                RpcError::new("Error", format!("{FORBIDDEN_CODE}: {forbidden_dir}"))
                    .with_code(FORBIDDEN_CODE),
            );
        }

        if inner.current_base_dir != target {
            log::info(format!(
                "copying data directory from {} to {target}",
                inner.current_base_dir
            ));
            copy_data_directory(&inner.current_base_dir, &target).await?;
            log::info("data directory copy done");
        }

        // patch 用原始参数（同 Node `this.update({dataBaseDir: newDir})`）：schema 去掉首尾空白，
        // 只有空白时报 ZodError；undefined 删除字段。写入只经 SettingStore（锁 + 原子写）。
        let patch: Patch = match new_dir {
            None => vec![("dataBaseDir".to_owned(), None)],
            Some(new_dir) => schema::parse_patch(&json!({ "dataBaseDir": new_dir }))
                .map_err(|message| RpcError::new("ZodError", message))?,
        };
        inner.settings.update(&patch, None).await
    }
}

/// `setting` channel 上的 `updateDataBaseDir`；叠在 R1 的 setting service 上，其他成员仍由它应答。
pub fn service(mover: DataDirMove) -> ServiceBuilder {
    ServiceBuilder::new("setting").method("updateDataBaseDir", move |_, args| {
        let mover = mover.clone();
        let new_dir = args.opt::<String>(0, "newDir");
        async move {
            mover.update_data_base_dir(new_dir?).await?;
            Ok(Reply::Undefined)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn mover(home: &std::path::Path, current: &std::path::Path, windows: bool) -> DataDirMove {
        let vars = BTreeMap::from([("ProgramFiles".to_owned(), "C:\\Program Files".to_owned())]);
        DataDirMove::with_paths(
            SettingStore::new(home),
            &current.to_string_lossy(),
            &home.to_string_lossy(),
            vars,
            windows,
        )
    }

    fn saved(home: &std::path::Path) -> Value {
        let text = std::fs::read_to_string(home.join(".zcode/v2/setting.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    #[tokio::test]
    async fn guard_rejects_before_copying_or_writing() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let move_data = mover(&home, &home, true);
        let error = move_data
            .update_data_base_dir(Some("C:\\Program Files\\ZCode\\data".to_owned()))
            .await
            .unwrap_err();
        assert_eq!(error.name, "Error");
        assert_eq!(error.code_str(), Some(FORBIDDEN_CODE));
        assert_eq!(
            error.message,
            "DATA_BASE_DIR_FORBIDDEN_WINDOWS_INSTALL_DIR: C:\\Program Files\\ZCode"
        );
        assert!(!home.join(".zcode").exists());
    }

    #[tokio::test]
    async fn success_writes_trimmed_dir_and_notifies() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let current = root.path().join("old");
        std::fs::create_dir_all(current.join(".zcode/v2")).unwrap();
        std::fs::write(current.join(".zcode/v2/marker.txt"), "data").unwrap();
        let move_data = mover(&home, &current, false);
        let mut updates = Box::pin(move_data.inner.settings.on_updated().subscribe());

        let target = root.path().join("new");
        move_data
            .update_data_base_dir(Some(format!("  {}  ", target.display())))
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(target.join(".zcode/v2/marker.txt")).unwrap(),
            "data"
        );
        assert_eq!(saved(&home)["dataBaseDir"], target.display().to_string());
        let keys = futures_util::StreamExt::next(&mut updates).await.unwrap();
        assert_eq!(keys, vec!["dataBaseDir".to_owned()]);
    }

    #[tokio::test]
    async fn undefined_resets_to_home_and_removes_the_field() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        std::fs::create_dir_all(home.join(".zcode/v2")).unwrap();
        std::fs::write(
            home.join(".zcode/v2/setting.json"),
            r#"{"dataBaseDir":"/elsewhere"}"#,
        )
        .unwrap();
        let move_data = mover(&home, &home, false);
        move_data.update_data_base_dir(None).await.unwrap();
        assert!(saved(&home).get("dataBaseDir").is_none());
    }
}
