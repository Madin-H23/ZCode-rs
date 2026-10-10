//! 与 Main、CLI、Node Host 共享的 JSON 配置文件（主要是 `~/.zcode/cli/config.json`）的读改写
//! （docs/specs/rust-host-extensions.md §3）。
//!
//! 整个“读 → 改 → 写”持跨进程锁 `<file>.lock/`（与 `@zcode/shared/node` 的 `withFileLock`
//! 同一协议），锁内同目录临时文件 + rename 原子替换，权限 0600。读不到文件时 `raw` 为 `None`；
//! 解析与容错语义由调用方决定（各 Node 写入方对损坏 JSON 的处理不同）。
//!
//! 本进程内这些文件的唯一写入方是 [`ConfigWrites`] 的 owner 任务（docs/specs/rust-u2-task-scope.md
//! §6.3）：扩展区各 service 与 settings-sync 共用一个，读改写按提交顺序逐个执行；跨进程互斥由文件锁
//! 负责。写入方放弃等待不打断已经开始的读改写。
use std::path::PathBuf;

use zcode_async::Spawner;
use zcode_host_kit::{RpcError, Serial};

use super::fsx;
use crate::store::{LockOptions, atomic_write_private_text, with_file_lock};

/// 调用方的决定：写回新内容，或不写。
pub enum Update<T> {
    Write(String, T),
    Skip(T),
}

/// 共享 JSON 配置文件的写入方（owner 任务 `extension-config-writes`）。可克隆，所有克隆共用一个 owner。
#[derive(Clone)]
pub struct ConfigWrites(Serial);

impl ConfigWrites {
    pub fn start(spawner: &Spawner) -> Self {
        // 用户操作触发的配置写入；排队超过 32 个时写入方等待。
        Self(Serial::start(spawner, "extension-config-writes", 32))
    }

    /// 持锁读改写。`mutate` 收到当前文本（文件不存在时 `None`），返回要写回的完整文本；它在 owner
    /// 任务里同步执行，不能再向本写入方提交（会等自己）。
    pub async fn update<T, Mutate>(&self, path: &str, mutate: Mutate) -> Result<T, RpcError>
    where
        Mutate: FnOnce(Option<String>) -> Result<Update<T>, RpcError> + Send + 'static,
        T: Send + 'static,
    {
        let path = path.to_owned();
        self.0
            .run(async move { update_locked(&path, mutate).await })
            .await
            .unwrap_or_else(|stopped| Err(fsx::error(stopped.to_string())))
    }
}

async fn update_locked<T>(
    path: &str,
    mutate: impl FnOnce(Option<String>) -> Result<Update<T>, RpcError>,
) -> Result<T, RpcError> {
    let file = PathBuf::from(path);
    let outcome = with_file_lock(&file, &LockOptions::default(), async || {
        let raw = match tokio::fs::read(&file).await {
            Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Ok(Err(fsx::fs_error(&error, "open", path))),
        };
        match mutate(raw) {
            Ok(Update::Write(content, value)) => {
                atomic_write_private_text(&file, &content).await?;
                Ok(Ok(value))
            }
            Ok(Update::Skip(value)) => Ok(Ok(value)),
            Err(error) => Ok(Err(error)),
        }
    })
    .await;
    match outcome {
        Ok(result) => result,
        Err(error) => Err(fsx::error(format!("{error:#}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn concurrent_updates_in_one_process_are_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json").display().to_string();
        let writes = ConfigWrites::start(&zcode_host_base::testing::spawner());
        let updates = (0..20).map(|index| {
            let (path, writes) = (path.clone(), writes.clone());
            async move {
                writes
                    .update(&path, move |raw| {
                        let mut value = raw
                            .as_deref()
                            .and_then(super::super::json::parse_record)
                            .unwrap_or_default();
                        value.insert(format!("k{index}"), serde_json::json!(index));
                        Ok(Update::Write(
                            super::super::json::pretty_line(&serde_json::Value::Object(value)),
                            (),
                        ))
                    })
                    .await
                    .unwrap();
            }
        });
        futures_util::future::join_all(updates).await;
        let text = std::fs::read_to_string(&path).unwrap();
        let value = super::super::json::parse_record(&text).unwrap();
        assert_eq!(value.len(), 20);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
