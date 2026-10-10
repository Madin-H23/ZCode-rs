//! Bot 通道的跨进程运行锁（bots/channelRuntime.ts `acquireBotRuntimeLock`）：保证同一个飞书应用
//! 在所有 ZCode 进程（Node Host、Rust Host、多个窗口）里只建立一条长连接。
//!
//! 锁目录 `~/.zcode/v2/bots-runtime-locks/<namespace>/<sha256(key)>.lock/`：先在 `.pending` 目录写好
//! `owner.json` 与 `lease-<nonce>`，再 rename 成锁目录；持有者每 10 秒 touch 租约文件。已有锁的持有者
//! 进程存活且租约 30 秒内更新过时让出（返回 None），否则清掉旧锁重试一次。
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Result;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::fs;
use zcode_async::OwnedTask;

pub const RETRY_MS: u64 = 10_000;
const LEASE_MS: u128 = 30_000;
const HEARTBEAT: Duration = Duration::from_secs(10);
const CLEANUP_RETRY_MS: [u64; 3] = [100, 250, 500];

pub struct RuntimeLock {
    path: PathBuf,
    nonce: String,
    bot_id: String,
    /// 续租任务由锁持有：锁被释放或丢弃时随之中止。
    heartbeat: OwnedTask<()>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn random_hex(len: usize) -> String {
    let mut bytes = vec![0u8; len];
    let _ = getrandom::fill(&mut bytes);
    hex(&bytes)
}

pub fn lock_path(dir: &Path, namespace: &str, key: &str) -> PathBuf {
    let digest = Sha256::digest(key.trim().as_bytes());
    dir.join("bots-runtime-locks")
        .join(namespace)
        .join(format!("{}.lock", hex(&digest)))
}

async fn remove_lock_path(path: &Path) -> std::io::Result<()> {
    let mut attempt = 0;
    loop {
        let result = match fs::metadata(path).await {
            Ok(meta) if meta.is_dir() => fs::remove_dir_all(path).await,
            Ok(_) => fs::remove_file(path).await,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                let retryable = matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied
                        | std::io::ErrorKind::ResourceBusy
                        | std::io::ErrorKind::DirectoryNotEmpty
                );
                let Some(delay) = CLEANUP_RETRY_MS.get(attempt).filter(|_| retryable) else {
                    return Err(error);
                };
                tokio::time::sleep(Duration::from_millis(*delay)).await;
                attempt += 1;
            }
        }
    }
}

async fn read_owner(path: &Path) -> Option<(u32, String, String)> {
    let text = fs::read_to_string(path.join("owner.json")).await.ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let pid = value["pid"].as_f64()?;
    Some((
        pid as u32,
        value["botId"].as_str()?.to_owned(),
        value["nonce"].as_str()?.to_owned(),
    ))
}

async fn lease_age_ms(path: &Path, nonce: &str) -> Option<u128> {
    let modified = fs::metadata(path.join(format!("lease-{nonce}")))
        .await
        .ok()?
        .modified()
        .ok()?;
    Some(
        SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default()
            .as_millis(),
    )
}

/// `acquireBotRuntimeLock`：拿到返回 Some；他人持有返回 None。
pub async fn acquire(
    dir: &Path,
    namespace: &str,
    key: &str,
    bot_id: &str,
) -> Result<Option<RuntimeLock>> {
    let path = lock_path(dir, namespace, key);
    let nonce = random_hex(8);
    let owner = json!({
        "pid": std::process::id(),
        "botId": bot_id,
        "nonce": nonce,
        "createdAt": crate::bots::now_ms(),
    });
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    for _ in 0..2 {
        let pending = PathBuf::from(format!("{}.{nonce}.pending", path.display()));
        let attempt: Result<Option<bool>> = async {
            fs::create_dir(&pending).await?;
            fs::write(pending.join("owner.json"), format!("{owner}\n")).await?;
            fs::write(pending.join(format!("lease-{nonce}")), "").await?;
            match fs::rename(&pending, &path).await {
                Ok(()) => Ok(Some(true)),
                Err(error) => {
                    if fs::metadata(&path).await.is_err() {
                        return Err(error.into());
                    }
                    if let Some((pid, _, owner_nonce)) = read_owner(&path).await
                        && crate::store::is_alive(pid)
                        && lease_age_ms(&path, &owner_nonce)
                            .await
                            .is_some_and(|age| age < LEASE_MS)
                    {
                        return Ok(None);
                    }
                    remove_lock_path(&path).await?;
                    Ok(Some(false))
                }
            }
        }
        .await;
        let _ = remove_lock_path(&pending).await;
        match attempt? {
            None => return Ok(None),
            Some(false) => continue,
            Some(true) => {
                let lease = path.join(format!("lease-{nonce}"));
                let heartbeat = zcode_async::owned(async move {
                    loop {
                        tokio::time::sleep(HEARTBEAT).await;
                        let lease = lease.clone();
                        let _ = zcode_async::blocking(move || {
                            std::fs::File::options()
                                .write(true)
                                .open(&lease)
                                .and_then(|file| file.set_modified(SystemTime::now()))
                        })
                        .await;
                    }
                });
                return Ok(Some(RuntimeLock {
                    path,
                    nonce,
                    bot_id: bot_id.to_owned(),
                    heartbeat,
                }));
            }
        }
    }
    Ok(None)
}

impl RuntimeLock {
    /// 只删除仍属于自己的锁目录。
    pub async fn release(self) {
        self.heartbeat.abort();
        if let Some((pid, bot_id, nonce)) = read_owner(&self.path).await
            && pid == std::process::id()
            && bot_id == self.bot_id
            && nonce == self.nonce
        {
            let _ = remove_lock_path(&self.path).await;
        }
    }
}
