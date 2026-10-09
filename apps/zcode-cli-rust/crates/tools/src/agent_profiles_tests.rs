//! The storage root comes from the runtime's snapshot and home, never the
//! process environment (spec rust-u1-runtime-library L4).
use super::*;
use std::sync::Arc;
use zcode_cli_net::{NetworkPolicy, RuntimeEnv};

fn egress(vars: Vec<(String, String)>, home: &Path) -> Egress {
    let env = Arc::new(RuntimeEnv::from_vars(vars));
    Egress::new(env, &NetworkPolicy::default(), home, home, "electron").unwrap()
}

#[test]
fn storage_dir_comes_from_the_snapshot_then_config_then_home() {
    let home = Path::new("/home/u");
    let configured = json!({"storage":{"dir":"data"}});
    let only_snapshot = egress(
        vec![("ZCODE_STORAGE_DIR".into(), "~/snapshot".into())],
        home,
    );
    assert_eq!(storage(&only_snapshot, &configured), home.join("snapshot"));
    assert_eq!(
        storage(&egress(vec![], home), &configured),
        home.join("data")
    );
    assert_eq!(
        storage(&egress(vec![], home), &json!({})),
        home.join(".zcode")
    );
}

#[tokio::test]
async fn user_profiles_are_read_from_the_snapshot_storage_dir() {
    let root = tempfile::tempdir().unwrap();
    let storage = root.path().join("storage");
    tokio::fs::create_dir_all(storage.join("agents"))
        .await
        .unwrap();
    tokio::fs::write(
        storage.join("agents/reviewer.md"),
        "---\nname: snapshot-reviewer\ndescription: Reviews\n---\nReview carefully.\n",
    )
    .await
    .unwrap();
    let vars = vec![(
        "ZCODE_STORAGE_DIR".into(),
        storage.to_string_lossy().into_owned(),
    )];
    let egress = egress(vars, &root.path().join("home"));
    let cwd = root.path().join("workspace");
    let profiles = discover(&egress, &cwd, &json!({}), &CancellationToken::new())
        .await
        .unwrap();
    assert!(
        profiles
            .iter()
            .any(|p| p.name == "snapshot-reviewer" && p.source == "user"),
        "{:?}",
        profiles.iter().map(|p| &p.name).collect::<Vec<_>>()
    );
}
