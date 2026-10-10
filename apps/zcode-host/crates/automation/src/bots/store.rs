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
use zcode_async::Spawner;
use zcode_host_kit::Serial;

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
    /// 配置与状态文件在本进程内的唯一写入方：每次持跨进程锁的读写在这个 owner 任务里按提交顺序执行
    /// （docs/specs/rust-u2-task-scope.md §6.3），游标等状态按写入顺序落盘。本地与远端 attachment 的
    /// Bots 共用同一个 BotsRepo。
    files: Serial,
}

impl BotsRepo {
    /// `dir` 是 `getAppConfigDir()`（`<data base>/.zcode/v2`）；文件 IO 的 owner 任务在 `spawner` 的作用域里。
    pub fn new(spawner: &Spawner, dir: PathBuf) -> Self {
        Self {
            dir,
            // 轮询游标与配置写入；排队超过 64 个时提交方等待。
            files: Serial::start(spawner, "bot-store-files", 64),
        }
    }

    /// 在 owner 任务里执行一次文件操作。
    async fn in_files<T, Op, Fut>(&self, operation: Op) -> Result<T>
    where
        Op: FnOnce(PathBuf) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<T>> + Send,
        T: Send + 'static,
    {
        let dir = self.dir.clone();
        self.files
            .run(async move { operation(dir).await })
            .await
            .map_err(|stopped| anyhow!(stopped))?
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub async fn read_config(&self) -> Result<Value> {
        self.in_files(|dir| async move {
            let path = dir.join(CONFIG_FILE);
            let legacy = dir.join(LEGACY_CONFIG_FILE);
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
        })
        .await
    }

    pub async fn write_config(&self, config: &Value) -> Result<Value> {
        let parsed = parse_config_file(config).map_err(|error| anyhow!(error))?;
        let written = parsed.clone();
        self.in_files(|dir| async move {
            let path = dir.join(CONFIG_FILE);
            with_file_lock(&path, &LockOptions::default(), async || {
                write_json(&path, &written).await
            })
            .await
        })
        .await?;
        Ok(parsed)
    }

    pub async fn read_state(&self) -> Result<Value> {
        self.in_files(|dir| async move {
            let path = dir.join(STATE_FILE);
            let v2 = dir.join(V2_STATE_FILE);
            let legacy = dir.join(LEGACY_STATE_FILE);
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
        })
        .await
    }

    pub async fn write_state(&self, state: &Value) -> Result<Value> {
        let parsed = parse_state_file(state).map_err(|error| anyhow!(error))?;
        let written = parsed.clone();
        self.in_files(|dir| async move {
            let path = dir.join(STATE_FILE);
            with_file_lock(&path, &LockOptions::default(), async || {
                write_json(&path, &written).await
            })
            .await
        })
        .await?;
        Ok(parsed)
    }
}
