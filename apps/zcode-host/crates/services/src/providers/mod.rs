//! `provider-settings` 与 `model-selection` channel（packages/services/src/model-provider/*）。
//!
//! 数据源：Main 在 InitLocal 给的内置配置文件，与 `<appConfigDir>/provider_config.json`。
//! 两个文件内容变化（每秒轮询，与 TS 的 personal 文件轮询节奏相同）时重新解析，revision 加一，
//! 并触发两个 channel 的 `onDidChange`。未登录：account provider 一律不可用。
pub mod resolve;

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};
use zcode_host_kit::{EventHub, Reply, RpcError, ServiceBuilder, log};

use resolve::Resolved;

const POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct ProviderRuntime {
    inner: Arc<Inner>,
}

struct Inner {
    builtin_file: Option<PathBuf>,
    personal_file: PathBuf,
    state: RwLock<State>,
    changed: EventHub<()>,
}

#[derive(Default)]
struct State {
    signature: Vec<u8>,
    revision: u64,
    resolved: Arc<Resolved>,
}

async fn read_json(path: &Path) -> (Vec<u8>, Value) {
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            let value = serde_json::from_slice::<Value>(&bytes).unwrap_or_else(|error| {
                log::warn(format!(
                    "invalid provider config {}: {error}",
                    path.display()
                ));
                Value::Null
            });
            (bytes, value)
        }
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn(format!(
                    "cannot read provider config {}: {error}",
                    path.display()
                ));
            }
            (Vec::new(), Value::Null)
        }
    }
}

impl ProviderRuntime {
    pub fn new(builtin_file: Option<PathBuf>, personal_file: PathBuf) -> Self {
        Self {
            inner: Arc::new(Inner {
                builtin_file,
                personal_file,
                state: RwLock::new(State::default()),
                changed: EventHub::new(),
            }),
        }
    }

    /// 给 CLI 的环境：`ZCODE_BUILTIN_PROVIDER_CONFIG_FILE` / `ZCODE_PERSONAL_PROVIDER_CONFIG_FILE`。
    pub fn builtin_file(&self) -> Option<&Path> {
        self.inner.builtin_file.as_deref()
    }

    pub fn personal_file(&self) -> &Path {
        &self.inner.personal_file
    }

    /// 重新读取两个文件；内容变化时重新解析并通知。返回当前快照。
    pub async fn reload(&self) -> (u64, Arc<Resolved>) {
        let (builtin_bytes, builtin) = match &self.inner.builtin_file {
            Some(path) => read_json(path).await,
            None => (Vec::new(), Value::Null),
        };
        let (personal_bytes, personal) = read_json(&self.inner.personal_file).await;
        let mut signature = builtin_bytes;
        signature.push(0);
        signature.extend_from_slice(&personal_bytes);
        {
            let state = self.lock_read();
            if state.revision > 0 && state.signature == signature {
                return (state.revision, state.resolved.clone());
            }
        }
        // 格式不对（schemaVersion 不是 1）的文件按空配置处理，与 TS 仓库读取失败时一致。
        let config_of = |value: &Value| {
            if value["schemaVersion"] == 1 {
                value["config"].clone()
            } else {
                json!({})
            }
        };
        let (builtin, personal) = (config_of(&builtin), config_of(&personal));
        let resolved = tokio::task::spawn_blocking(move || resolve::resolve(&builtin, &personal))
            .await
            .unwrap_or_default();
        let snapshot = {
            let mut state = self.lock_write();
            state.signature = signature;
            state.revision += 1;
            state.resolved = Arc::new(resolved);
            (state.revision, state.resolved.clone())
        };
        if snapshot.0 > 1 {
            self.inner.changed.emit(());
        }
        snapshot
    }

    pub async fn snapshot(&self) -> (u64, Arc<Resolved>) {
        {
            let state = self.lock_read();
            if state.revision > 0 {
                return (state.revision, state.resolved.clone());
            }
        }
        self.reload().await
    }

    /// 后台轮询文件变化；Host 退出时随运行时一起结束。
    pub fn start_polling(&self) {
        let runtime = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(POLL_INTERVAL).await;
                runtime.reload().await;
            }
        });
    }

    pub fn changes(&self) -> EventHub<()> {
        self.inner.changed.clone()
    }

    fn lock_read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.inner
            .state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.inner
            .state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub fn provider_settings_service(runtime: ProviderRuntime) -> ServiceBuilder {
    let (get, refresh, events) = (runtime.clone(), runtime.clone(), runtime);
    ServiceBuilder::new("provider-settings")
        .method("getView", move |_, _| {
            let runtime = get.clone();
            async move {
                let (revision, resolved) = runtime.snapshot().await;
                Ok(Reply::Value(resolved.provider_settings_view(revision)))
            }
        })
        .method("refresh", move |_, _| {
            let runtime = refresh.clone();
            async move {
                let (revision, resolved) = runtime.reload().await;
                Ok(Reply::Value(resolved.provider_settings_view(revision)))
            }
        })
        .event("onDidChange", move |_, _| {
            let runtime = events.clone();
            let stream = runtime.changes().subscribe().then(move |()| {
                let runtime = runtime.clone();
                async move {
                    let (revision, resolved) = runtime.snapshot().await;
                    Reply::Value(resolved.provider_settings_view(revision))
                }
            });
            Ok(Box::pin(stream))
        })
}

pub fn model_selection_service(runtime: ProviderRuntime) -> ServiceBuilder {
    let (get, events) = (runtime.clone(), runtime);
    ServiceBuilder::new("model-selection")
        .method("getView", move |_, args| {
            let runtime = get.clone();
            let input = args.opt::<Value>(0, "input");
            async move {
                let input: Option<Value> = input?;
                if let Some(input) = &input
                    && !input.is_object()
                {
                    return Err(RpcError::new("ZodError", "invalid model selection input"));
                }
                let (revision, resolved) = runtime.snapshot().await;
                Ok(Reply::Value(
                    resolved.model_selection_view(revision, input.as_ref()),
                ))
            }
        })
        .event("onDidChange", move |_, _| {
            let runtime = events.clone();
            let stream = runtime.changes().subscribe().then(move |()| {
                let runtime = runtime.clone();
                async move {
                    let (revision, resolved) = runtime.snapshot().await;
                    Reply::Value(resolved.model_selection_view(revision, None))
                }
            });
            Ok(Box::pin(stream))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn revision_increases_only_when_files_change() {
        let dir = tempfile::tempdir().unwrap();
        let personal = dir.path().join("provider_config.json");
        let runtime = ProviderRuntime::new(None, personal.clone());
        assert_eq!(runtime.snapshot().await.0, 1);
        assert_eq!(runtime.reload().await.0, 1);
        let mut changes = Box::pin(runtime.changes().subscribe());
        std::fs::write(&personal, r#"{"schemaVersion":1,"config":{}}"#).unwrap();
        assert_eq!(runtime.reload().await.0, 2);
        assert_eq!(changes.next().await, Some(()));
    }
}
