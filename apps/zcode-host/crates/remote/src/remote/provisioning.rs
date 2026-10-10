//! Provider Provisioning 的本机来源与远端执行（`providerProvisioningSource.ts`、
//! `remoteProviderProvisioningService.ts`）：读本机 Personal Provider Config、账号设置与 allowlist
//! 凭据，组装 envelope，交给远端 `provider-provisioning-target.apply`（rust-host-remote.md §2.6）。
//!
//! envelope 含解密后的凭据：只在本进程与远端 rpc 之间传递，绝不进日志。
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use tokio::task::AbortHandle;
use zcode_host_control::{ControlSink, HostToMain};
use zcode_host_kit::RpcValue;
use zcode_host_rpc::RpcClient;

use crate::account::credential::CredentialStore;

pub const TARGET_CHANNEL: &str = "provider-provisioning-target";

/// `PROVIDER_PROVISIONING_OAUTH_CREDENTIAL_KEYS`。
pub const OAUTH_CREDENTIAL_KEYS: &[&str] = &[
    "oauth:active_provider",
    "oauth:zai:access_token",
    "oauth:zai:refresh_token",
    "oauth:zai:user_info",
    "oauth:bigmodel:access_token",
    "oauth:bigmodel:refresh_token",
    "oauth:bigmodel:user_info",
    "zcodejwttoken",
];

/// `isProviderProvisioningAccountCredentialKey`：`^account-provider:.+:api-key$` 且无首尾空白。
pub fn is_account_credential_key(key: &str) -> bool {
    key.trim() == key
        && key
            .strip_prefix("account-provider:")
            .and_then(|rest| rest.strip_suffix(":api-key"))
            .is_some_and(|middle| !middle.is_empty())
}

/// 凭据是否属于 Provisioning 的同步事实（OAuth 会话或账号 Provider 的 API key）。
pub fn is_provisioned_credential(key: &str) -> bool {
    OAUTH_CREDENTIAL_KEYS.contains(&key) || is_account_credential_key(key)
}

pub struct ProvisioningSource {
    pub personal_config_file: PathBuf,
    /// 账号部分的凭据存储（`credentials.json` 的唯一读写实现）。
    pub credentials: CredentialStore,
}

fn empty_personal_config() -> Value {
    json!({
        "providerConfigRules": {"providerRules": []},
        "modelConfigRules": {"providerModelRules": [], "manualProviderModelRules": []},
    })
}

/// `encodeProviderConfigFile(personal).config`：只取 schema 允许的键，缺失的规则补空数组。
fn personal_config(raw: Option<&str>, path: &Path) -> Result<Value, String> {
    let Some(raw) = raw else {
        return Ok(empty_personal_config());
    };
    let unreadable = || format!("本地 Personal Provider Config 无法同步: {}", path.display());
    let file: Value = serde_json::from_str(raw).map_err(|_| unreadable())?;
    if file["schemaVersion"] != json!(1) || !file["config"].is_object() {
        return Err(unreadable());
    }
    let config = &file["config"];
    let array = |value: &Value| {
        if value.is_array() {
            value.clone()
        } else {
            json!([])
        }
    };
    let mut out = Map::new();
    if config["providerOrder"].is_array() {
        out.insert("providerOrder".into(), config["providerOrder"].clone());
    }
    out.insert(
        "providerConfigRules".into(),
        json!({"providerRules": array(&config["providerConfigRules"]["providerRules"])}),
    );
    out.insert(
        "modelConfigRules".into(),
        json!({
            "providerModelRules": array(&config["modelConfigRules"]["providerModelRules"]),
            "manualProviderModelRules": array(&config["modelConfigRules"]["manualProviderModelRules"]),
        }),
    );
    if config["defaultModelSelection"].is_object() {
        out.insert(
            "defaultModelSelection".into(),
            config["defaultModelSelection"].clone(),
        );
    }
    Ok(Value::Object(out))
}

impl ProvisioningSource {
    /// 测试用：空的临时目录。
    #[cfg(test)]
    pub(crate) fn for_test(dir: &Path) -> Self {
        Self {
            personal_config_file: dir.join("provider_config.json"),
            credentials: CredentialStore::new(
                &zcode_host_base::testing::spawner(),
                dir,
                "test-secret",
            ),
        }
    }

    /// allowlist 内的凭据（readProvisioningCredentials）：解密后为空的跳过。
    async fn credential_entries(&self) -> Result<Vec<Value>, String> {
        let selected = self
            .credentials
            .load_selected(is_provisioned_credential)
            .await
            .map_err(|error| error.to_string())?;
        Ok(selected
            .into_iter()
            .filter(|(_, value)| !value.trim().is_empty())
            .map(|(key, value)| {
                let scope = if OAUTH_CREDENTIAL_KEYS.contains(&key.as_str()) {
                    "oauth-session"
                } else {
                    "account-provider"
                };
                json!({"scope": scope, "key": key, "value": value})
            })
            .collect())
    }

    /// 组装 envelope（`providerProvisioningEnvelopeSchema`）。
    pub async fn read(&self, sync_id: &str, settings: &Value) -> Result<Value, String> {
        let read = |path: PathBuf| async move {
            match tokio::fs::read_to_string(&path).await {
                Ok(text) => Ok(Some(text)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(format!("{}: {error}", path.display())),
            }
        };
        let personal = read(self.personal_config_file.clone()).await?;
        let domain = match settings["providerFamilyDomain"].as_str() {
            Some(domain @ ("zai" | "bigmodel")) => json!(domain),
            _ => Value::Null,
        };
        let selections = if settings["providerFamilyConnectionSelections"].is_object() {
            settings["providerFamilyConnectionSelections"].clone()
        } else {
            json!({})
        };
        Ok(json!({
            "schemaVersion": 1,
            "syncId": sync_id,
            "personalConfig": personal_config(personal.as_deref(), &self.personal_config_file)?,
            "accountSettings": {
                "providerFamilyDomain": domain,
                "providerFamilyConnectionSelections": selections,
            },
            "credentials": self.credential_entries().await?,
        }))
    }
}

/// `syncLocalToRemote`：返回 `ProviderProvisioningResult`（status、errorMessage?）。
pub async fn sync_local_to_remote(
    source: &ProvisioningSource,
    settings: &Value,
    client: &RpcClient,
) -> Value {
    let sync_id = crate::agent::service::uuid_v4();
    let failed = |message: String| {
        json!({
            "syncId": sync_id, "status": "failed", "personalProviderCount": 0, "credentialCount": 0,
            "errorMessage": message, "rolledBack": true,
        })
    };
    let envelope = match source.read(&sync_id, settings).await {
        Ok(envelope) => envelope,
        Err(error) => return failed(error),
    };
    match client
        .call(TARGET_CHANNEL, "apply", vec![RpcValue::Json(envelope)])
        .await
    {
        Ok(result) => result.into_json(),
        Err(error) if error.message.contains("Unknown channel") => json!({
            "syncId": sync_id, "status": "unsupported", "personalProviderCount": 0, "credentialCount": 0,
            "errorMessage": "Local/Remote Provider Provisioning capability 不可用", "rolledBack": false,
        }),
        Err(error) => failed(error.message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 由 Node `createCredentialCipherProvider` 以 secret "test-secret" 加密 "token-1" 得到。
    const NODE_CIPHERTEXT: &str = "enc:v1:tTO2tJu_lZkZJwgA.-4ZIv62JA9WlgfcIO9wx6g.05-gxQ4Y4w";

    #[test]
    fn allowlist_matches_typescript() {
        assert!(is_provisioned_credential("zcodejwttoken"));
        assert!(is_provisioned_credential("account-provider:zai:api-key"));
        assert!(!is_provisioned_credential("account-provider::api-key"));
        assert!(!is_provisioned_credential(" account-provider:zai:api-key"));
        assert!(!is_provisioned_credential("account-provider:zai:identity"));
    }

    #[tokio::test]
    async fn envelope_keeps_only_allowlisted_credentials() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("credentials.json"),
            json!({
                "zcodejwttoken": NODE_CIPHERTEXT,
                "account-provider:zai:api-key": "plain-key",
                "account-provider:zai:identity": "ignored",
                "oauth:zai:user_info": "  ",
                "unrelated": "x",
            })
            .to_string(),
        )
        .unwrap();
        let source = ProvisioningSource::for_test(dir.path());
        let envelope = source
            .read("sync-1", &json!({"providerFamilyDomain": "zai"}))
            .await
            .unwrap();
        assert_eq!(
            envelope,
            json!({
                "schemaVersion": 1, "syncId": "sync-1",
                "personalConfig": empty_personal_config(),
                "accountSettings": {"providerFamilyDomain": "zai", "providerFamilyConnectionSelections": {}},
                "credentials": [
                    {"scope": "oauth-session", "key": "zcodejwttoken", "value": "token-1"},
                    {"scope": "account-provider", "key": "account-provider:zai:api-key", "value": "plain-key"},
                ],
            })
        );
        std::fs::write(
            dir.path().join("provider_config.json"),
            json!({"schemaVersion": 1, "config": {"providerOrder": ["a"], "providerConfigRules": {"providerRules": [{"id": "a"}]}}}).to_string(),
        )
        .unwrap();
        let envelope = source.read("sync-2", &json!({})).await.unwrap();
        assert_eq!(
            envelope["personalConfig"],
            json!({"providerOrder": ["a"], "providerConfigRules": {"providerRules": [{"id": "a"}]},
                   "modelConfigRules": {"providerModelRules": [], "manualProviderModelRules": []}})
        );
        assert_eq!(
            envelope["accountSettings"]["providerFamilyDomain"],
            Value::Null
        );
        // 密钥不匹配：解密失败让本次同步失败，而不是带着缺失的凭据继续。
        let wrong = ProvisioningSource {
            personal_config_file: dir.path().join("provider_config.json"),
            credentials: CredentialStore::new(
                &zcode_host_base::testing::spawner(),
                dir.path(),
                "other-secret",
            ),
        };
        assert!(
            wrong
                .read("sync-3", &json!({}))
                .await
                .unwrap_err()
                .starts_with("凭据解密失败")
        );
    }

    struct Sink(std::sync::Mutex<Vec<Value>>);

    impl ControlSink for Sink {
        fn send(&self, message: HostToMain) {
            self.0
                .lock()
                .unwrap()
                .push(serde_json::to_value(&message).unwrap());
        }
    }

    fn builtin_path() -> PathBuf {
        PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../../config/provider/zcode-builtin.json"
        ))
    }

    /// 三个写入方提交成功后各自触发一次同步；无关的设置键与凭据不触发（node.ts 的触发规则）。
    #[tokio::test]
    async fn committed_writes_notify_main_to_resync_remote_environments() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink(std::sync::Mutex::new(Vec::new())));
        let providers = crate::providers::ProviderRuntime::new(
            &zcode_host_base::testing::spawner(),
            Some(builtin_path()),
            dir.path().join("provider_config.json"),
        );
        let sources = ProvisioningSources {
            settings: crate::settings::SettingStore::new(
                &zcode_host_base::testing::spawner(),
                dir.path(),
            ),
            credentials: CredentialStore::new(
                &zcode_host_base::testing::spawner(),
                dir.path(),
                "test-secret",
            ),
            providers: providers.clone(),
        };
        let tasks = watch_sources(&sources, sink.clone());
        let triggers = || -> Vec<String> {
            sink.0
                .lock()
                .unwrap()
                .iter()
                .map(|message| {
                    assert_eq!(message["type"], "provider-provisioning-source-changed");
                    message["trigger"].as_str().unwrap().to_owned()
                })
                .collect()
        };
        let settle = || tokio::time::sleep(std::time::Duration::from_millis(50));

        sources
            .settings
            .update(
                &vec![("providerFamilyDomain".into(), Some(json!("zai")))],
                None,
            )
            .await
            .unwrap();
        sources
            .settings
            .update(&vec![("memoryEnabled".into(), Some(json!(true)))], None)
            .await
            .unwrap();
        sources
            .credentials
            .save("zcodejwttoken", "jwt")
            .await
            .unwrap();
        sources.credentials.save("unrelated", "x").await.unwrap();
        sources
            .credentials
            .delete("account-provider:zai:api-key")
            .await
            .unwrap();
        crate::providers::personal::PersonalWriter::new(providers)
            .create_provider(&json!({"initialConfig": {
                "api": {"type": "openai-chat-completions", "baseUrl": "https://api.example.com/v1"},
                "access": {"type": "api-key", "apiKey": "sk-x"}}}))
            .await
            .unwrap();
        settle().await;
        let mut seen = triggers();
        seen.sort();
        assert_eq!(
            seen,
            [
                "account-settings",
                "credential",
                "credential",
                "personal-config"
            ]
        );
        for task in tasks {
            task.abort();
        }
    }
}

/// 本机的三个 Provisioning 事实源（各自是唯一写入方）。
#[derive(Clone)]
pub struct ProvisioningSources {
    pub settings: crate::settings::SettingStore,
    pub credentials: CredentialStore,
    pub providers: crate::providers::ProviderRuntime,
}

/// 写入方提交成功后通知 Main：`provider-provisioning-source-changed{trigger}`，Main 再让每个在线的
/// 远端 Environment 重新同步（node.ts 的三个触发；合并与排队由 Main 的 coordinator 负责，Host 不防抖）。
/// - Personal Provider Config：本进程提交的写入 → `personal-config`；
/// - 设置里的 `providerFamilyDomain` / `providerFamilyConnectionSelections` → `account-settings`；
/// - allowlist 内的凭据保存或删除 → `credential`。
pub fn watch_sources(
    sources: &ProvisioningSources,
    control: Arc<dyn ControlSink>,
) -> Vec<AbortHandle> {
    let notify = move |trigger: &'static str| {
        control.send(HostToMain::Raw(json!({
            "type": "provider-provisioning-source-changed",
            "trigger": trigger,
        })));
    };
    let personal = sources.providers.personal_updates().subscribe();
    let settings = sources.settings.on_updated().subscribe();
    let credentials = sources.credentials.mutations().subscribe();
    let (on_personal, on_settings, on_credential) = (notify.clone(), notify.clone(), notify);
    vec![
        tokio::spawn(async move {
            let mut personal = Box::pin(personal);
            while personal.next().await.is_some() {
                on_personal("personal-config");
            }
        })
        .abort_handle(),
        tokio::spawn(async move {
            let mut settings = Box::pin(settings);
            while let Some(keys) = settings.next().await {
                if keys.iter().any(|key| {
                    key == "providerFamilyDomain" || key == "providerFamilyConnectionSelections"
                }) {
                    on_settings("account-settings");
                }
            }
        })
        .abort_handle(),
        tokio::spawn(async move {
            let mut credentials = Box::pin(credentials);
            while let Some(mutation) = credentials.next().await {
                if is_provisioned_credential(&mutation.key) {
                    on_credential("credential");
                }
            }
        })
        .abort_handle(),
    ]
}
