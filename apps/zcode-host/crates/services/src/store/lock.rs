//! 跨进程锁目录协议（packages/shared/src/node/atomicFileLock.ts、privateFilePersistence.ts）。
//!
//! 锁是 `<file>.lock/` 目录，里面唯一的 `owner-<token>.json` 记录 `{pid, createdAt, token}`。
//! 回收只针对 owner 已退出、或无 PID 且超过 grace 的锁；等到上限仍拿不到就报超时，
//! 绝不越过上限去删别人的锁。进程内先按路径 FIFO 排队，每个进程只让队首竞争 OS 锁。
use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use serde_json::Value;
use tokio::fs;

use super::process;

/// 与 Node `ZCODE_FILE_LOCK_TIMEOUT_ERROR_CODE` 对应。
pub const FILE_LOCK_TIMEOUT_CODE: &str = "ZCODE_FILE_LOCK_TIMEOUT";
const MAX_METADATA_CLOCK_SKEW_MS: f64 = 5.0 * 60_000.0;

#[derive(Debug, Clone)]
pub struct LockOptions {
    pub retry_delays: Vec<Duration>,
    pub ownerless_grace: Duration,
    pub max_wait: Duration,
}

impl Default for LockOptions {
    fn default() -> Self {
        // privateFilePersistence.ts 与 atomicFileUtils.ts 的默认值。
        Self {
            retry_delays: [25, 50, 100, 200, 400]
                .into_iter()
                .map(Duration::from_millis)
                .collect(),
            ownerless_grace: Duration::from_millis(100),
            max_wait: Duration::from_millis(8_000),
        }
    }
}

/// 持有中的锁；`release` 只删除本 writer 的 owner 文件，锁被接管时不会碰到别人的 token。
pub struct FileLock {
    lock_dir: PathBuf,
    owner_file: PathBuf,
    released: bool,
}

impl FileLock {
    pub async fn release(mut self) {
        self.released = true;
        let _ = fs::remove_file(&self.owner_file).await;
        let _ = fs::remove_dir(&self.lock_dir).await;
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        if !self.released {
            // 异常路径（future 被取消）兜底同步清理；正常路径走 release。
            let _ = std::fs::remove_file(&self.owner_file);
            let _ = std::fs::remove_dir(&self.lock_dir);
        }
    }
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64() * 1000.0)
        .unwrap_or_default()
}

fn mtime_ms(metadata: &std::fs::Metadata) -> Option<f64> {
    metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs_f64() * 1000.0)
}

fn parse_timestamp(value: Option<f64>, observed_at: f64) -> Option<f64> {
    value.filter(|value| {
        value.is_finite() && *value >= 0.0 && *value <= observed_at + MAX_METADATA_CLOCK_SKEW_MS
    })
}

struct Metadata {
    created_at: Option<f64>,
    pid: Option<u32>,
}

fn parse_metadata(raw: &str, observed_at: f64) -> Metadata {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return Metadata {
            created_at: None,
            pid: None,
        };
    };
    // 0、负数、小数或超出范围的 PID 没有 owner 语义（与 Node 的 isSafeInteger && > 0 一致）。
    let pid = value
        .get("pid")
        .and_then(Value::as_u64)
        .filter(|pid| *pid > 0)
        .and_then(|pid| u32::try_from(pid).ok());
    Metadata {
        created_at: parse_timestamp(value.get("createdAt").and_then(Value::as_f64), observed_at),
        pid,
    }
}

/// 双重非法时间戳只能从当前锁实例首次被看见时起算 grace；实例被替换后重新计时。
#[derive(Default)]
struct InstanceObserver {
    observations: HashMap<PathBuf, (f64, String)>,
}

impl InstanceObserver {
    fn observe(&mut self, path: &Path, metadata: &std::fs::Metadata, observed_at: f64) -> f64 {
        let identity = instance_identity(metadata);
        if let Some((first, previous)) = self.observations.get(path)
            && *previous == identity
        {
            return *first;
        }
        self.observations
            .insert(path.to_owned(), (observed_at, identity));
        observed_at
    }
}

#[cfg(unix)]
fn instance_identity(metadata: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!(
        "{}:{}:{:?}",
        metadata.dev(),
        metadata.ino(),
        mtime_ms(metadata)
    )
}

#[cfg(not(unix))]
fn instance_identity(metadata: &std::fs::Metadata) -> String {
    format!("{:?}:{:?}", metadata.created().ok(), mtime_ms(metadata))
}

async fn owner_reclaimable(
    owner_file: &Path,
    ownerless_grace: Duration,
    observer: &mut InstanceObserver,
) -> std::io::Result<bool> {
    let observed_at = now_ms();
    let raw = fs::read_to_string(owner_file).await?;
    let metadata = parse_metadata(&raw, observed_at);
    let created_at = match metadata.created_at {
        Some(created_at) => created_at,
        None => {
            let stat = fs::metadata(owner_file).await?;
            parse_timestamp(mtime_ms(&stat), observed_at)
                .unwrap_or_else(|| observer.observe(owner_file, &stat, observed_at))
        }
    };
    let owner_exited = metadata.pid.is_some_and(|pid| !process::is_alive(pid));
    let ownerless_stale = metadata.pid.is_none()
        && observed_at - created_at >= ownerless_grace.as_secs_f64() * 1000.0;
    Ok(owner_exited || ownerless_stale)
}

fn is_owner_entry(name: &str) -> bool {
    name.starts_with("owner-") && name.ends_with(".json")
}

async fn list_dir(path: &Path) -> std::io::Result<Vec<String>> {
    let mut entries = Vec::new();
    let mut reader = fs::read_dir(path).await?;
    while let Some(entry) = reader.next_entry().await? {
        entries.push(entry.file_name().to_string_lossy().into_owned());
    }
    Ok(entries)
}

enum Removal {
    Removed,
    Kept,
    Failed(std::io::Error),
}

async fn remove_abandoned(
    lock_dir: &Path,
    ownerless_grace: Duration,
    observer: &mut InstanceObserver,
) -> Removal {
    let result: std::io::Result<bool> = async {
        let stat = fs::metadata(lock_dir).await?;
        if stat.is_dir() {
            let entries = list_dir(lock_dir).await?;
            let owners: Vec<&String> = entries.iter().filter(|name| is_owner_entry(name)).collect();
            if owners.len() == 1 {
                let owner_file = lock_dir.join(owners[0]);
                if !owner_reclaimable(&owner_file, ownerless_grace, observer).await? {
                    return Ok(false);
                }
                // 按唯一 owner 文件删除即所有权校验：锁被替换后新 owner 文件名不同，rmdir 会失败。
                let _ = fs::remove_file(&owner_file).await;
                fs::remove_dir(lock_dir).await?;
                return Ok(true);
            }
            let observed_at = now_ms();
            let directory_timestamp = parse_timestamp(mtime_ms(&stat), observed_at)
                .unwrap_or_else(|| observer.observe(lock_dir, &stat, observed_at));
            if observed_at - directory_timestamp < ownerless_grace.as_secs_f64() * 1000.0 {
                return Ok(false);
            }
            for owner in &owners {
                if !owner_reclaimable(&lock_dir.join(owner), ownerless_grace, observer).await? {
                    return Ok(false);
                }
            }
            // 空目录（mkdir 后崩溃）或多 owner 的损坏目录：只删观察到的条目，新增条目会让 rmdir 失败。
            for entry in &entries {
                let _ = fs::remove_file(lock_dir.join(entry)).await;
            }
            fs::remove_dir(lock_dir).await?;
            return Ok(true);
        }
        // 兼容旧版单文件锁。
        let raw = fs::read_to_string(lock_dir).await?;
        if !owner_reclaimable(lock_dir, ownerless_grace, observer).await? {
            return Ok(false);
        }
        if fs::read_to_string(lock_dir).await? != raw {
            return Ok(false);
        }
        fs::remove_file(lock_dir).await?;
        Ok(true)
    }
    .await;
    match result {
        Ok(true) => Removal::Removed,
        Ok(false) => Removal::Kept,
        Err(error)
            if error.kind() == ErrorKind::NotFound
                || error.kind() == ErrorKind::DirectoryNotEmpty =>
        {
            Removal::Kept
        }
        Err(error) => Removal::Failed(error),
    }
}

fn random_base36() -> String {
    let mut bytes = [0u8; 8];
    let _ = getrandom::fill(&mut bytes);
    let mut value = u64::from_le_bytes(bytes);
    let mut out = String::new();
    while value > 0 {
        let digit = (value % 36) as u32;
        out.push(char::from_digit(digit, 36).unwrap_or('0'));
        value /= 36;
    }
    if out.is_empty() {
        out.push('0');
    }
    out
}

fn timeout_error(lock_dir: &Path, waited: Duration) -> anyhow::Error {
    anyhow!(
        "{FILE_LOCK_TIMEOUT_CODE}: Timed out after {}ms waiting for the ZCode file lock: {}",
        waited.as_millis(),
        lock_dir.display()
    )
}

fn is_permission_error(error: &std::io::Error) -> bool {
    error.kind() == ErrorKind::PermissionDenied
}

pub async fn acquire_file_lock(file: &Path, options: &LockOptions) -> Result<FileLock> {
    let lock_dir = PathBuf::from(format!("{}.lock", file.display()));
    let token = format!(
        "{}-{}-{}",
        std::process::id(),
        now_ms() as u64,
        random_base36()
    );
    let owner_file = lock_dir.join(format!("owner-{token}.json"));
    let payload = format!(
        "{}\n",
        serde_json::json!({"pid": std::process::id(), "createdAt": now_ms() as u64, "token": token})
    );
    let ownerless_grace = options.ownerless_grace.min(options.max_wait / 2);
    let started = Instant::now();
    let mut observer = InstanceObserver::default();
    let mut last_removal_error: Option<std::io::Error> = None;

    for attempt in 0usize.. {
        let mut created = false;
        let outcome: std::io::Result<()> = async {
            fs::create_dir(&lock_dir).await?;
            created = true;
            let created_stat = fs::metadata(&lock_dir).await?;
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            let mut owner = options.open(&owner_file).await?;
            tokio::io::AsyncWriteExt::write_all(&mut owner, payload.as_bytes()).await?;
            drop(owner);
            let current_stat = fs::metadata(&lock_dir).await?;
            let owners: Vec<String> = list_dir(&lock_dir)
                .await?
                .into_iter()
                .filter(|name| is_owner_entry(name))
                .collect();
            let expected = format!("owner-{token}.json");
            if instance_identity_key(&created_stat) != instance_identity_key(&current_stat)
                || owners.len() != 1
                || owners[0] != expected
            {
                return Err(std::io::Error::new(
                    ErrorKind::AlreadyExists,
                    "ZCode file lock ownership changed during acquire",
                ));
            }
            Ok(())
        }
        .await;
        let error = match outcome {
            Ok(()) => {
                return Ok(FileLock {
                    lock_dir,
                    owner_file,
                    released: false,
                });
            }
            Err(error) => error,
        };
        if created {
            let _ = fs::remove_file(&owner_file).await;
            let _ = fs::remove_dir(&lock_dir).await;
        }
        let lost_created = created && error.kind() == ErrorKind::NotFound;
        if error.kind() != ErrorKind::AlreadyExists && !lost_created {
            return Err(error.into());
        }
        let elapsed = started.elapsed();
        if elapsed >= options.max_wait {
            if let Some(removal) = last_removal_error.take()
                && is_permission_error(&removal)
            {
                return Err(removal.into());
            }
            return Err(timeout_error(&lock_dir, elapsed));
        }
        match remove_abandoned(&lock_dir, ownerless_grace, &mut observer).await {
            Removal::Removed => continue,
            Removal::Kept => {}
            Removal::Failed(error) => last_removal_error = Some(error),
        }
        let remaining = options.max_wait.saturating_sub(elapsed);
        if options.retry_delays.is_empty() || remaining.is_zero() {
            if let Some(removal) = last_removal_error.take()
                && is_permission_error(&removal)
            {
                return Err(removal.into());
            }
            return Err(timeout_error(&lock_dir, elapsed));
        }
        let delay = options.retry_delays[attempt.min(options.retry_delays.len() - 1)];
        tokio::time::sleep(delay.min(remaining)).await;
    }
    unreachable!("lock acquisition loop always returns")
}

#[cfg(unix)]
fn instance_identity_key(metadata: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn instance_identity_key(_metadata: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

type Tail = Arc<tokio::sync::Mutex<()>>;

fn process_tail(file: &Path) -> Tail {
    static TAILS: OnceLock<Mutex<HashMap<PathBuf, Tail>>> = OnceLock::new();
    let tails = TAILS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut tails = tails
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    tails.entry(file.to_owned()).or_default().clone()
}

/// 在跨进程锁内执行 `operation`。同一进程内对同一路径先 FIFO 排队（tokio Mutex 公平），
/// 避免几十个调用方同时轮询目录锁形成惊群。
pub async fn with_file_lock<T>(
    file: &Path,
    options: &LockOptions,
    operation: impl AsyncFnOnce() -> Result<T>,
) -> Result<T> {
    let tail = process_tail(file);
    let _queue = tail.lock().await;
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent).await?;
    }
    let lock = acquire_file_lock(file, options).await?;
    let result = operation().await;
    lock.release().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fast() -> LockOptions {
        LockOptions {
            retry_delays: vec![Duration::from_millis(10)],
            ownerless_grace: Duration::from_millis(50),
            max_wait: Duration::from_millis(400),
        }
    }

    #[tokio::test]
    async fn owner_file_matches_node_format_and_is_removed_on_release() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("setting.json");
        let lock = acquire_file_lock(&file, &fast()).await.unwrap();
        let lock_dir = dir.path().join("setting.json.lock");
        let owners = list_dir(&lock_dir).await.unwrap();
        assert_eq!(owners.len(), 1);
        let raw = std::fs::read_to_string(lock_dir.join(&owners[0])).unwrap();
        assert!(raw.ends_with('\n'));
        let value: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["pid"], std::process::id());
        assert!(owners[0].contains(value["token"].as_str().unwrap()));
        let keys: Vec<&String> = value.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["pid", "createdAt", "token"]);
        lock.release().await;
        assert!(!lock_dir.exists());
    }

    #[tokio::test]
    async fn live_owner_blocks_until_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.json");
        let held = acquire_file_lock(&file, &fast()).await.unwrap();
        let error = acquire_file_lock(&file, &fast()).await.err().unwrap();
        assert!(error.to_string().contains(FILE_LOCK_TIMEOUT_CODE));
        held.release().await;
        acquire_file_lock(&file, &fast())
            .await
            .unwrap()
            .release()
            .await;
    }

    #[tokio::test]
    async fn dead_owner_and_stale_ownerless_locks_are_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("b.json");
        let lock_dir = dir.path().join("b.json.lock");
        std::fs::create_dir(&lock_dir).unwrap();
        // 不存在的进程（PID 上限之外）持有的锁可以回收。
        std::fs::write(
            lock_dir.join("owner-dead.json"),
            r#"{"pid":2147483000,"createdAt":1,"token":"dead"}"#,
        )
        .unwrap();
        acquire_file_lock(&file, &fast())
            .await
            .unwrap()
            .release()
            .await;

        // 空锁目录（mkdir 后崩溃）超过 grace 后回收。
        std::fs::create_dir(&lock_dir).unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        acquire_file_lock(&file, &fast())
            .await
            .unwrap()
            .release()
            .await;
    }

    #[tokio::test]
    async fn with_file_lock_serializes_read_modify_write() {
        let dir = tempfile::tempdir().unwrap();
        let file = Arc::new(dir.path().join("counter.json"));
        std::fs::write(file.as_path(), "0").unwrap();
        let mut tasks = Vec::new();
        for _ in 0..20 {
            let file = file.clone();
            tasks.push(tokio::spawn(async move {
                with_file_lock(&file, &LockOptions::default(), || async {
                    let value: u32 = fs::read_to_string(file.as_path()).await?.parse()?;
                    tokio::task::yield_now().await;
                    fs::write(file.as_path(), (value + 1).to_string()).await?;
                    Ok(())
                })
                .await
                .unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(std::fs::read_to_string(file.as_path()).unwrap(), "20");
    }
}
