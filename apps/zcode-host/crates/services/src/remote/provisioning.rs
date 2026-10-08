//! Provider Provisioning 的本机来源与远端执行（`providerProvisioningSource.ts`、
//! `remoteProviderProvisioningService.ts`）：读本机 Personal Provider Config、账号设置与 allowlist
//! 凭据，组装 envelope，交给远端 `provider-provisioning-target.apply`（rust-host-remote.md §2.6）。
//!
//! envelope 含解密后的凭据：只在本进程与远端 rpc 之间传递，绝不进日志。
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use zcode_host_kit::RpcValue;
use zcode_host_rpc::RpcClient;

pub const TARGET_CHANNEL: &str = "provider-provisioning-target";

/// `PROVIDER_PROVISIONING_OAUTH_CREDENTIAL_KEYS`。
const OAUTH_CREDENTIAL_KEYS: &[&str] = &[
    "oauth:active_provider",
    "oauth:zai:access_token",
    "oauth:zai:refresh_token",
    "oauth:zai:user_info",
    "oauth:bigmodel:access_token",
    "oauth:bigmodel:refresh_token",
    "oauth:bigmodel:user_info",
    "zcodejwttoken",
];

const CIPHER_PREFIX: &str = "enc:v1:";

/// `isProviderProvisioningAccountCredentialKey`：`^account-provider:.+:api-key$` 且无首尾空白。
fn is_account_credential_key(key: &str) -> bool {
    key.trim() == key
        && key
            .strip_prefix("account-provider:")
            .and_then(|rest| rest.strip_suffix(":api-key"))
            .is_some_and(|middle| !middle.is_empty())
}

/// Node `os.platform()`。
fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// Node `os.userInfo().username`。
#[cfg(unix)]
#[allow(unsafe_code)]
fn username() -> Option<String> {
    // SAFETY: getpwuid_r 只写调用方提供的缓冲区；返回的指针只在本函数内读取。
    unsafe {
        let uid = libc::geteuid();
        let mut passwd: libc::passwd = std::mem::zeroed();
        let mut buffer = vec![0 as libc::c_char; 4096];
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let status = libc::getpwuid_r(
            uid,
            &mut passwd,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        );
        if status != 0 || result.is_null() || passwd.pw_name.is_null() {
            return None;
        }
        Some(
            std::ffi::CStr::from_ptr(passwd.pw_name)
                .to_string_lossy()
                .into_owned(),
        )
    }
}

#[cfg(not(unix))]
fn username() -> Option<String> {
    None
}

/// `credentialCipherProvider.ts` 的解密：key = sha256(`ZCODE_CREDENTIAL_SECRET` 或 fallback 串)。
pub struct CredentialCipher {
    key: LessSafeKey,
}

impl CredentialCipher {
    pub fn new(secret: Option<&str>, home: &Path, user: Option<String>) -> Self {
        let secret = match secret.filter(|secret| !secret.is_empty()) {
            Some(secret) => secret.to_owned(),
            None => format!(
                "zcode-credential-fallback:{}:{}:{}",
                node_platform(),
                home.display(),
                user.or_else(username).unwrap_or_else(|| "unknown".into())
            ),
        };
        let digest = Sha256::digest(secret.as_bytes());
        let key = UnboundKey::new(&AES_256_GCM, &digest).expect("AES-256 key is 32 bytes");
        Self {
            key: LessSafeKey::new(key),
        }
    }

    /// 非 `enc:v1:` 前缀的旧明文原样返回。
    pub fn decrypt(&self, value: &str) -> Result<String, String> {
        let Some(payload) = value.strip_prefix(CIPHER_PREFIX) else {
            return Ok(value.to_owned());
        };
        let fail = |reason: &str| format!("凭据解密失败：{reason}");
        let parts: Vec<&str> = payload.split('.').collect();
        if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
            return Err(fail("密文格式非法"));
        }
        let decode = |part: &str| URL_SAFE_NO_PAD.decode(part.trim_end_matches('='));
        let iv = decode(parts[0]).map_err(|_| fail("IV 长度非法"))?;
        let tag = decode(parts[1]).map_err(|_| fail("AuthTag 长度非法"))?;
        let mut buffer = decode(parts[2]).map_err(|_| fail("密钥不匹配或密文已损坏"))?;
        if tag.len() != 16 {
            return Err(fail("AuthTag 长度非法"));
        }
        let nonce = Nonce::try_assume_unique_for_key(&iv).map_err(|_| fail("IV 长度非法"))?;
        buffer.extend_from_slice(&tag);
        let plain = self
            .key
            .open_in_place(nonce, Aad::empty(), &mut buffer)
            .map_err(|_| fail("密钥不匹配或密文已损坏"))?;
        String::from_utf8(plain.to_vec()).map_err(|_| fail("密钥不匹配或密文已损坏"))
    }
}

pub struct ProvisioningSource {
    pub personal_config_file: PathBuf,
    pub credential_file: PathBuf,
    pub cipher: CredentialCipher,
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
    fn credentials(&self, raw: Option<&str>) -> Result<Vec<Value>, String> {
        let Some(raw) = raw else {
            return Ok(Vec::new());
        };
        let parsed: Value = serde_json::from_str(raw)
            .map_err(|error| format!("Credential Store 无法解析：{error}"))?;
        let Some(object) = parsed.as_object() else {
            return Err("Credential Store 必须是 JSON 对象".into());
        };
        let mut entries = Vec::new();
        for (key, encrypted) in object {
            let scope = if OAUTH_CREDENTIAL_KEYS.contains(&key.as_str()) {
                "oauth-session"
            } else if is_account_credential_key(key) {
                "account-provider"
            } else {
                // allowlist 之外的历史记录不是同步事实，值损坏也不阻断合法凭据。
                continue;
            };
            let Some(encrypted) = encrypted.as_str() else {
                return Err(format!(
                    "Credential allowlist value must be a string: {key}"
                ));
            };
            let value = self.cipher.decrypt(encrypted)?;
            if value.trim().is_empty() {
                continue;
            }
            entries.push(json!({"scope": scope, "key": key, "value": value}));
        }
        Ok(entries)
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
        let credentials = read(self.credential_file.clone()).await?;
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
            "credentials": self.credentials(credentials.as_deref())?,
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
    fn decrypts_node_ciphertext_and_legacy_plaintext() {
        let cipher = CredentialCipher::new(Some("test-secret"), Path::new("/h"), None);
        assert_eq!(cipher.decrypt(NODE_CIPHERTEXT).unwrap(), "token-1");
        assert_eq!(cipher.decrypt("plain").unwrap(), "plain");
        assert!(cipher.decrypt("enc:v1:a.b").is_err());
    }

    #[tokio::test]
    async fn envelope_keeps_only_allowlisted_credentials() {
        let dir = std::env::temp_dir().join(format!("zcode-provisioning-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let credentials = dir.join("credentials.json");
        std::fs::write(
            &credentials,
            json!({
                "zcodejwttoken": NODE_CIPHERTEXT,
                "account-provider:zai:api-key": "plain-key",
                "account-provider:zai:identity": "ignored",
                "oauth:zai:user_info": "  ",
                "unrelated": 3,
            })
            .to_string(),
        )
        .unwrap();
        let source = ProvisioningSource {
            personal_config_file: dir.join("provider_config.json"),
            credential_file: credentials,
            cipher: CredentialCipher::new(Some("test-secret"), Path::new("/h"), None),
        };
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
            dir.join("provider_config.json"),
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
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
