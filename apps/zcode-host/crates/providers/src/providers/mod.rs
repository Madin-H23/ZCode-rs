//! `provider-settings` 与 `model-selection` channel（packages/services/src/model-provider/*）。
//!
//! 数据源：Main 在 InitLocal 给的内置配置文件，与 `<appConfigDir>/provider_config.json`。
//! 两个文件内容变化（每秒轮询，与 TS 的 personal 文件轮询节奏相同）时重新解析，revision 加一，
//! 并触发两个 channel 的 `onDidChange`。未登录：account provider 一律不可用。
pub mod builtin;
pub mod personal;
pub mod personal_schema;
pub mod resolve;

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};
use zcode_host_kit::{EventHub, Overflow, Reply, RpcError, ServiceBuilder, log};

use resolve::Resolved;

const POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct ProviderRuntime {
    inner: Arc<Inner>,
}

struct Inner {
    builtin_file: Option<PathBuf>,
    /// Endpoint 作用域的 Active/LKG 来源（账号栈接入端点后设置）；未设置时直接读随包文件。
    builtin_source: RwLock<Option<Arc<builtin::BuiltinSource>>>,
    /// 最近一次读取对齐好的 Active 路径（CLI 读它；与 registry revision 的路径一致）。
    active_builtin: RwLock<Option<PathBuf>>,
    personal_file: PathBuf,
    state: RwLock<State>,
    changed: EventHub<()>,
    /// 内置配置内容变化（账号层据此重新解析，reason `config:zcodeBuiltin:file-changed`）。
    builtin_changed: EventHub<()>,
    /// 本进程成功提交的个人配置写入（Node `personal:updated`）；轮询发现的外部变化不算。
    personal_updated: EventHub<()>,
    /// 账号层已接入时，首份账号快照发布前的读取要等待它（TS 的 Registry 同样等账号源首读）。
    account_ready: tokio::sync::watch::Sender<bool>,
}

#[derive(Default)]
struct State {
    signature: Vec<u8>,
    revision: u64,
    resolved: Arc<Resolved>,
    builtin: Arc<Value>,
    personal: Arc<Value>,
    builtin_revision: String,
    builtin_signature: Vec<u8>,
    /// 个人配置原始字节的 sha256；写入时在锁内核对（membership revision）。
    personal_revision: String,
    account: Option<Arc<AccountLayer>>,
}

/// 第三层账号 overlay（`AccountProviderConfigSnapshot`）：providers 只含 access / builtinModelIds。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AccountLayer {
    pub revision: String,
    pub based_on_builtin_revision: String,
    pub providers: serde_json::Map<String, Value>,
    pub states: serde_json::Map<String, Value>,
}

/// 内置文件的签名部分（reload 里 builtin 字节后接 0 再接个人字节）。
fn builtin_bytes_signature(signature: &[u8]) -> Vec<u8> {
    match signature.iter().position(|byte| *byte == 0) {
        Some(index) => signature[..index].to_vec(),
        None => signature.to_vec(),
    }
}

/// `zcode-builtin:<revision>:<sha256(resolve(path))>`，与 CLI registry 的计算一致。
fn builtin_revision_of(builtin: &Value, path: Option<&Path>) -> String {
    use sha2::Digest;
    let Some(path) = path else {
        return "zcode-builtin:none".to_owned();
    };
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let digest = sha2::Sha256::digest(absolute.to_string_lossy().as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    match builtin["revision"].as_u64() {
        Some(revision) => format!("zcode-builtin:{revision}:{hex}"),
        None => format!("zcode-builtin:invalid:{hex}"),
    }
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
                builtin_source: RwLock::new(None),
                active_builtin: RwLock::new(None),
                personal_file,
                state: RwLock::new(State::default()),
                // 三个都是“有变化”的信号（订阅者收到后重新读快照）：队列里已有一条时合并（§6.2）。
                changed: EventHub::new("providers-changed", 1, Overflow::Coalesce),
                builtin_changed: EventHub::new("providers-builtin-changed", 1, Overflow::Coalesce),
                personal_updated: EventHub::new(
                    "providers-personal-updated",
                    1,
                    Overflow::Coalesce,
                ),
                account_ready: tokio::sync::watch::channel(true).0,
            }),
        }
    }

    /// 给 CLI 的环境：`ZCODE_BUILTIN_PROVIDER_CONFIG_FILE` / `ZCODE_PERSONAL_PROVIDER_CONFIG_FILE`。
    /// 接入 Endpoint 作用域来源后是已物化的 Active 路径，否则是随包文件。
    pub fn builtin_file(&self) -> Option<PathBuf> {
        let active = self
            .inner
            .active_builtin
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        active.or_else(|| self.inner.builtin_file.clone())
    }

    /// 接入 Endpoint 作用域的 Built-in 来源（`EndpointScopedZCodeBuiltinSource`）；没有随包文件时忽略。
    pub fn set_builtin_environment(&self, environment: builtin::BuiltinEnvironment) {
        let Some(bundled) = self.inner.builtin_file.clone() else {
            return;
        };
        *self
            .inner
            .builtin_source
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(builtin::BuiltinSource::new(bundled, environment));
    }

    fn builtin_source(&self) -> Option<Arc<builtin::BuiltinSource>> {
        self.inner
            .builtin_source
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 等首轮读取物化好 Active 后返回 CLI 应读的内置配置路径（`resolveZCodeBuiltinActiveFilePath`）。
    pub async fn ensure_builtin_file(&self) -> Option<PathBuf> {
        if self.lock_read().revision == 0 {
            self.reload().await;
        }
        self.builtin_file()
    }

    /// `refreshZCodeBuiltin({ force })`：CDN 有更新时立刻重新读取并通知。
    pub async fn refresh_builtin(&self, force: bool) -> Result<String, String> {
        let Some(source) = self.builtin_source() else {
            return Ok("skipped".into());
        };
        let result = source.refresh(force).await?;
        if result == "updated" {
            self.reload().await;
        }
        Ok(result)
    }

    /// 周期检查（`NodeProviderConfigRuntime.start`）：启动时一次，之后每分钟一次；
    /// 是否真正下载由控制文件的租约与下次时间决定。
    pub fn start_builtin_refresh(&self) {
        if self.builtin_source().is_none() {
            return;
        }
        let runtime = self.clone();
        tokio::spawn(async move {
            loop {
                if let Err(error) = runtime.refresh_builtin(false).await {
                    log::warn(format!("ZCode Built-in Config 远端刷新失败: {error}"));
                }
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
    }

    async fn read_builtin(&self) -> (Vec<u8>, Value, Option<PathBuf>) {
        let Some(bundled) = self.inner.builtin_file.clone() else {
            return (Vec::new(), Value::Null, None);
        };
        let Some(source) = self.builtin_source() else {
            let (bytes, value) = read_json(&bundled).await;
            return (bytes, value, Some(bundled));
        };
        match source.read().await {
            Ok(read) => {
                let value = serde_json::from_slice::<Value>(&read.bytes).unwrap_or(Value::Null);
                *self
                    .inner
                    .active_builtin
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    read.materialized.then(|| read.path.clone());
                (read.bytes, value, Some(read.path))
            }
            Err(error) => {
                log::warn(format!("ZCode Built-in 配置不可用: {error}"));
                (Vec::new(), Value::Null, Some(bundled))
            }
        }
    }

    pub fn personal_file(&self) -> &Path {
        &self.inner.personal_file
    }

    /// 重新读取两个文件；内容变化时重新解析并通知。返回当前快照。
    pub async fn reload(&self) -> (u64, Arc<Resolved>) {
        let (builtin_bytes, builtin, builtin_path) = self.read_builtin().await;
        let (personal_bytes, personal) = read_json(&self.inner.personal_file).await;
        // 路径也进签名：Endpoint 切换后即使内容相同，registry revision（含路径哈希）也要更新。
        let mut signature = builtin_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned().into_bytes())
            .unwrap_or_default();
        signature.push(0xff);
        signature.extend_from_slice(&builtin_bytes);
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
        let builtin_revision = builtin_revision_of(&builtin, builtin_path.as_deref());
        let personal_revision = {
            use sha2::Digest;
            sha2::Sha256::digest(&personal_bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        let (builtin, personal) = (
            Arc::new(config_of(&builtin)),
            Arc::new(config_of(&personal)),
        );
        let builtin_changed = {
            let mut state = self.lock_write();
            let changed = state.revision > 0
                && state.builtin_signature != builtin_bytes_signature(&signature);
            state.builtin_signature = builtin_bytes_signature(&signature);
            state.builtin = builtin;
            state.personal = personal;
            state.builtin_revision = builtin_revision;
            state.personal_revision = personal_revision;
            state.signature = signature;
            changed
        };
        let snapshot = self.re_resolve().await;
        if builtin_changed {
            self.inner.builtin_changed.emit(());
        }
        snapshot
    }

    /// 用缓存的两层配置与当前账号层重新解析，revision 加一并通知。
    async fn re_resolve(&self) -> (u64, Arc<Resolved>) {
        let (builtin, personal, account) = {
            let state = self.lock_read();
            (
                state.builtin.clone(),
                state.personal.clone(),
                state.account.clone(),
            )
        };
        let resolved = tokio::task::spawn_blocking(move || {
            resolve::resolve(&builtin, &personal, account.as_deref())
        })
        .await
        .unwrap_or_default();
        let snapshot = {
            let mut state = self.lock_write();
            state.revision += 1;
            state.resolved = Arc::new(resolved);
            (state.revision, state.resolved.clone())
        };
        if snapshot.0 > 1 {
            self.inner.changed.emit(());
        }
        snapshot
    }

    /// 账号层接入：之后的读取等首份账号快照（`set_account_layer`）发布。
    pub fn attach_account_layer(&self) {
        let has_layer = self.lock_read().account.is_some();
        if !has_layer {
            self.inner.account_ready.send_replace(false);
        }
    }

    /// 发布账号层；内容不变时不重新解析。
    pub async fn set_account_layer(&self, layer: AccountLayer) {
        let unchanged = {
            let mut state = self.lock_write();
            let unchanged = state.account.as_deref() == Some(&layer);
            if !unchanged {
                state.account = Some(Arc::new(layer));
            }
            unchanged
        };
        if !unchanged && self.lock_read().revision > 0 {
            self.re_resolve().await;
        }
        self.inner.account_ready.send_replace(true);
    }

    pub fn account_layer(&self) -> Option<Arc<AccountLayer>> {
        self.lock_read().account.clone()
    }

    /// 内置层的账号 provider 规则与 revision（`zcode-builtin:<revision>:<sha256(path)>`）。
    pub async fn builtin_snapshot(&self) -> (String, Arc<Value>) {
        if self.lock_read().revision == 0 {
            self.reload().await;
        }
        let state = self.lock_read();
        (state.builtin_revision.clone(), state.builtin.clone())
    }

    /// 原子地取快照与对应的个人配置 revision（写入的 membership 核对用）。
    pub async fn snapshot_with_personal(&self) -> (u64, Arc<Resolved>, String) {
        let (revision, resolved) = self.snapshot().await;
        let state = self.lock_read();
        if state.revision == revision {
            return (revision, resolved, state.personal_revision.clone());
        }
        (
            state.revision,
            state.resolved.clone(),
            state.personal_revision.clone(),
        )
    }

    /// 本进程提交的个人配置写入（Provider Provisioning 的 `personal-config` 触发）。
    pub fn personal_updates(&self) -> EventHub<()> {
        self.inner.personal_updated.clone()
    }

    pub(crate) fn notify_personal_updated(&self) {
        self.inner.personal_updated.emit(());
    }

    pub fn builtin_changes(&self) -> EventHub<()> {
        self.inner.builtin_changed.clone()
    }

    pub async fn snapshot(&self) -> (u64, Arc<Resolved>) {
        let mut ready = self.inner.account_ready.subscribe();
        // 发送端由自身持有，wait_for 不会因关闭而失败；失败时直接读当前快照。
        let _ = ready.wait_for(|ready| *ready).await;
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
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.inner
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
