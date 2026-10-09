//! 临时文件 + rename 的原子写入（packages/services/src/fs/atomicFileUtils.ts 与
//! packages/shared/src/node/privateFilePersistence.ts）。写入本身由 `zcode_kit::fs::atomic_write`
//! 完成（同目录临时文件、fsync、rename 重试、失败删除临时文件）；这里保留 Node 对应实现的
//! 临时文件命名、权限、重试档位，以及 `atomicWriteText` 的跨进程锁与过期临时文件清理。
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use tokio::fs;
use zcode_kit::fs::{FileMode, ParentDir, TempName, WriteOptions, atomic_write};

use super::lock::{LockOptions, acquire_file_lock};

const TEXT_RENAME_RETRY: [Duration; 7] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
    Duration::from_millis(1600),
    Duration::from_millis(3200),
];
const PRIVATE_RENAME_RETRY: [Duration; 5] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
];
const STALE_TEMP: Duration = Duration::from_secs(60);

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 只清理同一目标、超过一分钟的 `<name>.*.tmp`，不碰其他进程刚创建的临时文件。
/// 在本次写入创建临时文件之前调用，所以不会删到自己的临时文件。
async fn cleanup_stale_temps(target: &Path) {
    let Some(dir) = target.parent() else {
        return;
    };
    let prefix = format!("{}.", file_name(target));
    let Ok(mut entries) = fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&prefix) || !name.ends_with(".tmp") {
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
            let _ = fs::remove_file(entry.path()).await;
        }
    }
}

/// `atomicWriteText`：默认先取跨进程锁；调用方已持有同一文件的锁时传 `use_lock: false`。
/// 临时文件用可见名 `<name>.<pid>.<ms>.<rand>.tmp`，与 Node 的过期清理规则对应。
pub async fn atomic_write_text(target: &Path, content: &str, use_lock: bool) -> Result<()> {
    let dir = target.parent().map(Path::to_path_buf).unwrap_or_default();
    fs::create_dir_all(&dir).await?;
    let lock = if use_lock {
        Some(acquire_file_lock(target, &LockOptions::default()).await?)
    } else {
        None
    };
    cleanup_stale_temps(target).await;
    let options = WriteOptions {
        mode: FileMode::Default,
        temp_name: TempName::Visible,
        parent: ParentDir::Existing,
        rename_retry: &TEXT_RENAME_RETRY,
    };
    let result = atomic_write(target, content.as_bytes(), options).await;
    if let Some(lock) = lock {
        lock.release().await;
    }
    Ok(result?)
}

/// `atomicWritePrivateTextFile`：凭据等私密文件，隐藏临时文件名，权限 0600，不加锁（调用方持锁）。
pub async fn atomic_write_private_text(target: &Path, content: &str) -> Result<()> {
    let options = WriteOptions {
        mode: FileMode::Private,
        temp_name: TempName::Hidden,
        parent: ParentDir::Create,
        rename_retry: &PRIVATE_RENAME_RETRY,
    };
    Ok(atomic_write(target, content.as_bytes(), options).await?)
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
