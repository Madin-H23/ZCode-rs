//! Workspace hook trust store file shared with the Desktop (Node
//! `adapters/src/storage/workspace-hook-trust-store.ts`): lock file, atomic
//! writes, corrupt files moved aside.
use crate::contract::{TrustLoad, TrustStorePort};
use crate::domain::hooks::trust::{self, Record};
use crate::process_clock::{alive, boot_time_ms, process_start_ms};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use zcode_kit::fs::{FileMode, ParentDir, TempName, WriteOptions, atomic_write};

const LOCK_TIMEOUT: Duration = Duration::from_millis(5_000);
const STALE_LOCK_MS: u64 = 30_000;
const LOCK_RETRY: Duration = Duration::from_millis(10);
const RENAME_RETRIES: [Duration; 5] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
];
const START_TIME_TOLERANCE_MS: i64 = 2_000;

/// Lock tokens whose owner is alive in this process. Runtimes in one process
/// share the pid (spec rust-u1-runtime-library L6), so a lock with this pid is
/// live only while its token is registered here.
static HELD: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

fn held() -> std::sync::MutexGuard<'static, HashSet<String>> {
    // 集合只做单次插入/删除，panic 不会留下半更新的数据，中毒后继续使用。
    HELD.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The registration of one lock token; dropping it unregisters the token.
struct Token(String);

impl Token {
    fn register() -> Self {
        let token = uuid::Uuid::new_v4().to_string();
        held().insert(token.clone());
        Self(token)
    }
}

impl Drop for Token {
    fn drop(&mut self) {
        held().remove(&self.0);
    }
}

/// Node `resolveWorkspaceHookTrustStorePath`: `storage.dir` of the user config
/// file only (`~/x`, absolute, or relative to home), default `~/.zcode`.
pub async fn trust_store_path(home: &Path, user_config: &Path) -> Result<PathBuf> {
    let config = match tokio::fs::read_to_string(user_config).await {
        Ok(text) => serde_json::from_str::<Value>(text.as_str()).with_context(|| {
            format!(
                "Unable to read trusted user config for Workspace Hook Trust store: {}",
                user_config.display()
            )
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Value::Null,
        Err(error) => return Err(error).context("Unable to read trusted user config"),
    };
    let configured = config["storage"]["dir"]
        .as_str()
        .map(str::trim)
        .unwrap_or("");
    let root = if configured.is_empty() {
        home.join(".zcode")
    } else if let Some(rest) = configured.strip_prefix("~/") {
        home.join(rest)
    } else {
        // 安全原因：相对 storage.dir 绑定用户目录，不能随 workspace cwd 漂移。
        home.join(configured)
    };
    Ok(root.join("security").join("workspace-hook-trust-v1.json"))
}

pub struct FileTrustStore {
    file: PathBuf,
    lock: PathBuf,
    /// In-process FIFO (tokio's mutex is fair) in front of the lock file.
    queue: tokio::sync::Mutex<()>,
}

impl FileTrustStore {
    pub fn new(file: PathBuf) -> Self {
        let mut lock = file.clone().into_os_string();
        lock.push(".lock");
        Self {
            file,
            lock: lock.into(),
            queue: tokio::sync::Mutex::new(()),
        }
    }

    async fn ensure_directory(&self) -> Result<()> {
        let directory = self.file.parent().context("Trust store has no directory")?;
        tokio::fs::create_dir_all(directory).await?;
        set_mode(directory, 0o700).await?;
        Ok(())
    }

    #[expect(clippy::await_holding_invalid_type, reason = "存量，U2 改为 actor")]
    async fn locked<T>(&self, operation: impl AsyncFnOnce() -> Result<T>) -> Result<T> {
        let _queued = self.queue.lock().await;
        self.ensure_directory().await?;
        let mut guard = LockGuard {
            path: self.lock.clone(),
            token: Some(self.acquire().await?),
        };
        let result = operation().await;
        guard.release().await;
        result
    }

    async fn acquire(&self) -> Result<Token> {
        let started = std::time::Instant::now();
        // 先登记再写锁：同进程的其他 runtime 读到这个 token 时它一定已登记。
        let token = Token::register();
        loop {
            let mut options = tokio::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            match options.open(&self.lock).await {
                Ok(mut handle) => {
                    let owner = format!(
                        "{{\"pid\":{},\"startTime\":{},\"token\":\"{}\"}}\n",
                        std::process::id(),
                        boot_time_ms(),
                        token.0
                    );
                    if let Err(error) = handle.write_all(owner.as_bytes()).await {
                        // 只可能删除自己用 create_new 独占创建的锁，不越权。
                        drop(handle);
                        let _ = tokio::fs::remove_file(&self.lock).await;
                        return Err(error.into());
                    }
                    return Ok(token);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    self.remove_stale().await?;
                    if started.elapsed() >= LOCK_TIMEOUT {
                        bail!(
                            "Timed out acquiring Workspace Hook Trust store lock: {}",
                            self.lock.display()
                        );
                    }
                    tokio::time::sleep(LOCK_RETRY).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Node `removeStaleLock`: only locks older than 30 s whose owner is gone
    /// (dead pid, or a pid reused by another process instance). Locks of this
    /// process are judged by the token registry instead, at any age.
    async fn remove_stale(&self) -> Result<()> {
        // L6 修复：同进程可能同时运行多个 runtime，也可能有已停止或 panic 的 runtime 留下的锁。
        // 旧逻辑把本 pid 的锁一律视为存活，残留锁在进程退出前永远无法回收；现在以本进程登记的
        // token 为准：已注销即无主，立即按 token 回收（核对 token，不误删新持有者的锁）。
        if let Some(owner) = read_owner(&self.lock).await
            && owner.pid == std::process::id() as i64
        {
            if !held().contains(&owner.token) {
                release(&self.lock, &owner.token).await;
            }
            return Ok(());
        }
        let metadata = match tokio::fs::metadata(&self.lock).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let age = metadata
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .map_or(0, |d| d.as_millis() as u64);
        if age <= STALE_LOCK_MS {
            return Ok(());
        }
        let Some(owner) = read_owner(&self.lock).await else {
            return remove(&self.lock).await;
        };
        if !alive(owner.pid) {
            return remove(&self.lock).await;
        }
        let (Some(current), Some(recorded)) = (process_start_ms(owner.pid).await, owner.start)
        else {
            return Ok(());
        };
        if (current - recorded).abs() > START_TIME_TOLERANCE_MS {
            return remove(&self.lock).await;
        }
        Ok(())
    }

    /// Node `readCurrent(recoverCorrupt: true)`.
    async fn read_current(&self) -> Result<TrustLoad> {
        let content = match tokio::fs::read_to_string(&self.file).await {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(TrustLoad::Records(vec![]));
            }
            Err(error) => return Err(error.into()),
        };
        if let Some(records) = trust::parse_store(&content) {
            // 权限加固失败不影响读出的记录；下一次写入会再次尝试。
            let _ = set_mode(&self.file, 0o600).await;
            return Ok(TrustLoad::Records(records));
        }
        let mut aside = self.file.clone().into_os_string();
        aside.push(format!(".corrupt-{}", crate::now()));
        // 改名失败仍按损坏处理（fail-closed），原文件留在原地。
        if tokio::fs::rename(&self.file, &aside).await.is_ok() {
            let _ = set_mode(Path::new(&aside), 0o600).await;
        }
        tracing::warn!(
            target: "zcode::hooks",
            event = "workspace_hook.trust_store_corrupt",
            "Workspace Hook Trust store is corrupt and was moved aside"
        );
        Ok(TrustLoad::Corrupt)
    }

    async fn mutate(&self, update: impl FnOnce(Vec<Record>) -> Vec<Record>) -> Result<Vec<Record>> {
        self.locked(async || {
            let current = match self.read_current().await? {
                TrustLoad::Records(records) => records,
                TrustLoad::Corrupt => vec![],
            };
            let next = update(current);
            self.write(&next).await?;
            Ok(next)
        })
        .await
    }

    /// Node `atomicWrite`: hidden temp file, 0600, rename retried while the
    /// target is briefly busy (zcode-kit also fsyncs the file and directory).
    async fn write(&self, records: &[Record]) -> Result<()> {
        let options = WriteOptions {
            mode: FileMode::Private,
            temp_name: TempName::Hidden,
            // 目录由 locked() 的 ensure_directory 创建并设为 0700。
            parent: ParentDir::Existing,
            rename_retry: &RENAME_RETRIES,
        };
        let content = trust::store_content(records);
        Ok(atomic_write(&self.file, content.as_bytes(), options).await?)
    }
}

#[async_trait]
impl TrustStorePort for FileTrustStore {
    async fn load(&self) -> Result<TrustLoad> {
        self.locked(async || self.read_current().await).await
    }
    async fn grant(&self, records: Vec<Record>) -> Result<Vec<Record>> {
        self.mutate(|mut current| {
            for record in records {
                match current.iter_mut().find(|r| r.key() == record.key()) {
                    Some(slot) => *slot = record,
                    None => current.push(record),
                }
            }
            current
        })
        .await
    }
    async fn revoke(&self, identity: &str, digests: Option<Vec<String>>) -> Result<Vec<Record>> {
        if digests.as_ref().is_some_and(Vec::is_empty) {
            // 空数组既不是“全部”也没有目标，任何 IO 之前拒绝。
            bail!("hookDeclarationDigests must be undefined or non-empty");
        }
        self.mutate(|current| {
            current
                .into_iter()
                .filter(|r| {
                    r.workspace_identity != identity
                        || digests
                            .as_ref()
                            .is_some_and(|d| !d.contains(&r.hook_declaration_digest))
                })
                .collect()
        })
        .await
    }
}

/// The lock file this process created. Released explicitly; a dropped
/// operation (cancelled future) unregisters its token at once, so other
/// owners in this process may reclaim the file even if the spawned removal
/// never runs.
struct LockGuard {
    path: PathBuf,
    token: Option<Token>,
}

impl LockGuard {
    async fn release(&mut self) {
        if let Some(token) = self.token.take() {
            release(&self.path, &token.0).await;
        }
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // 取出的 Token 在本函数结束时注销，删除任务是否还能运行都不影响同进程回收。
        if let Some(token) = self.token.take()
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            let (path, token) = (self.path.clone(), token.0.clone());
            handle.spawn(async move { release(&path, &token).await });
        }
    }
}

/// 释放前核验所有权：锁若已被判过期并易主，删除会破坏新持有者的互斥。
async fn release(path: &Path, token: &str) {
    if read_owner(path).await.is_some_and(|o| o.token == token) {
        let _ = tokio::fs::remove_file(path).await;
    }
}

struct Owner {
    pid: i64,
    token: String,
    start: Option<i64>,
}

async fn read_owner(path: &Path) -> Option<Owner> {
    let value: Value = serde_json::from_str(&tokio::fs::read_to_string(path).await.ok()?).ok()?;
    Some(Owner {
        pid: value["pid"].as_f64()? as i64,
        token: value["token"].as_str()?.to_owned(),
        start: value["startTime"].as_f64().map(|n| n as i64),
    })
}

async fn remove(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

async fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

#[cfg(test)]
#[path = "trust_store_tests.rs"]
mod tests;
