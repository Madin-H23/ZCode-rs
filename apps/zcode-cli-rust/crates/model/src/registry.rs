//! Existing App provider files, resolved once per configuration revision.
//!
//! The Engine refreshes every second; the files are only read when their
//! metadata changed or a minute passed (spec rust-provider-config-poll).
use super::registry_rules::{resolve, validate_account};
use crate::contract::{ModelIdentity, ModelPort, ModelRegistry};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use zcode_kit::fs::{ChangeGate, FileStamp};

/// Unchanged metadata still re-reads the files this often (writes that keep
/// size, mtime and ctime within one timestamp tick; Windows has no ctime).
const FULL_READ_EVERY: Duration = Duration::from_secs(60);

pub(super) type ModelKey = (String, String, String);
#[derive(Default)]
pub(super) struct Snapshot {
    pub(super) signature: Vec<u8>,
    pub(super) models: BTreeMap<ModelKey, Arc<dyn ModelPort>>,
    pub(super) catalog: Vec<Value>,
    pub(super) options: Vec<Value>,
    /// Visible models in Node `toModelOption` shape (legacy session snapshots).
    pub(super) legacy: Vec<Value>,
    pub(super) default: Option<ModelIdentity>,
}
pub struct Registry {
    builtin: PathBuf,
    personal: PathBuf,
    inputs: tokio::sync::Mutex<Inputs>,
    snapshot: RwLock<Snapshot>,
    egress: Arc<zcode_cli_net::Egress>,
}
/// What a refresh reads besides the files, under the one serial refresh entry.
struct Inputs {
    account: Option<Value>,
    /// Metadata of the files before the last successful read.
    files: ChangeGate,
}
impl Registry {
    /// The provider files named by the runtime's environment snapshot (the
    /// Host injects them into the runtime only, never the process environment).
    pub async fn from_env(egress: Arc<zcode_cli_net::Egress>) -> Result<Option<Arc<Self>>> {
        let env = egress.runtime_env();
        let builtin = env
            .get("ZCODE_BUILTIN_PROVIDER_CONFIG_FILE")
            .map(PathBuf::from);
        let personal = env
            .get("ZCODE_PERSONAL_PROVIDER_CONFIG_FILE")
            .map(PathBuf::from);
        if builtin.is_none() && personal.is_none() {
            return Ok(None);
        }
        let registry = Arc::new(Self::new(
            builtin.context("Builtin config required")?,
            personal.context("Personal config required")?,
            egress,
        ));
        registry.refresh(None).await?;
        Ok(Some(registry))
    }
    /// Relative paths resolve against the runtime's base directory (Node: cwd).
    pub fn new(builtin: PathBuf, personal: PathBuf, egress: Arc<zcode_cli_net::Egress>) -> Self {
        Self {
            builtin: egress.base_dir().join(builtin),
            personal: egress.base_dir().join(personal),
            inputs: tokio::sync::Mutex::new(Inputs {
                account: None,
                files: ChangeGate::new(FULL_READ_EVERY),
            }),
            snapshot: Default::default(),
            egress,
        }
    }
}
#[async_trait::async_trait]
impl ModelRegistry for Registry {
    async fn received_account(&self) -> Option<Value> {
        self.inputs.lock().await.account.clone()
    }
    fn model_options(&self) -> Vec<Value> {
        self.snapshot.read().unwrap().options.clone()
    }
    fn legacy_models(&self) -> Vec<Value> {
        self.snapshot.read().unwrap().legacy.clone()
    }
    fn catalog(&self) -> Vec<Value> {
        self.snapshot.read().unwrap().catalog.clone()
    }
    fn default_selection(&self) -> Option<ModelIdentity> {
        self.snapshot.read().unwrap().default.clone()
    }
    fn resolve(&self, s: &ModelIdentity) -> Result<Arc<dyn ModelPort>> {
        self.snapshot
            .read()
            .unwrap()
            .models
            .get(&(
                s.provider_id.clone(),
                s.model_id.clone(),
                s.reasoning_level.clone(),
            ))
            .cloned()
            .context("Selected model is unavailable")
    }
    #[expect(clippy::await_holding_invalid_type, reason = "存量，U2 改为 actor")]
    async fn refresh(&self, update: Option<Value>) -> Result<bool> {
        // 配置刷新共用一个串行入口；迟到的文件读取不能覆盖新账号事实。
        let mut inputs = self.inputs.lock().await;
        let polled = update.is_none();
        if let Some(next) = update {
            validate_account(&next)?;
            inputs.account = Some(next);
        }
        // 先取元数据再读：读的过程中文件再变，下一轮元数据就不同。轮询时元数据没变就不读、不算哈希；
        // 账号更新总是读。读失败不记录，下一轮照常重读（与每轮都读时相同）。
        let stamps = vec![
            FileStamp::of(&self.builtin).await,
            FileStamp::of(&self.personal).await,
        ];
        let now = Instant::now();
        if polled && !inputs.files.should_read(&stamps, now) {
            return Ok(false);
        }
        let changed = self.load(inputs.account.as_ref()).await?;
        inputs.files.record(stamps, now);
        Ok(changed)
    }
}
impl Registry {
    /// Reads both files; resolves a new snapshot when they or the account changed.
    async fn load(&self, account: Option<&Value>) -> Result<bool> {
        let builtin = tokio::fs::read(&self.builtin)
            .await
            .context("Cannot read builtin provider config")?;
        let personal = match tokio::fs::read(&self.personal).await {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                b"{\"schemaVersion\":1,\"config\":{}}".to_vec()
            }
            Err(_) => anyhow::bail!("Cannot read personal provider config"),
        };
        ensure!(
            builtin.len() <= 8 * 1024 * 1024 && personal.len() <= 8 * 1024 * 1024,
            "Provider config exceeds limit"
        );
        let mut hash = Sha256::new();
        hash.update(&builtin);
        hash.update(&personal);
        hash.update(serde_json::to_vec(&account)?);
        let signature = hash.finalize().to_vec();
        if self.snapshot.read().unwrap().signature == signature {
            return Ok(false);
        }
        let builtin: Value =
            serde_json::from_slice(&builtin).context("Invalid builtin provider config")?;
        let personal: Value =
            serde_json::from_slice(&personal).context("Invalid personal provider config")?;
        ensure!(
            builtin["schemaVersion"] == 1 && personal["schemaVersion"] == 1,
            "Unsupported provider schema"
        );
        let path = std::path::absolute(&self.builtin)?;
        let source = format!("{:x}", Sha256::digest(path.to_string_lossy().as_bytes()));
        let revision = format!(
            "zcode-builtin:{}:{source}",
            builtin["revision"]
                .as_u64()
                .context("Invalid builtin revision")?
        );
        if let Some(account) = account
            && account["basedOnZCodeBuiltinRevision"] != revision
        {
            return Ok(false);
        }
        let egress = self.egress.clone();
        let account_value = account.cloned().unwrap_or_else(|| json!({}));
        let mut next = zcode_async::blocking(move || {
            resolve(
                &builtin["config"],
                &personal["config"],
                &account_value,
                egress,
            )
        })
        .await??;
        next.signature = signature;
        *self.snapshot.write().unwrap() = next;
        Ok(true)
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
