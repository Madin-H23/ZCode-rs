//! `credential` channel 与凭据存储（packages/services/src/credential/*）。
//!
//! `<appConfigDir>/credentials.json` 与 Node Host、TS CLI 共用：值用 AES-256-GCM 加密成
//! `enc:v1:<iv>.<tag>.<ciphertext>`（无填充 base64url），读改写全程持同一个目录锁。
//! 进程内不缓存文件内容，每次读写都以磁盘为准。
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use zcode_host_kit::{EventHub, Overflow, Reply, RpcError, ServiceBuilder, log};

use crate::store::{LockOptions, atomic_write_private_text, with_file_lock};

const ENCRYPTED_VALUE_PREFIX: &str = "enc:v1:";
const IV_BYTES: usize = 12;
const TAG_BYTES: usize = 16;
const SECRET_ENV_KEY: &str = "ZCODE_CREDENTIAL_SECRET";
/// `@zcode/shared` `CREDENTIAL_DECRYPT_ERROR_CODE` / `_PREFIX`：UI 按它们判断“可恢复为未登录”。
pub const DECRYPT_ERROR_CODE: &str = "ZCODE_CREDENTIAL_DECRYPT_FAILED";
pub const DECRYPT_ERROR_PREFIX: &str = "凭据解密失败：";

/// Node `Buffer.from(raw, "base64url")` 接受有无填充两种写法；输出始终无填充。
const BASE64URL: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialError {
    /// 解密失败（原因文案与 Node 相同）。
    Decrypt(&'static str),
    /// 文件损坏：已备份，拒绝覆盖。
    Corrupt(String),
    /// 读写失败。
    Io(String),
    InvalidKey,
}

impl CredentialError {
    pub fn is_decrypt(&self) -> bool {
        matches!(self, CredentialError::Decrypt(_))
    }

    pub fn to_rpc(&self) -> RpcError {
        match self {
            CredentialError::Decrypt(reason) => {
                RpcError::new("Error", format!("{DECRYPT_ERROR_PREFIX}{reason}"))
                    .with_code(DECRYPT_ERROR_CODE)
            }
            CredentialError::InvalidKey => RpcError::new(
                "ZodError",
                "invalid argument key: String must contain at least 1 character(s)",
            )
            .with_code("fault.invalidArgument"),
            CredentialError::Corrupt(message) | CredentialError::Io(message) => {
                RpcError::new("Error", message.clone())
            }
        }
    }
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CredentialError::Decrypt(reason) => write!(f, "{DECRYPT_ERROR_PREFIX}{reason}"),
            CredentialError::Corrupt(message) | CredentialError::Io(message) => {
                f.write_str(message)
            }
            CredentialError::InvalidKey => f.write_str("credential key must not be empty"),
        }
    }
}

impl std::error::Error for CredentialError {}

/// 持久化成功后的 Host 内部通知（Node `onDidMutate`），不进入 RPC。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialMutation {
    pub operation: &'static str,
    pub key: String,
}

/// AES-256-GCM 加解密（credentialCipherProvider.ts）。
#[derive(Clone)]
pub struct CredentialCipher {
    key: [u8; 32],
}

impl CredentialCipher {
    pub fn new(secret: &str) -> Self {
        Self {
            key: Sha256::digest(secret.as_bytes()).into(),
        }
    }

    fn sealing_key(&self) -> LessSafeKey {
        // 32 字节 key 对 AES-256-GCM 总是合法。
        LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &self.key).expect("AES-256 key length"))
    }

    pub fn encrypt(&self, value: &str) -> String {
        let mut iv = [0u8; IV_BYTES];
        // 熵源不可用时无法安全加密；与 Node randomBytes 抛错同样中止（不会静默写入弱 IV）。
        getrandom::fill(&mut iv).expect("system randomness for credential IV");
        self.encrypt_with_iv(value, iv)
    }

    /// 固定 IV 的加密，只给格式对拍测试用。
    #[doc(hidden)]
    pub fn encrypt_with_iv(&self, value: &str, iv: [u8; IV_BYTES]) -> String {
        let mut data = value.as_bytes().to_vec();
        let tag = self
            .sealing_key()
            .seal_in_place_separate_tag(Nonce::assume_unique_for_key(iv), Aad::empty(), &mut data)
            .expect("AES-GCM seal of an in-memory buffer");
        format!(
            "{ENCRYPTED_VALUE_PREFIX}{}.{}.{}",
            BASE64URL.encode(iv),
            BASE64URL.encode(tag.as_ref()),
            BASE64URL.encode(&data)
        )
    }

    pub fn decrypt(&self, value: &str) -> Result<String, CredentialError> {
        let Some(payload) = value.strip_prefix(ENCRYPTED_VALUE_PREFIX) else {
            return Ok(value.to_owned());
        };
        let parts: Vec<&str> = payload.split('.').collect();
        if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
            return Err(CredentialError::Decrypt("密文格式非法"));
        }
        let decode = |raw: &str| BASE64URL.decode(raw).unwrap_or_default();
        let (iv, tag, cipher) = (decode(parts[0]), decode(parts[1]), decode(parts[2]));
        let iv: [u8; IV_BYTES] = iv
            .try_into()
            .map_err(|_| CredentialError::Decrypt("IV 长度非法"))?;
        if tag.len() != TAG_BYTES {
            return Err(CredentialError::Decrypt("AuthTag 长度非法"));
        }
        let mut data = cipher;
        data.extend_from_slice(&tag);
        let plain = self
            .sealing_key()
            .open_in_place(Nonce::assume_unique_for_key(iv), Aad::empty(), &mut data)
            .map_err(|_| CredentialError::Decrypt("密钥不匹配或密文已损坏"))?;
        Ok(String::from_utf8_lossy(plain).into_owned())
    }
}

/// Node `os.platform()`。
pub fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// Node `os.homedir()`：POSIX 先看 `HOME`，Windows 看 `USERPROFILE`，都没有时查系统账户。
pub fn node_homedir(vars: &BTreeMap<String, String>) -> String {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    match vars.get(key).filter(|value| !value.is_empty()) {
        Some(value) => value.clone(),
        None => system_user().map(|user| user.1).unwrap_or_default(),
    }
}

/// `(username, homedir)` of the effective user（Node `os.userInfo()`）。
#[cfg(unix)]
fn system_user() -> Option<(String, String)> {
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    #[allow(unsafe_code)]
    // SAFETY: getpwuid_r 只写入调用方提供的 passwd 结构与缓冲区；返回成功且 result 非空时，
    // 其字段指向 buffer 内以 NUL 结尾的字符串，在 buffer 存活期间有效。
    unsafe {
        let mut passwd: libc::passwd = std::mem::zeroed();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let status = libc::getpwuid_r(
            libc::geteuid(),
            &mut passwd,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        );
        if status != 0 || result.is_null() || passwd.pw_name.is_null() {
            return None;
        }
        let name = std::ffi::CStr::from_ptr(passwd.pw_name)
            .to_string_lossy()
            .into_owned();
        let dir = if passwd.pw_dir.is_null() {
            String::new()
        } else {
            std::ffi::CStr::from_ptr(passwd.pw_dir)
                .to_string_lossy()
                .into_owned()
        };
        Some((name, dir))
    }
}

#[cfg(not(unix))]
fn system_user() -> Option<(String, String)> {
    None
}

/// Node `userInfo().username`；Windows 上 libuv 用 GetUserNameW，与 `USERNAME` 一致。
fn node_username(vars: &BTreeMap<String, String>) -> String {
    if cfg!(windows) {
        return vars
            .get("USERNAME")
            .cloned()
            .unwrap_or_else(|| "unknown".to_owned());
    }
    system_user()
        .map(|user| user.0)
        .unwrap_or_else(|| "unknown".to_owned())
}

/// `defaultCredentialSecret`：显式 secret 优先，否则由平台、home 与用户名派生。
pub fn credential_secret(vars: &BTreeMap<String, String>) -> String {
    if let Some(secret) = vars.get(SECRET_ENV_KEY).filter(|value| !value.is_empty()) {
        return secret.clone();
    }
    format!(
        "zcode-credential-fallback:{}:{}:{}",
        node_platform(),
        node_homedir(vars),
        node_username(vars)
    )
}

#[derive(Clone)]
pub struct CredentialStore {
    file: PathBuf,
    cipher: CredentialCipher,
    mutations: EventHub<CredentialMutation>,
}

/// 记录读取结果：保持文件里的键序，写回时与 Node 的对象展开一致。
type Record = Map<String, Value>;

async fn backup_corrupt_file(file: &Path) -> std::io::Result<PathBuf> {
    let content = tokio::fs::read(file).await?;
    let digest = Sha256::digest(&content);
    let id: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let backup = PathBuf::from(format!("{}.corrupt-{}.bak", file.display(), &id[..24]));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    match options.open(&backup).await {
        Ok(mut handle) => {
            use tokio::io::AsyncWriteExt;
            handle.write_all(&content).await?;
            handle.flush().await?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o600)).await?;
    }
    Ok(backup)
}

impl CredentialStore {
    pub fn new(app_config_dir: &Path, secret: &str) -> Self {
        Self {
            file: app_config_dir.join("credentials.json"),
            cipher: CredentialCipher::new(secret),
            // 订阅者是远端预配（凭据变化后重新下发），一次写入一条；落后 256 条说明它卡住，结束它的订阅
            // （docs/specs/rust-u2-task-scope.md §6.2）。
            mutations: EventHub::new("credential-mutations", 256, Overflow::CloseSubscriber),
        }
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    pub fn mutations(&self) -> EventHub<CredentialMutation> {
        self.mutations.clone()
    }

    async fn read_all(&self) -> Result<Record, CredentialError> {
        let raw = match tokio::fs::read_to_string(&self.file).await {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
            Err(_) => {
                return Err(CredentialError::Io(format!(
                    "Unable to read ZCode credentials: {}",
                    self.file.display()
                )));
            }
        };
        let parsed = serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|value| match value {
                Value::Object(record) if record.values().all(Value::is_string) => Some(record),
                _ => None,
            });
        match parsed {
            Some(record) => Ok(record),
            None => {
                // 把损坏文件当空 store 再保存会清空其他 OAuth 与登录凭据：保留证据并报错。
                let backup = backup_corrupt_file(&self.file).await.ok();
                log::warn(format!(
                    "credentialService read failed; refusing to overwrite corrupt credential store {} (backup {:?})",
                    self.file.display(),
                    backup
                ));
                Err(CredentialError::Corrupt(format!(
                    "ZCode credentials are corrupt: {}",
                    self.file.display()
                )))
            }
        }
    }

    async fn write_all(&self, record: &Record) -> Result<(), CredentialError> {
        let mut text = serde_json::to_string_pretty(record)
            .map_err(|error| CredentialError::Io(error.to_string()))?;
        text.push('\n');
        atomic_write_private_text(&self.file, &text)
            .await
            .map_err(|error| CredentialError::Io(format!("{error:#}")))
    }

    pub async fn load(&self, key: &str) -> Result<Option<String>, CredentialError> {
        if key.is_empty() {
            return Err(CredentialError::InvalidKey);
        }
        let record = self.read_all().await?;
        match record.get(key).and_then(Value::as_str) {
            None => Ok(None),
            Some(raw) => self.cipher.decrypt(raw).map(Some),
        }
    }

    /// 读出并解密 `select` 选中的键（文件里的顺序）。Provider Provisioning 用它导出 allowlist 凭据
    /// （providerProvisioningSource.ts），解密只有这一份实现。
    pub async fn load_selected(
        &self,
        select: impl Fn(&str) -> bool,
    ) -> Result<Vec<(String, String)>, CredentialError> {
        let record = self.read_all().await?;
        let mut selected = Vec::new();
        for (key, raw) in record {
            if !select(&key) {
                continue;
            }
            let raw = raw.as_str().unwrap_or_default();
            selected.push((key, self.cipher.decrypt(raw)?));
        }
        Ok(selected)
    }

    /// 读改写持锁；`mutate` 返回 false 时不写回（锁仍覆盖读取）。
    async fn mutate(
        &self,
        key: &str,
        operation: &'static str,
        apply: impl FnOnce(&mut Record) + Send,
    ) -> Result<(), CredentialError> {
        if key.is_empty() {
            return Err(CredentialError::InvalidKey);
        }
        let mut apply = Some(apply);
        let result: anyhow::Result<Result<(), CredentialError>> =
            with_file_lock(&self.file, &LockOptions::default(), async || {
                let mut record = match self.read_all().await {
                    Ok(record) => record,
                    Err(error) => return Ok(Err(error)),
                };
                if let Some(apply) = apply.take() {
                    apply(&mut record);
                }
                Ok(self.write_all(&record).await)
            })
            .await;
        match result {
            Ok(Ok(())) => {
                self.mutations.emit(CredentialMutation {
                    operation,
                    key: key.to_owned(),
                });
                Ok(())
            }
            Ok(Err(error)) => Err(error),
            Err(error) => Err(CredentialError::Io(format!("{error:#}"))),
        }
    }

    pub async fn save(&self, key: &str, value: &str) -> Result<(), CredentialError> {
        let encrypted = self.cipher.encrypt(value);
        let owned = key.to_owned();
        self.mutate(key, "save", move |record| {
            record.insert(owned, Value::String(encrypted));
        })
        .await
    }

    pub async fn delete(&self, key: &str) -> Result<(), CredentialError> {
        let owned = key.to_owned();
        self.mutate(key, "delete", move |record| {
            // JS `delete` 保持其余键的相对顺序。
            record.shift_remove(&owned);
        })
        .await
    }
}

pub fn service(store: CredentialStore) -> ServiceBuilder {
    let (load, save, delete) = (store.clone(), store.clone(), store);
    ServiceBuilder::new("credential")
        .method("load", move |_, args| {
            let store = load.clone();
            async move {
                let key: String = args.get(0, "key")?;
                match store.load(&key).await {
                    Ok(value) => Ok(Reply::of(&value)),
                    Err(error) => Err(error.to_rpc()),
                }
            }
        })
        .method("save", move |_, args| {
            let store = save.clone();
            async move {
                let key: String = args.get(0, "key")?;
                let value: String = args.get(1, "value")?;
                store
                    .save(&key, &value)
                    .await
                    .map(|()| Reply::Undefined)
                    .map_err(|error| error.to_rpc())
            }
        })
        .method("delete", move |_, args| {
            let store = delete.clone();
            async move {
                let key: String = args.get(0, "key")?;
                store
                    .delete(&key)
                    .await
                    .map(|()| Reply::Undefined)
                    .map_err(|error| error.to_rpc())
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &Path) -> CredentialStore {
        CredentialStore::new(dir, "test-secret")
    }

    #[test]
    fn cipher_roundtrip_and_errors_match_node() {
        let cipher = CredentialCipher::new("s");
        let encrypted = cipher.encrypt("hello 世界");
        assert!(encrypted.starts_with("enc:v1:"));
        assert!(!encrypted.contains('='));
        assert_eq!(cipher.decrypt(&encrypted).unwrap(), "hello 世界");
        assert_eq!(cipher.decrypt("plain").unwrap(), "plain");
        assert_eq!(
            cipher.decrypt("enc:v1:a.b").unwrap_err(),
            CredentialError::Decrypt("密文格式非法")
        );
        assert_eq!(
            cipher.decrypt("enc:v1:AAAA.AAAA.AAAA").unwrap_err(),
            CredentialError::Decrypt("IV 长度非法")
        );
        let other = CredentialCipher::new("other");
        assert_eq!(
            other.decrypt(&encrypted).unwrap_err(),
            CredentialError::Decrypt("密钥不匹配或密文已损坏")
        );
        let rpc = other.decrypt(&encrypted).unwrap_err().to_rpc();
        assert_eq!(rpc.code_str(), Some(DECRYPT_ERROR_CODE));
    }

    #[tokio::test]
    async fn save_load_delete_keep_order_and_private_mode() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        assert_eq!(store.load("a").await.unwrap(), None);
        store.save("a", "1").await.unwrap();
        store.save("b", "2").await.unwrap();
        store.save("c", "3").await.unwrap();
        store.delete("a").await.unwrap();
        store.save("a", "4").await.unwrap();
        store.save("b", "5").await.unwrap();
        let text = std::fs::read_to_string(store.file()).unwrap();
        let keys: Vec<String> = serde_json::from_str::<Map<String, Value>>(&text)
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(keys, ["b", "c", "a"]);
        assert!(text.starts_with("{\n  \"b\": \"enc:v1:"));
        assert!(text.ends_with("\"\n}\n"));
        assert_eq!(store.load("b").await.unwrap().as_deref(), Some("5"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.file())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(
            store.load("").await.unwrap_err(),
            CredentialError::InvalidKey
        );
    }

    #[tokio::test]
    async fn corrupt_file_is_backed_up_and_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        std::fs::write(store.file(), "{not json").unwrap();
        let error = store.save("a", "1").await.unwrap_err();
        assert!(matches!(error, CredentialError::Corrupt(_)));
        assert_eq!(std::fs::read_to_string(store.file()).unwrap(), "{not json");
        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("credentials.json.corrupt-") && name.ends_with(".bak"))
            .collect();
        assert_eq!(backups.len(), 1);
        std::fs::write(store.file(), r#"{"a": 1}"#).unwrap();
        assert!(matches!(
            store.load("a").await.unwrap_err(),
            CredentialError::Corrupt(_)
        ));
    }

    #[tokio::test]
    async fn concurrent_writers_do_not_lose_updates() {
        let dir = tempfile::tempdir().unwrap();
        let first = store(dir.path());
        let second = store(dir.path());
        let mut tasks = Vec::new();
        for index in 0..10 {
            let writer = if index % 2 == 0 {
                first.clone()
            } else {
                second.clone()
            };
            tasks.push(tokio::spawn(async move {
                writer
                    .save(&format!("k{index}"), &format!("v{index}"))
                    .await
            }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        for index in 0..10 {
            assert_eq!(
                first.load(&format!("k{index}")).await.unwrap(),
                Some(format!("v{index}"))
            );
        }
    }

    #[test]
    fn secret_prefers_explicit_env() {
        let mut vars = BTreeMap::new();
        vars.insert(SECRET_ENV_KEY.to_owned(), "x".to_owned());
        assert_eq!(credential_secret(&vars), "x");
        vars.insert(SECRET_ENV_KEY.to_owned(), String::new());
        vars.insert("HOME".to_owned(), "/home/u".to_owned());
        vars.insert("USERPROFILE".to_owned(), "C:\\Users\\u".to_owned());
        let secret = credential_secret(&vars);
        assert!(secret.starts_with(&format!("zcode-credential-fallback:{}:", node_platform())));
        let home = if cfg!(windows) {
            "C:\\Users\\u"
        } else {
            "/home/u"
        };
        assert!(secret.contains(&format!(":{home}:")));
    }
}
