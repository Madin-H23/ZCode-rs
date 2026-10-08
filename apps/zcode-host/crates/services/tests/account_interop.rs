//! 凭据文件与 TS 实现的交叉读写（docs/specs/rust-host-account.md §2.1）。
//!
//! 用真实的 `createCredentialService`（经 tsx 运行 scripts/rust-host/credential-interop.ts）
//! 读写同一个临时目录里的 credentials.json：Rust 写的 TS 能读，TS 写的 Rust 能读，
//! 两边并发写入都不丢更新，固定 IV 的密文逐字节相同。没有安装 node_modules 时跳过。
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use zcode_host_services::account::credential::{CredentialCipher, CredentialStore};

const SECRET: &str = "interop-secret";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../..")
        .canonicalize()
        .unwrap()
}

fn tsx_available(root: &Path) -> bool {
    root.join("node_modules/.bin/tsx").exists()
}

fn run_ts(root: &Path, home: &Path, mode: &str, payload: Value) -> Value {
    let output = Command::new("node")
        .current_dir(root)
        .args(["--import", "tsx", "scripts/rust-host/credential-interop.ts", mode])
        .arg(payload.to_string())
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("ZCODE_DATA_BASE_DIR", home)
        .env("ZCODE_DESKTOP_HOME_DIR", home)
        .env("ZCODE_CREDENTIAL_SECRET", SECRET)
        .output()
        .expect("run node");
    assert!(
        output.status.success(),
        "TS interop failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.lines().last().unwrap_or("null")).unwrap()
}

fn store(home: &Path) -> CredentialStore {
    CredentialStore::new(&home.join(".zcode").join("v2"), SECRET)
}

#[tokio::test]
async fn credentials_cross_read_and_write_with_typescript() {
    let root = repo_root();
    if !tsx_available(&root) {
        eprintln!("skipped: node_modules/.bin/tsx missing (run pnpm install)");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let rust = store(home);

    // Rust 写 → TS 读。
    rust.save("oauth:active_provider", "zai").await.unwrap();
    rust.save("zcodejwttoken", "jwt-值-1").await.unwrap();
    let read = run_ts(
        &root,
        home,
        "read",
        json!(["oauth:active_provider", "zcodejwttoken", "missing"]),
    );
    assert_eq!(
        read,
        json!({"oauth:active_provider": "zai", "zcodejwttoken": "jwt-值-1", "missing": null})
    );

    // TS 写 / 删 → Rust 读，键序沿用 TS 的对象语义。
    run_ts(
        &root,
        home,
        "write",
        json!({"oauth:zai:access_token": "token-ts", "zcodejwttoken": null}),
    );
    assert_eq!(
        rust.load("oauth:zai:access_token").await.unwrap().as_deref(),
        Some("token-ts")
    );
    assert_eq!(rust.load("zcodejwttoken").await.unwrap(), None);
    let text = std::fs::read_to_string(rust.file()).unwrap();
    let keys: Vec<String> = serde_json::from_str::<serde_json::Map<String, Value>>(&text)
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(keys, ["oauth:active_provider", "oauth:zai:access_token"]);

    // 换 secret 后两边给出同一个解密错误码。
    let other = CredentialStore::new(&home.join(".zcode").join("v2"), "wrong");
    let error = other.load("oauth:active_provider").await.unwrap_err();
    assert!(error.is_decrypt());

    // 并发写入：TS 与 Rust 各写 15 个键，持同一目录锁，全部保留。
    let ts_keys: serde_json::Map<String, Value> = (0..15)
        .map(|index| (format!("ts:{index}"), json!(format!("t{index}"))))
        .collect();
    let ts_root = root.clone();
    let ts_home = home.to_path_buf();
    let ts_writer = std::thread::spawn(move || {
        run_ts(&ts_root, &ts_home, "write", Value::Object(ts_keys));
    });
    for index in 0..15 {
        rust.save(&format!("rs:{index}"), &format!("r{index}"))
            .await
            .unwrap();
    }
    ts_writer.join().unwrap();
    for index in 0..15 {
        assert_eq!(
            rust.load(&format!("ts:{index}")).await.unwrap(),
            Some(format!("t{index}"))
        );
        assert_eq!(
            rust.load(&format!("rs:{index}")).await.unwrap(),
            Some(format!("r{index}"))
        );
    }
}

#[test]
fn fixed_iv_ciphertext_matches_node_crypto() {
    let root = repo_root();
    if !tsx_available(&root) {
        eprintln!("skipped: node_modules/.bin/tsx missing (run pnpm install)");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let iv = [7u8, 1, 2, 3, 4, 5, 6, 8, 9, 10, 11, 250];
    for plaintext in ["", "a", "zcode-jwt.eyJhbGciOi.世界 🎉"] {
        let expected = run_ts(
            &root,
            temp.path(),
            "vector",
            json!({"secret": SECRET, "ivHex": iv.iter().map(|b| format!("{b:02x}")).collect::<String>(), "plaintext": plaintext}),
        );
        let actual = CredentialCipher::new(SECRET).encrypt_with_iv(plaintext, iv);
        assert_eq!(Value::String(actual.clone()), expected, "{plaintext:?}");
        let decrypted = CredentialCipher::new(SECRET).decrypt(&actual);
        if plaintext.is_empty() {
            // Node 把空密文段判为格式非法（`!cipherRaw`），空值存进去读不回来；两边一致。
            assert!(decrypted.unwrap_err().is_decrypt());
        } else {
            assert_eq!(decrypted.unwrap(), plaintext);
        }
    }
}
