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
