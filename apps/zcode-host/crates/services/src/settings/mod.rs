//! `setting` channel（packages/services/src/setting/settingService.ts）。
//!
//! 文件固定在 `<home>/.zcode/v2/setting.json`（home 取 `ZCODE_DESKTOP_HOME_DIR`，否则 HOME），
//! 与 Main 的 setting service 共享。读改写全程持跨进程锁（S0 修复后的语义），进程内另有写队列
//! 保证 `get` 不会越过已入队的写入。
pub mod schema;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::fs;
use tokio::sync::Mutex;
use zcode_host_kit::{EventHub, Reply, RpcError, ServiceBuilder, log};

use crate::store::{LockOptions, atomic_write_text, with_file_lock};
use schema::Patch;

const PARSE_RETRY_COUNT: usize = 3;
const PARSE_RETRY_DELAY: Duration = Duration::from_millis(300);
const MAX_RECENT_PROJECTS: usize = 10;

struct ReadResult {
    settings: Value,
    raw: Value,
    needs_persist: bool,
}

/// 设置文件的唯一读写入口；Host 内部的其他 service 也通过它读取。
#[derive(Clone)]
pub struct SettingStore {
    inner: Arc<Inner>,
}

struct Inner {
    file: PathBuf,
    home: PathBuf,
    /// 进程内写队列；跨进程互斥由文件锁负责。
    queue: Mutex<()>,
    /// Host 内部监听（相当于 observableSettingService.onDidUpdate），携带 patch 的键。
    updated: EventHub<Vec<String>>,
}

fn quarantine_name(file: &Path) -> PathBuf {
    // new Date().toISOString().replace(/[:.]/g, "-")
    let iso = chrono_like_iso_now().replace([':', '.'], "-");
    PathBuf::from(format!("{}.corrupt-{iso}", file.display()))
}

fn chrono_like_iso_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let (secs, millis) = (now.as_secs() as i64, now.subsec_millis());
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant 的 days → civil 日期算法。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

impl SettingStore {
    pub fn new(home: &Path) -> Self {
        Self {
            inner: Arc::new(Inner {
                file: home.join(".zcode").join("v2").join("setting.json"),
                home: home.to_owned(),
                queue: Mutex::new(()),
                updated: EventHub::new(),
            }),
        }
    }

    pub fn file(&self) -> &Path {
        &self.inner.file
    }

    pub fn on_updated(&self) -> EventHub<Vec<String>> {
        self.inner.updated.clone()
    }

    async fn read(&self) -> ReadResult {
        let defaults = || ReadResult {
            settings: schema::default_settings(),
            raw: Value::Object(Map::new()),
            needs_persist: false,
        };
        let mut attempt = 0;
        let raw: Value = loop {
            let text = match fs::read_to_string(&self.inner.file).await {
                Ok(text) => text,
                Err(error) => {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        log::warn(format!("setting read failed, using defaults: {error}"));
                    }
                    return defaults();
                }
            };
            match serde_json::from_str::<Value>(&text) {
                Ok(value) => break value,
                Err(error) if attempt + 1 < PARSE_RETRY_COUNT => {
                    attempt += 1;
                    log::debug(format!("setting parse failed, retrying: {error}"));
                    tokio::time::sleep(PARSE_RETRY_DELAY).await;
                }
                Err(error) => {
                    // 手动编辑或磁盘异常写坏的文件：备份后返回默认值，下一次 update 重建合法配置。
                    let backup = quarantine_name(&self.inner.file);
                    match fs::rename(&self.inner.file, &backup).await {
                        Ok(()) => log::warn(format!(
                            "invalid setting.json backed up to {}: {error}",
                            backup.display()
                        )),
                        Err(rename) if rename.kind() == std::io::ErrorKind::NotFound => {}
                        Err(rename) => log::warn(format!("setting quarantine failed: {rename}")),
                    }
                    return defaults();
                }
            }
        };
        let migrated = schema::migrate_legacy_account_connections(&raw);
        match schema::parse_settings(&migrated) {
            Some(settings) => ReadResult {
                needs_persist: schema::should_persist_migrations(&raw),
                settings,
                raw,
            },
            None => {
                log::warn("setting.json failed validation; using defaults");
                ReadResult { raw, ..defaults() }
            }
        }
    }

    /// 在进程内队列和跨进程锁内执行一次读改写。
    async fn locked<T>(
        &self,
        operation: impl AsyncFnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let _queue = self.inner.queue.lock().await;
        with_file_lock(&self.inner.file, &LockOptions::default(), operation).await
    }

    async fn write(
        &self,
        settings: &Value,
        raw: &Value,
        patch_has_selections: bool,
    ) -> anyhow::Result<()> {
        let mut persisted = schema::retained_legacy_fields(raw);
        if let Some(settings) = settings.as_object() {
            for (key, value) in settings {
                persisted.insert(key.clone(), value.clone());
            }
        }
        // 旧 team 连接还缺组织信息时，普通写入不能把默认 {} 当成用户的新选择落盘。
        if !patch_has_selections && schema::has_incomplete_legacy_team_connections(raw) {
            persisted.remove("providerFamilyConnectionSelections");
        }
        let text = serde_json::to_string_pretty(&Value::Object(persisted))?;
        atomic_write_text(&self.inner.file, &text, false).await
    }

    pub async fn get(&self) -> Value {
        // 先等已入队的写入完成，保证读到已提交的选择。
        drop(self.inner.queue.lock().await);
        let result = self.read().await;
        if !result.needs_persist {
            return result.settings;
        }
        let persisted = self
            .locked(async || {
                let latest = self.read().await;
                if latest.needs_persist {
                    self.write(&latest.settings, &latest.raw, true).await?;
                }
                Ok(())
            })
            .await;
        if let Err(error) = persisted {
            log::warn(format!("setting migration persist failed: {error:#}"));
        }
        self.read().await.settings
    }

    pub async fn update(&self, patch: &Patch, expected: Option<&Patch>) -> Result<(), RpcError> {
        let has_selections = patch
            .iter()
            .any(|(key, _)| key == "providerFamilyConnectionSelections");
        let result = self
            .locked(async || {
                let current = self.read().await;
                if let Some(expected) = expected {
                    let expected_value = |key: &str| {
                        expected
                            .iter()
                            .find(|(name, _)| name == key)
                            .and_then(|(_, value)| value.clone())
                    };
                    let domain_matches = current.settings.get("providerFamilyDomain").cloned()
                        == expected_value("providerFamilyDomain");
                    let selections = |value: Option<Value>| {
                        value
                            .unwrap_or_else(|| Value::Object(Map::new()))
                            .to_string()
                    };
                    let selections_match =
                        selections(
                            current
                                .settings
                                .get("providerFamilyConnectionSelections")
                                .cloned(),
                        ) == selections(expected_value("providerFamilyConnectionSelections"));
                    if !domain_matches || !selections_match {
                        anyhow::bail!("Account connection settings changed");
                    }
                }
                let mut merged = schema::merge(&current.settings, patch)
                    .ok_or_else(|| anyhow::anyhow!("ZodError: merged settings are invalid"))?;
                if let Some(recent) = merged
                    .get_mut("recentProjects")
                    .and_then(Value::as_array_mut)
                {
                    let mut seen = std::collections::HashSet::new();
                    recent.retain(|item| seen.insert(item.to_string()));
                    recent.truncate(MAX_RECENT_PROJECTS);
                }
                self.write(&merged, &current.raw, has_selections).await
            })
            .await;
        match result {
            Ok(()) => {
                self.inner
                    .updated
                    .emit(patch.iter().map(|(key, _)| key.clone()).collect());
                Ok(())
            }
            Err(error) => {
                let message = format!("{error:#}");
                let name = if message.starts_with("ZodError") {
                    "ZodError"
                } else {
                    "Error"
                };
                Err(RpcError::new(name, message))
            }
        }
    }

    /// `ensureDefaultProject`：`<homedir>/ZCodeProject`，mkdir -p。
    pub async fn ensure_default_project(&self, homedir: Option<String>) -> Result<Value, RpcError> {
        let base = homedir
            .map(PathBuf::from)
            .unwrap_or_else(|| self.inner.home.clone());
        let path = base.join("ZCodeProject");
        let existed = fs::metadata(&path).await.is_ok();
        fs::create_dir_all(&path)
            .await
            .map_err(|error| RpcError::new("Error", error.to_string()))?;
        Ok(serde_json::json!({"path": path.display().to_string(), "created": !existed}))
    }
}

fn parse_patch_arg(value: &Value) -> Result<Patch, RpcError> {
    schema::parse_patch(value).map_err(|message| RpcError::new("ZodError", message))
}

pub fn service(store: SettingStore) -> ServiceBuilder {
    let (get_store, update_store, project_store) = (store.clone(), store.clone(), store);
    ServiceBuilder::new("setting")
        .method("get", move |_, _| {
            let store = get_store.clone();
            async move { Ok(Reply::Value(store.get().await)) }
        })
        .method("update", move |_, args| {
            let store = update_store.clone();
            let patch = args
                .get::<Value>(0, "patch")
                .and_then(|value| parse_patch_arg(&value));
            let expected = args
                .opt::<Value>(1, "expectedAccountSettings")
                .and_then(|value| value.map(|value| parse_patch_arg(&value)).transpose());
            async move {
                let (patch, expected) = (patch?, expected?);
                store.update(&patch, expected.as_ref()).await?;
                Ok(Reply::Undefined)
            }
        })
        .method("ensureDefaultProject", move |_, args| {
            let store = project_store.clone();
            let homedir = args.opt::<String>(0, "homedir");
            async move { Ok(Reply::Value(store.ensure_default_project(homedir?).await?)) }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn missing_file_returns_defaults_without_writing() {
        let home = tempfile::tempdir().unwrap();
        let store = SettingStore::new(home.path());
        assert_eq!(store.get().await, schema::default_settings());
        assert!(!store.file().exists());
    }

    #[tokio::test]
    async fn update_persists_pretty_json_and_keeps_legacy_fields() {
        let home = tempfile::tempdir().unwrap();
        let store = SettingStore::new(home.path());
        std::fs::create_dir_all(store.file().parent().unwrap()).unwrap();
        std::fs::write(
            store.file(),
            r#"{"closeToTrayOnWindowsMigrationInitialized":true,"messageStreamShowReasoningMigrationInitialized":true,"modelProviderFamilyModes":{"zai":"apiKey"}}"#,
        )
        .unwrap();
        let patch = schema::parse_patch(&json!({"recentProjects": ["/a", "/a", "/b"]})).unwrap();
        store.update(&patch, None).await.unwrap();
        let text = std::fs::read_to_string(store.file()).unwrap();
        assert!(text.starts_with("{\n  \"modelProviderFamilyModes\""));
        let saved: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(saved["recentProjects"], json!(["/a", "/b"]));
        assert_eq!(store.get().await["recentProjects"], json!(["/a", "/b"]));
    }

    #[tokio::test]
    async fn migration_flags_trigger_one_persist_on_read() {
        let home = tempfile::tempdir().unwrap();
        let store = SettingStore::new(home.path());
        std::fs::create_dir_all(store.file().parent().unwrap()).unwrap();
        std::fs::write(store.file(), r#"{"locale":"en-US"}"#).unwrap();
        let settings = store.get().await;
        assert_eq!(settings["localePreference"], "en-US");
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(store.file()).unwrap()).unwrap();
        assert_eq!(saved["closeToTrayOnWindowsMigrationInitialized"], true);
    }

    #[tokio::test]
    async fn corrupt_file_is_quarantined() {
        let home = tempfile::tempdir().unwrap();
        let store = SettingStore::new(home.path());
        std::fs::create_dir_all(store.file().parent().unwrap()).unwrap();
        std::fs::write(store.file(), ":wq").unwrap();
        assert_eq!(store.get().await, schema::default_settings());
        let names: Vec<String> = std::fs::read_dir(store.file().parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names
                .iter()
                .any(|name| name.starts_with("setting.json.corrupt-"))
        );
    }

    #[tokio::test]
    async fn expected_account_settings_guard_rejects_stale_writes() {
        let home = tempfile::tempdir().unwrap();
        let store = SettingStore::new(home.path());
        let patch = schema::parse_patch(&json!({"providerFamilyDomain": "zai"})).unwrap();
        let expected = schema::parse_patch(&json!({"providerFamilyDomain": "bigmodel"})).unwrap();
        let error = store.update(&patch, Some(&expected)).await.unwrap_err();
        assert!(
            error
                .message
                .contains("Account connection settings changed")
        );
    }
}
