//! 临时文件 + rename 的原子写入（packages/services/src/fs/atomicFileUtils.ts 与
//! packages/shared/src/node/privateFilePersistence.ts）。临时文件放在目标同目录，避免 Windows
//! 跨卷 rename 失败；杀软 / 索引器短暂占用导致的 EPERM / EBUSY / EACCES 按档位重试。
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tokio::fs;

use super::lock::{LockOptions, acquire_file_lock};

const TEXT_RENAME_RETRY_MS: [u64; 7] = [50, 100, 200, 400, 800, 1600, 3200];
const PRIVATE_RENAME_RETRY_MS: [u64; 5] = [50, 100, 200, 400, 800];
const STALE_TEMP: Duration = Duration::from_secs(60);

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn random_suffix() -> String {
    let mut bytes = [0u8; 6];
    let _ = getrandom::fill(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn retryable(error: &std::io::Error) -> bool {
    // EPERM / EACCES 都映射为 PermissionDenied；EBUSY 为 ResourceBusy。
    matches!(
        error.kind(),
        ErrorKind::PermissionDenied | ErrorKind::ResourceBusy
    )
}

async fn rename_with_retry(temp: &Path, target: &Path, delays: &[u64]) -> std::io::Result<()> {
    for attempt in 0.. {
        match fs::rename(temp, target).await {
            Ok(()) => return Ok(()),
            Err(error) => match delays.get(attempt) {
                Some(delay) if retryable(&error) => {
                    tokio::time::sleep(Duration::from_millis(*delay)).await;
                }
                _ => return Err(error),
            },
        }
    }
    unreachable!("rename loop always returns")
}

/// 只清理同一目标、超过一分钟的 `<name>.*.tmp`，不碰其他进程刚创建的临时文件。
async fn cleanup_stale_temps(target: &Path, exclude: &Path) {
    let Some(dir) = target.parent() else {
        return;
    };
    let prefix = format!("{}.", file_name(target));
    let Ok(mut entries) = fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if !name.starts_with(&prefix) || !name.ends_with(".tmp") || path == exclude {
            continue;
        }
        let stale = entry
            .metadata()
            .await
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= STALE_TEMP);
        if stale {
            let _ = fs::remove_file(&path).await;
        }
    }
}

/// `atomicWriteText`：默认先取跨进程锁；调用方已持有同一文件的锁时传 `use_lock: false`。
pub async fn atomic_write_text(target: &Path, content: &str, use_lock: bool) -> Result<()> {
    let dir = target.parent().map(Path::to_path_buf).unwrap_or_default();
    let temp: PathBuf = dir.join(format!(
        "{}.{}.{}.{}.tmp",
        file_name(target),
        std::process::id(),
        now_ms(),
        random_suffix()
    ));
    fs::create_dir_all(&dir).await?;
    let lock = if use_lock {
        Some(acquire_file_lock(target, &LockOptions::default()).await?)
    } else {
        None
    };
    let result: Result<()> = async {
        cleanup_stale_temps(target, &temp).await;
        fs::write(&temp, content).await?;
        rename_with_retry(&temp, target, &TEXT_RENAME_RETRY_MS).await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&temp).await;
    }
    if let Some(lock) = lock {
        lock.release().await;
    }
    result
}

/// `atomicWritePrivateTextFile`：凭据等私密文件，权限 0600，不加锁（调用方持锁）。
pub async fn atomic_write_private_text(target: &Path, content: &str) -> Result<()> {
    let dir = target.parent().map(Path::to_path_buf).unwrap_or_default();
    let temp = dir.join(format!(
        ".{}.{}.{}.{}.tmp",
        file_name(target),
        std::process::id(),
        now_ms(),
        random_suffix()
    ));
    fs::create_dir_all(&dir).await?;
    let result: Result<()> = async {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temp).await?;
        tokio::io::AsyncWriteExt::write_all(&mut file, content.as_bytes()).await?;
        tokio::io::AsyncWriteExt::flush(&mut file).await?;
        drop(file);
        rename_with_retry(&temp, target, &PRIVATE_RENAME_RETRY_MS).await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&temp).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_atomically_and_cleans_only_stale_temps() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("setting.json");
        let fresh_temp = dir.path().join("setting.json.1.2.fresh.tmp");
        std::fs::write(&fresh_temp, "x").unwrap();
        atomic_write_text(&target, "{\"a\":1}", true).await.unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{\"a\":1}");
        assert!(
            fresh_temp.exists(),
            "fresh temp files of other writers stay"
        );
        assert!(!dir.path().join("setting.json.lock").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn private_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("credentials.json");
        atomic_write_private_text(&target, "{}").await.unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
