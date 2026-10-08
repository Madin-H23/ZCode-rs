//! 与 Node 共用的凭据文件（credential/credentialService.ts + credentialCipherProvider.ts）：
//! `~/.zcode/v2/credentials.json`，值为 `enc:v1:<iv>.<tag>.<cipher>`（base64url，AES-256-GCM）。
//!
//! 密钥 = sha256(`ZCODE_CREDENTIAL_SECRET`，否则 `zcode-credential-fallback:<platform>:<homedir>:<username>`)，
//! 与 Node 逐字节相同，两边读写的是同一份文件。写入在跨进程文件锁内整读整写；文件损坏时备份并拒绝覆盖。
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio::fs;

use crate::store::{LockOptions, atomic_write_private_text, with_file_lock};

const PREFIX: &str = "enc:v1:";
const IV_BYTES: usize = 12;
const TAG_BYTES: usize = 16;
/// `CREDENTIAL_DECRYPT_ERROR_PREFIX`。
const DECRYPT_ERROR_PREFIX: &str = "凭据解密失败：";

/// Node `os.platform()`。
fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// Node `os.userInfo().username`：取进程有效用户的账户名，而不是环境变量。
#[cfg(unix)]
#[allow(unsafe_code)]
fn username() -> Option<String> {
    // SAFETY: getpwuid_r 写入调用方提供的缓冲区；返回的指针只在本函数内使用。
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
    std::env::var("USERNAME").ok()
}

pub struct CredentialCipher {
    key: LessSafeKey,
}

impl CredentialCipher {
    /// `secret` 为 `ZCODE_CREDENTIAL_SECRET`（非空时）；`home` 为 Node `os.homedir()`。
    pub fn new(secret: Option<&str>, home: &Path) -> Self {
        let secret = match secret.filter(|secret| !secret.is_empty()) {
            Some(secret) => secret.to_owned(),
            None => format!(
                "zcode-credential-fallback:{}:{}:{}",
                node_platform(),
                home.display(),
                username().unwrap_or_else(|| "unknown".into())
            ),
        };
        let digest = Sha256::digest(secret.as_bytes());
        let key = UnboundKey::new(&AES_256_GCM, &digest).expect("AES-256 key is 32 bytes");
        Self {
            key: LessSafeKey::new(key),
        }
    }

    pub fn encrypt(&self, value: &str) -> Result<String> {
        let mut iv = [0u8; IV_BYTES];
        SystemRandom::new()
            .fill(&mut iv)
            .map_err(|_| anyhow!("random IV unavailable"))?;
        let mut buffer = value.as_bytes().to_vec();
        let tag = self
            .key
            .seal_in_place_separate_tag(Nonce::assume_unique_for_key(iv), Aad::empty(), &mut buffer)
            .map_err(|_| anyhow!("credential encryption failed"))?;
        Ok(format!(
            "{PREFIX}{}.{}.{}",
            URL_SAFE_NO_PAD.encode(iv),
            URL_SAFE_NO_PAD.encode(tag.as_ref()),
            URL_SAFE_NO_PAD.encode(&buffer)
        ))
    }

    /// 非 `enc:v1:` 前缀的旧明文原样返回（同 TS）。
    pub fn decrypt(&self, value: &str) -> Result<String> {
        let Some(payload) = value.strip_prefix(PREFIX) else {
            return Ok(value.to_owned());
        };
        let fail = |reason: &str| anyhow!("{DECRYPT_ERROR_PREFIX}{reason}");
        let parts: Vec<&str> = payload.split('.').collect();
        if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
            return Err(fail("密文格式非法"));
        }
        // Node 的 base64url 解码宽松：容忍末尾 `=`。
        let decode = |part: &str| URL_SAFE_NO_PAD.decode(part.trim_end_matches('='));
        let iv = decode(parts[0]).map_err(|_| fail("IV 长度非法"))?;
        let tag = decode(parts[1]).map_err(|_| fail("AuthTag 长度非法"))?;
        let cipher = decode(parts[2]).map_err(|_| fail("密钥不匹配或密文已损坏"))?;
        if iv.len() != IV_BYTES {
            return Err(fail("IV 长度非法"));
        }
        if tag.len() != TAG_BYTES {
            return Err(fail("AuthTag 长度非法"));
        }
        let nonce = Nonce::try_assume_unique_for_key(&iv).map_err(|_| fail("IV 长度非法"))?;
        let mut buffer = cipher;
        buffer.extend_from_slice(&tag);
        let plain = self
            .key
            .open_in_place(nonce, Aad::empty(), &mut buffer)
            .map_err(|_| fail("密钥不匹配或密文已损坏"))?;
        String::from_utf8(plain.to_vec()).map_err(|_| fail("密钥不匹配或密文已损坏"))
    }
}

pub struct CredentialStore {
    file: PathBuf,
    cipher: CredentialCipher,
}

impl CredentialStore {
    pub fn new(dir: &Path, cipher: CredentialCipher) -> Self {
        Self {
            file: dir.join("credentials.json"),
            cipher,
        }
    }

    async fn read_all(&self) -> Result<Map<String, Value>> {
        let text = match fs::read_to_string(&self.file).await {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Unable to read ZCode credentials: {}", self.file.display())
                });
            }
        };
        let parsed = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .filter(|object| object.values().all(Value::is_string));
        match parsed {
            Some(object) => Ok(object),
            None => {
                let backup = backup_corrupt_file(&self.file).await.ok();
                zcode_host_kit::log::warn(format!(
                    "credential store read failed; refusing to overwrite corrupt credential store backup={backup:?}"
                ));
                bail!("ZCode credentials are corrupt: {}", self.file.display())
            }
        }
    }

    pub async fn load(&self, key: &str) -> Result<Option<String>> {
        if key.is_empty() {
            bail!("credential key must not be empty");
        }
        let all = self.read_all().await?;
        match all.get(key).and_then(Value::as_str) {
            Some(raw) => self.cipher.decrypt(raw).map(Some),
            None => Ok(None),
        }
    }

    pub async fn save(&self, key: &str, value: &str) -> Result<()> {
        if key.is_empty() {
            bail!("credential key must not be empty");
        }
        let encrypted = self.cipher.encrypt(value)?;
        with_file_lock(&self.file, &LockOptions::default(), async || {
            let mut all = self.read_all().await?;
            all.insert(key.to_owned(), Value::String(encrypted.clone()));
            write_all(&self.file, &all).await
        })
        .await
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        if key.is_empty() {
            bail!("credential key must not be empty");
        }
        with_file_lock(&self.file, &LockOptions::default(), async || {
            let mut all = self.read_all().await?;
            all.shift_remove(key);
            write_all(&self.file, &all).await
        })
        .await
    }
}

async fn write_all(file: &Path, all: &Map<String, Value>) -> Result<()> {
    atomic_write_private_text(
        file,
        &super::store::pretty_json(&Value::Object(all.clone())),
    )
    .await
}

/// `backupCorruptFile`：按内容命名的 0600 备份，已存在则复用。
async fn backup_corrupt_file(file: &Path) -> Result<PathBuf> {
    let content = fs::read(file).await?;
    let digest = Sha256::digest(&content);
    let id: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let backup = PathBuf::from(format!("{}.corrupt-{}.bak", file.display(), &id[..24]));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    match options.open(&backup).await {
        Ok(mut handle) => {
            use tokio::io::AsyncWriteExt;
            handle.write_all(&content).await?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    Ok(backup)
}
