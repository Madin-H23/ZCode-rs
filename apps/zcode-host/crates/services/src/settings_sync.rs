//! `settings-sync` channel（packages/services/src/settings-sync/settingsSyncService.ts）。
//!
//! 首启导入提示、Claude 全局 `CLAUDE.md` → `~/.zcode/AGENTS.md` 的迁移已移植；首启 `detect`
//! 返回空结果（与 TS 相同：首启不扫描）。设置页“从其他 Agent 手动导入”的扫描与导入
//! （skills / commands / plugins / MCP）尚未移植，见 docs/specs/rust-host-account.md §5。
use std::path::PathBuf;

use serde_json::{Value, json};
use zcode_host_kit::{Reply, RpcError, ServiceBuilder, log};

use crate::settings::SettingStore;
use crate::settings::schema::parse_patch;

/// `resolveUserHomeDir`：HOME / USERPROFILE（trim 后非空），否则 os.homedir()。
fn user_home(vars: &std::collections::BTreeMap<String, String>, fallback: &std::path::Path) -> PathBuf {
    ["HOME", "USERPROFILE"]
        .iter()
        .find_map(|key| vars.get(*key).map(|v| v.trim()).filter(|v| !v.is_empty()))
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback.to_path_buf())
}

#[derive(Clone)]
pub struct SettingsSync {
    settings: SettingStore,
    home: PathBuf,
}

async fn exists(path: &std::path::Path) -> bool {
    tokio::fs::metadata(path).await.is_ok()
}

impl SettingsSync {
    pub fn new(
        settings: SettingStore,
        vars: &std::collections::BTreeMap<String, String>,
        user_home_fallback: &std::path::Path,
    ) -> Self {
        Self {
            settings,
            home: user_home(vars, user_home_fallback),
        }
    }

    fn source(&self) -> PathBuf {
        self.home.join(".claude").join("CLAUDE.md")
    }

    fn target(&self) -> PathBuf {
        self.home.join(".zcode").join("AGENTS.md")
    }

    pub async fn migration_status(&self) -> Value {
        let (source, target) = (self.source(), self.target());
        let source_exists = exists(&source).await;
        let target_exists = exists(&target).await;
        let mut status = json!({
            "sourcePath": source.display().to_string(),
            "targetPath": target.display().to_string(),
            "sourceExists": source_exists,
            "targetExists": target_exists,
            "supported": source_exists,
        });
        if !source_exists {
            status["unavailableReason"] = json!("missingSource");
        }
        status
    }

    pub async fn copy_agents_file(&self, overwrite: bool) -> Result<Value, RpcError> {
        let (source, target) = (self.source(), self.target());
        let base = json!({
            "sourcePath": source.display().to_string(),
            "targetPath": target.display().to_string(),
        });
        let with = |mut value: Value, extra: Value| {
            for (key, field) in extra.as_object().into_iter().flatten() {
                value[key] = field.clone();
            }
            value
        };
        if !exists(&source).await {
            return Ok(with(base, json!({"status": "skipped", "overwritten": false, "skippedReason": "missingSource"})));
        }
        let target_exists = exists(&target).await;
        if target_exists && !overwrite {
            return Ok(with(base, json!({"status": "skipped", "overwritten": false, "skippedReason": "targetExists"})));
        }
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| RpcError::new("Error", error.to_string()))?;
        }
        tokio::fs::copy(&source, &target)
            .await
            .map_err(|error| RpcError::new("Error", error.to_string()))?;
        Ok(with(base, json!({"status": "copied", "overwritten": target_exists})))
    }

    pub async fn first_run_prompt_state(&self) -> Value {
        let settings = self.settings.get().await;
        json!({"handled": settings["settingsSyncFirstRunPromptHandled"] == true})
    }

    pub async fn mark_first_run_prompt_handled(&self) -> Result<(), RpcError> {
        // 记录“首启导入提示已被消费”（不是“导入成功”）：跳过也算，避免下次启动重复弹窗。
        let patch = parse_patch(&json!({"settingsSyncFirstRunPromptHandled": true}))
            .map_err(|message| RpcError::new("ZodError", message))?;
        self.settings.update(&patch, None).await
    }
}

pub fn service(sync: SettingsSync) -> ServiceBuilder {
    let (status, copy, detect, state, mark) =
        (sync.clone(), sync.clone(), sync.clone(), sync.clone(), sync);
    ServiceBuilder::new("settings-sync")
        .method("getClaudeAgentsFileMigrationStatus", move |_, _| {
            let sync = status.clone();
            async move { Ok(Reply::Value(sync.migration_status().await)) }
        })
        .method("copyClaudeAgentsFileToZcodeAgentsFile", move |_, args| {
            let sync = copy.clone();
            async move {
                let request: Value = args.opt(0, "request")?.unwrap_or_else(|| json!({}));
                let result = sync.copy_agents_file(request["overwrite"] == true).await?;
                log::info(format!(
                    "[settings-sync] Claude AGENTS.md migration completed status={} overwritten={}",
                    result["status"], result["overwritten"]
                ));
                Ok(Reply::Value(result))
            }
        })
        .method("detect", move |_, args| {
            let _ = &detect;
            async move {
                let request: Value = args.opt(0, "request")?.unwrap_or_else(|| json!({}));
                let manual = request["intent"] == "manualImport"
                    && request["categories"]
                        .as_array()
                        .is_some_and(|categories| !categories.is_empty());
                if manual {
                    return Err(RpcError::new(
                        "NotImplemented",
                        "settings-sync manual import is not implemented in the Rust Host yet",
                    )
                    .with_code(zcode_host_kit::NOT_IMPLEMENTED_CODE));
                }
                Ok(Reply::Value(json!({"agents": []})))
            }
        })
        .method("getFirstRunPromptState", move |_, _| {
            let sync = state.clone();
            async move { Ok(Reply::Value(sync.first_run_prompt_state().await)) }
        })
        .method("markFirstRunPromptHandled", move |_, _| {
            let sync = mark.clone();
            async move { sync.mark_first_run_prompt_handled().await.map(|()| Reply::Undefined) }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn first_run_flag_and_agents_file_copy() {
        let home = tempfile::tempdir().unwrap();
        let settings = SettingStore::new(home.path());
        let vars = std::collections::BTreeMap::from([(
            "HOME".to_owned(),
            home.path().display().to_string(),
        )]);
        let sync = SettingsSync::new(settings, &vars, home.path());
        assert_eq!(sync.first_run_prompt_state().await, json!({"handled": false}));
        sync.mark_first_run_prompt_handled().await.unwrap();
        assert_eq!(sync.first_run_prompt_state().await, json!({"handled": true}));
        let status = sync.migration_status().await;
        assert_eq!(status["supported"], false);
        assert_eq!(status["unavailableReason"], "missingSource");
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        std::fs::write(home.path().join(".claude/CLAUDE.md"), "rules").unwrap();
        assert_eq!(sync.copy_agents_file(false).await.unwrap()["status"], "copied");
        let skipped = sync.copy_agents_file(false).await.unwrap();
        assert_eq!(skipped["skippedReason"], "targetExists");
        let overwritten = sync.copy_agents_file(true).await.unwrap();
        assert_eq!(overwritten["overwritten"], true);
        assert_eq!(
            std::fs::read_to_string(home.path().join(".zcode/AGENTS.md")).unwrap(),
            "rules"
        );
    }
}
