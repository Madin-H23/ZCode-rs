use super::*;
use crate::domain::config::ConfigSnapshot;
use serde_json::json;

fn paths(storage: serde_json::Value) -> StoragePaths {
    let config = ConfigSnapshot {
        config: json!({ "storage": storage }),
        ..ConfigSnapshot::default()
    };
    resolve(Path::new("/work"), Path::new("/home/u"), &config)
}

#[test]
fn defaults_live_under_home() {
    let resolved = paths(json!({}));
    assert_eq!(
        (resolved.database, resolved.artifacts),
        (
            PathBuf::from("/home/u/.zcode/cli/db/db.sqlite"),
            PathBuf::from("/home/u/.zcode/cli/artifacts"),
        )
    );
}

#[test]
fn tilde_expands_against_home_and_relative_paths_stay_under_cwd() {
    let resolved = paths(json!({ "dir": "~", "sessionDbPath": "data/db.sqlite" }));
    assert_eq!(
        (resolved.database, resolved.artifacts),
        (
            PathBuf::from("/work/data/db.sqlite"),
            PathBuf::from("/home/u/cli/artifacts"),
        )
    );
    let absolute = paths(json!({ "dir": "/srv/z", "sessionDbPath": "~/db.sqlite" }));
    assert_eq!(
        (absolute.database, absolute.artifacts),
        (
            PathBuf::from("/home/u/db.sqlite"),
            PathBuf::from("/srv/z/cli/artifacts"),
        )
    );
}
