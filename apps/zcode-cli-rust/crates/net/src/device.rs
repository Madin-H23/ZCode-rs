//! Device id shared with Desktop through `telemetry-state.json`
//! (Node `adapters/src/device/cli-device-mid.ts`). Any failure falls back to an
//! id that is valid for this process only; the caller caches the result.
use serde_json::{Map, Value};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex, PoisonError},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zcode_kit::fs::{FileMode, ParentDir, TempName, WriteOptions, atomic_write};

const LOCK_RETRY_DELAY: Duration = Duration::from_millis(10);
const LOCK_RETRY_COUNT: usize = 200;
const LOCK_STALE: Duration = Duration::from_secs(5 * 60);

/// Lock tokens held by a live owner in this process. Several runtimes share
/// the pid (spec rust-u1-runtime-library L6), so a lock with this pid is live
/// only while its token is registered here.
static HELD: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

fn held() -> std::sync::MutexGuard<'static, HashSet<String>> {
    // 集合只做单次插入/删除，panic 不会留下半更新的数据，中毒后继续使用。
    HELD.lock().unwrap_or_else(PoisonError::into_inner)
}

/// This process's registration of one lock token; dropping it (including a
/// cancelled or panicking owner) unregisters the token.
pub(crate) struct Token(pub(crate) String);

impl Token {
    pub(crate) fn register() -> Self {
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

/// `${ZCODE_DATA_BASE_DIR || home}/.zcode/v2/telemetry-state.json`.
pub fn state_file(base_dir: Option<&str>, home: &Path, cwd: &Path) -> PathBuf {
    let base = base_dir.map(str::trim).filter(|b| !b.is_empty());
    let base = match base {
        None | Some("~") => home.to_path_buf(),
        Some(path) => match path.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => cwd.join(path),
        },
    };
    base.join(".zcode").join("v2").join("telemetry-state.json")
}

/// Node `ensureCliDeviceMid` without the process cache.
pub async fn ensure(state_file: &Path) -> String {
    let generated = uuid::Uuid::new_v4().to_string();
    match persisted(state_file, &generated).await {
        Ok(id) => id,
        Err(error) => {
            tracing::warn!(
                target: "zcode::net",
                event = "device.persist_failed",
                error = %error,
                "Device id is not persisted; using a process-local id"
            );
            generated
        }
    }
}

fn device_id(state: &Map<String, Value>) -> Option<String> {
    state
        .get("deviceMid")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

async fn read_state(path: &Path) -> Map<String, Value> {
    match tokio::fs::read(path)
        .await
        .map(|b| serde_json::from_slice(&b))
    {
        Ok(Ok(Value::Object(state))) => state,
        _ => Map::new(),
    }
}

async fn persisted(state_file: &Path, generated: &str) -> std::io::Result<String> {
    if let Some(id) = device_id(&read_state(state_file).await) {
        return Ok(id);
    }
    let directory = state_file.parent().unwrap_or(Path::new("."));
    tokio::fs::create_dir_all(directory).await?;
    let lock = directory.join("telemetry-state.lock");
    // 先登记再写锁：同进程的其他 runtime 读到这个 token 时它一定已登记。
    let token = Token::register();
    for _ in 0..LOCK_RETRY_COUNT {
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
            .await
        {
            Ok(mut handle) => {
                let result = locked(state_file, &mut handle, (generated, &token.0)).await;
                drop(handle);
                let _ = tokio::fs::remove_file(&lock).await;
                return result;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if !remove_stale_lock(&lock).await {
                    tokio::time::sleep(LOCK_RETRY_DELAY).await;
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::other("CLI telemetry state lock timeout"))
}

async fn locked(
    state_file: &Path,
    handle: &mut tokio::fs::File,
    (generated, token): (&str, &str),
) -> std::io::Result<String> {
    use tokio::io::AsyncWriteExt;
    // `token` 只供同进程区分 owner；Node 只读 createdAt 与 pid，多出的字段不影响它。
    let owner =
        serde_json::json!({"createdAt": now_ms(), "pid": std::process::id(), "token": token});
    handle.write_all(owner.to_string().as_bytes()).await?;
    let mut state = read_state(state_file).await;
    if let Some(id) = device_id(&state) {
        return Ok(id);
    }
    state.insert("deviceMid".into(), generated.into());
    write_state(state_file, &state).await?;
    Ok(generated.to_owned())
}

/// Atomic replace so Desktop never reads a torn file. Hidden temp name and
/// default permissions as before; the directory is created by [`persisted`].
async fn write_state(path: &Path, state: &Map<String, Value>) -> std::io::Result<()> {
    let body = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
    let options = WriteOptions {
        mode: FileMode::Default,
        temp_name: TempName::Hidden,
        parent: ParentDir::Existing,
        rename_retry: &[],
    };
    atomic_write(path, &body, options).await
}

/// Node `removeStaleTelemetryLockIfNeeded`.
async fn remove_stale_lock(lock: &Path) -> bool {
    let Ok(metadata) = tokio::fs::metadata(lock).await else {
        return false;
    };
    let age = metadata
        .modified()
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .unwrap_or_default();
    if age < LOCK_STALE {
        let owner = tokio::fs::read(lock)
            .await
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .filter(|o| o["createdAt"].is_number());
        let pid = owner.as_ref().and_then(|o| o["pid"].as_f64());
        match pid {
            None => return false,
            // L6 修复：同进程的多个 runtime 共用 pid，“pid 存活”不能说明 owner 存活。被取消或
            // panic 的 owner 留下的锁曾要等 5 分钟才过期；现在以本进程登记的 token 为准。
            Some(pid) if pid == f64::from(std::process::id()) => {
                let token = owner.as_ref().and_then(|o| o["token"].as_str());
                if token.is_some_and(|t| held().contains(t)) {
                    return false;
                }
            }
            Some(pid) if process_alive(pid) => return false,
            Some(_) => {}
        }
    }
    let _ = tokio::fs::remove_file(lock).await;
    true
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Node `isProcessAlive` (`process.kill(pid, 0)`; EPERM counts as alive).
fn process_alive(pid: f64) -> bool {
    if pid.fract() != 0.0 || pid <= 0.0 || pid > f64::from(u32::MAX) {
        return false;
    }
    alive(pid as u32)
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: 信号 0 只做存在性与权限检查，不向进程发送信号。
    let delivered = unsafe { libc::kill(pid, 0) } == 0;
    delivered || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn alive(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_ACCESS_DENIED, GetLastError, STILL_ACTIVE},
        System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    // SAFETY: 句柄只在本函数内使用并关闭；与 libuv `uv_kill(pid, 0)` 的判定一致。
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        let mut code = 0u32;
        let running = GetExitCodeProcess(handle, &mut code) != 0 && code == STILL_ACTIVE as u32;
        CloseHandle(handle);
        running
    }
}

#[cfg(not(any(unix, windows)))]
fn alive(_pid: u32) -> bool {
    true
}
