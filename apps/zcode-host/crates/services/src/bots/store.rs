//! Bot 配置/状态文件（bots/repo.ts）：`~/.zcode/v2/bot-config.v3.json`、`bot-state.v3.json`。
//!
//! 与 Node 共享同一批文件：读写都在跨进程文件锁内，写入走私密原子写（0600）。当前文件缺失时
//! 从旧版文件单向导入并立即落盘。
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use tokio::fs;

use super::model::{
    import_legacy_config, import_legacy_state, parse_config_file, parse_state_file,
};
use crate::store::{LockOptions, atomic_write_private_text, with_file_lock};

pub const CONFIG_FILE: &str = "bot-config.v3.json";
const LEGACY_CONFIG_FILE: &str = "bot-config.json";
const LEGACY_STATE_FILE: &str = "bot-state.json";
const V2_STATE_FILE: &str = "bot-state.v2.json";
pub const STATE_FILE: &str = "bot-state.v3.json";

async fn read_optional_json(path: &Path) -> Result<Option<Value>> {
    match fs::read_to_string(path).await {
        Ok(text) => {
            Ok(Some(serde_json::from_str(&text).with_context(|| {
                format!("invalid JSON: {}", path.display())
            })?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// `JSON.stringify(value, null, 2) + "\n"`。
pub fn pretty_json(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".into());
    text.push('\n');
    text
}

async fn write_json(path: &Path, value: &Value) -> Result<()> {
    atomic_write_private_text(path, &pretty_json(value)).await
}

#[derive(Clone)]
pub struct BotsRepo {
    dir: PathBuf,
}

impl BotsRepo {
    /// `dir` 是 `getAppConfigDir()`（`<data base>/.zcode/v2`）。
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub async fn read_config(&self) -> Result<Value> {
        let path = self.dir.join(CONFIG_FILE);
        let legacy = self.dir.join(LEGACY_CONFIG_FILE);
        with_file_lock(&path, &LockOptions::default(), async || {
            if let Some(current) = read_optional_json(&path).await? {
                return parse_config_file(&current).map_err(|error| anyhow!(error));
            }
            let imported = match read_optional_json(&legacy).await? {
                Some(legacy) => import_legacy_config(&legacy),
                None => json!({"version": 3, "bots": []}),
            };
            let config = parse_config_file(&imported).map_err(|error| anyhow!(error))?;
            write_json(&path, &config).await?;
            Ok(config)
        })
        .await
    }

    pub async fn write_config(&self, config: &Value) -> Result<Value> {
        let parsed = parse_config_file(config).map_err(|error| anyhow!(error))?;
        let path = self.dir.join(CONFIG_FILE);
        with_file_lock(&path, &LockOptions::default(), async || {
            write_json(&path, &parsed).await
        })
        .await?;
        Ok(parsed)
    }

    pub async fn read_state(&self) -> Result<Value> {
        let path = self.dir.join(STATE_FILE);
        let v2 = self.dir.join(V2_STATE_FILE);
        let legacy = self.dir.join(LEGACY_STATE_FILE);
        with_file_lock(&path, &LockOptions::default(), async || {
            if let Some(current) = read_optional_json(&path).await? {
                return parse_state_file(&current).map_err(|error| anyhow!(error));
            }
            let previous = match read_optional_json(&v2).await? {
                Some(value) => Some(value),
                None => read_optional_json(&legacy).await?,
            };
            let imported = match previous {
                Some(previous) => import_legacy_state(&previous),
                None => json!({"version": 3, "bots": {}}),
            };
            let state = parse_state_file(&imported).map_err(|error| anyhow!(error))?;
            write_json(&path, &state).await?;
            Ok(state)
        })
        .await
    }

    pub async fn write_state(&self, state: &Value) -> Result<Value> {
        let parsed = parse_state_file(state).map_err(|error| anyhow!(error))?;
        let path = self.dir.join(STATE_FILE);
        with_file_lock(&path, &LockOptions::default(), async || {
            write_json(&path, &parsed).await
        })
        .await?;
        Ok(parsed)
    }
}
