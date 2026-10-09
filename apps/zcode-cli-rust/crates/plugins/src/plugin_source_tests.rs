//! Local plugin sources (spec rust-u1-runtime-library L4): relative paths
//! resolve against the request's working directory, never the process cwd.
use super::*;
use crate::http::{Get, Http, Response};
use tokio_util::sync::CancellationToken;

struct Offline;

#[async_trait::async_trait]
impl Http for Offline {
    async fn get(&self, _: Get<'_>, _: &CancellationToken) -> Result<Response> {
        bail!("offline")
    }
}

fn entry(source: Value) -> Entry {
    Entry {
        name: "local".into(),
        description: None,
        version: None,
        source: Some(source),
        cache_path: None,
        dependencies: None,
        strict: None,
        listing: None,
        raw: Value::Null,
    }
}

#[tokio::test]
async fn relative_local_sources_resolve_against_the_given_working_directory() {
    let storage = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let local = workspace.path().join("plugins/local");
    tokio::fs::create_dir_all(&local).await.unwrap();
    let git = crate::git::Git {
        binary: "git".into(),
        env: vec![],
    };
    let cancel = CancellationToken::new();
    let ports = Ports {
        storage: storage.path(),
        cwd: workspace.path(),
        cancel: &cancel,
        http: &Offline,
        git: &git,
    };
    for source in [
        Value::from("plugins/local"),
        serde_json::json!({"source":"directory","path":"./plugins/local"}),
    ] {
        let entry = entry(source);
        let input = Input {
            entry: &entry,
            marketplace: "m",
            source_root: None,
            manifest: None,
        };
        let resolved = resolve(&input, &ports).await.unwrap();
        assert_eq!((resolved.path, resolved.cleanup), (local.clone(), None));
    }
}
