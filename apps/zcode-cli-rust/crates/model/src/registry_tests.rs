//! Provider files come from the runtime's environment snapshot, relative to
//! its base directory (spec rust-u1-runtime-library L4).
use super::*;
use zcode_cli_net::{Egress, NetworkPolicy, RuntimeEnv};

fn egress(vars: Vec<(String, String)>, base: &std::path::Path) -> Arc<Egress> {
    let env = Arc::new(RuntimeEnv::from_vars(vars));
    let egress = Egress::new(env, &NetworkPolicy::default(), base, base, "electron").unwrap();
    Arc::new(egress)
}

#[tokio::test]
async fn provider_files_named_only_by_the_snapshot_are_loaded() {
    let base = tempfile::tempdir().unwrap();
    tokio::fs::write(
        base.path().join("builtin.json"),
        r#"{"schemaVersion":1,"revision":7,"config":{}}"#,
    )
    .await
    .unwrap();
    assert!(
        Registry::from_env(egress(vec![], base.path()))
            .await
            .unwrap()
            .is_none()
    );
    let vars = vec![
        (
            "ZCODE_BUILTIN_PROVIDER_CONFIG_FILE".into(),
            "builtin.json".into(),
        ),
        (
            "ZCODE_PERSONAL_PROVIDER_CONFIG_FILE".into(),
            "personal.json".into(),
        ),
    ];
    let registry = Registry::from_env(egress(vars, base.path()))
        .await
        .unwrap()
        .expect("registry from the snapshot");
    assert_eq!(
        (registry.builtin.clone(), registry.personal.clone()),
        (
            base.path().join("builtin.json"),
            base.path().join("personal.json")
        )
    );
    assert_eq!(registry.catalog(), Vec::<Value>::new());
}

const BUILTIN: &str = r#"{"schemaVersion":1,"revision":7,"config":{}}"#;

/// A registry over `builtin.json` (present) and `personal.json` (absent) in a
/// fresh directory, after its first read.
async fn polled_registry() -> (tempfile::TempDir, Registry) {
    let base = tempfile::tempdir().unwrap();
    std::fs::write(base.path().join("builtin.json"), BUILTIN).unwrap();
    let registry = Registry::new(
        "builtin.json".into(),
        "personal.json".into(),
        egress(vec![], base.path()),
    );
    assert!(registry.refresh(None).await.unwrap());
    assert!(!registry.refresh(None).await.unwrap());
    (base, registry)
}

/// The once-a-second poll reads the files again when their metadata says they
/// changed (spec rust-provider-config-poll); unchanged files are skipped.
#[tokio::test]
async fn polling_picks_up_files_that_appear_change_get_replaced_or_vanish() {
    let (base, registry) = polled_registry().await;
    let personal = base.path().join("personal.json");
    // 与缺失时的默认内容字节不同，才能看出读到了新文件。
    std::fs::write(&personal, r#"{"schemaVersion": 1, "config": {}}"#).unwrap();
    assert!(registry.refresh(None).await.unwrap());
    std::fs::write(&personal, r#"{"schemaVersion": 1, "config": { }}"#).unwrap();
    assert!(registry.refresh(None).await.unwrap());
    let temp = base.path().join(".personal.json.tmp");
    std::fs::write(&temp, r#"{"schemaVersion": 1,  "config": { }}"#).unwrap();
    std::fs::rename(&temp, &personal).unwrap();
    assert!(registry.refresh(None).await.unwrap());
    std::fs::remove_file(&personal).unwrap();
    assert!(registry.refresh(None).await.unwrap());
    assert!(!registry.refresh(None).await.unwrap());
}

/// An atomic replacement keeps size and mtime but not the inode.
#[cfg(unix)]
#[tokio::test]
async fn polling_picks_up_a_same_size_replacement_with_the_old_mtime() {
    let (base, registry) = polled_registry().await;
    let builtin = base.path().join("builtin.json");
    let modified = std::fs::metadata(&builtin).unwrap().modified().unwrap();
    let temp = base.path().join(".builtin.json.tmp");
    std::fs::write(&temp, r#"{"schemaVersion":1,"revision":8,"config":{}}"#).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&temp)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    std::fs::rename(&temp, &builtin).unwrap();
    assert!(registry.refresh(None).await.unwrap());
}

/// An account update reads the files even when their metadata is unchanged.
#[tokio::test]
async fn account_updates_bypass_the_metadata_check() {
    let (base, registry) = polled_registry().await;
    let path = std::path::absolute(base.path().join("builtin.json")).unwrap();
    let account = json!({
        "revision": "account:1",
        "basedOnZCodeBuiltinRevision": format!(
            "zcode-builtin:7:{:x}",
            Sha256::digest(path.to_string_lossy().as_bytes())
        ),
        "providers": {},
    });
    assert!(registry.refresh(Some(account.clone())).await.unwrap());
    assert_eq!(registry.received_account().await, Some(account));
    assert!(!registry.refresh(None).await.unwrap());
}
