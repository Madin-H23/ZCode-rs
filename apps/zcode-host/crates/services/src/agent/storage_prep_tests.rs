use std::sync::{Arc, Mutex};

use super::*;

fn script_command(script: &str) -> AgentCommand {
    AgentCommand {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into(), "sh".into()],
        cwd: None,
        env: BTreeMap::new(),
        supports_storage_startup: true,
        storage_entry: None,
    }
}

fn env() -> BTreeMap<String, String> {
    BTreeMap::from([("PATH".to_owned(), "/usr/bin:/bin".to_owned())])
}

fn frames(db: &str, extra: &str) -> String {
    let state = |seq: u32, phase: &str| {
        format!(
            r#"{{"method":"startup/storageState","params":{{"schemaVersion":1,"attemptId":"a","sequence":{seq},"databaseId":"{db}","databaseKind":"session","phase":"{phase}","elapsedMs":0{extra}}}}}"#
        )
    };
    format!("{}\\n{}\\n", state(1, "checking"), state(2, "ready"))
}

/// 模拟 `--prepare-storage`：报告路径、把 Host 的应答写进 reply 文件、报告阶段、prepared。
fn happy_script(db_path: &Path, reply_file: &Path) -> String {
    format!(
        r#"[ "$1" = "--prepare-storage" ] || exit 9
printf '%s\n' '{{"method":"startup/storagePath","params":{{"path":"{db}"}}}}'
read line; printf '%s\n' "$line" >> "{reply}"
printf '{frames}'
printf '%s\n' '{{"method":"startup/storagePrepared","params":{{}}}}'
cat > /dev/null
exit 0"#,
        db = db_path.display(),
        reply = reply_file.display(),
        frames = frames(
            "db1",
            r#","migration":{"kind":"initialize","executedCount":1,"committedCount":1,"lastAppliedMigrationId":null}"#
        ),
    )
}

#[cfg(unix)]
#[tokio::test]
async fn prepares_and_reuses_the_same_database() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("sessions.db");
    let reply = dir.path().join("reply.txt");
    let command = script_command(&happy_script(&db, &reply));
    let reports: Arc<Mutex<Vec<Progress>>> = Arc::default();
    let sink = reports.clone();
    let report = move |progress: Progress| sink.lock().unwrap().push(progress);
    let mut prepared = HashSet::new();
    prepare_directory(&command, dir.path(), &env(), &mut prepared, false, &report)
        .await
        .unwrap();
    prepare_directory(&command, dir.path(), &env(), &mut prepared, true, &report)
        .await
        .unwrap();
    let replies = std::fs::read_to_string(&reply).unwrap();
    let lines: Vec<&str> = replies.lines().collect();
    assert_eq!(
        lines,
        [
            r#"{"method":"startup/storagePathReady","reuse":false}"#,
            r#"{"method":"startup/storagePathReady","reuse":true}"#
        ]
    );
    let reports = reports.lock().unwrap();
    assert_eq!(reports.len(), 4);
    assert_eq!(reports[0].database_phase, "checking");
    assert_eq!(reports[1].database_phase, "ready");
    assert_eq!(reports[0].database_id, "db1");
    assert!(!reports[1].final_database);
    assert!(reports[3].final_database);
    assert_eq!(reports[0].migration.as_ref().unwrap()["kind"], "initialize");
}

#[cfg(unix)]
#[tokio::test]
async fn failed_frames_and_protocol_errors_are_classified() {
    let dir = tempfile::tempdir().unwrap();
    let failed = format!(
        r#"printf '%s\n' '{{"method":"startup/storagePath","params":{{"path":"/tmp/x.db"}}}}'
read line
printf '%s\n' '{{"method":"startup/storageState","params":{{"schemaVersion":1,"attemptId":"a","sequence":1,"databaseId":"db","databaseKind":"session","phase":"failed","elapsedMs":0,"errorCode":"sql_failed","sqliteCode":11,"migrationId":"0002_x"}}}}'
exit 1"#
    );
    let mut prepared = HashSet::new();
    let error = prepare_directory(
        &script_command(&failed),
        dir.path(),
        &env(),
        &mut prepared,
        true,
        &|_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(error.error_code, "corrupt");
    assert_eq!(error.sqlite_code, Some(11));
    assert_eq!(error.migration_id.as_deref(), Some("0002_x"));

    let garbage = script_command("printf 'not json\\n'; sleep 5");
    let started = std::time::Instant::now();
    let error = prepare_directory(&garbage, dir.path(), &env(), &mut prepared, true, &|_| {})
        .await
        .unwrap_err();
    assert_eq!(error.error_code, "transport_closed");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "process tree was reclaimed"
    );

    let quiet_exit = script_command("exit 0");
    let error = prepare_directory(
        &quiet_exit,
        dir.path(),
        &env(),
        &mut prepared,
        true,
        &|_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(error.error_code, "transport_closed");

    let mut unsupported = script_command("exit 0");
    unsupported.supports_storage_startup = false;
    let error = prepare_directory(
        &unsupported,
        dir.path(),
        &env(),
        &mut prepared,
        true,
        &|_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(error.error_code, "unsupported_runtime");
}

#[test]
fn classification_follows_typescript() {
    assert_eq!(classify("sql_failed", Some(13), None), "storage_full");
    assert_eq!(
        classify("sql_failed", None, Some("EACCES")),
        "permission_denied"
    );
    assert_eq!(classify("sql_failed", Some(266), None), "io_error");
    assert_eq!(classify("sql_failed", Some(14), None), "open_failed");
    assert_eq!(
        classify("checksum_mismatch", Some(5), None),
        "checksum_mismatch"
    );
    assert_eq!(classify("sql_failed", Some(5), None), "lock_timeout");
    assert_eq!(classify("weird", None, None), "sql_failed");
    assert_eq!(
        classify("startup_status_timeout", None, None),
        "startup_status_timeout"
    );
}

#[tokio::test]
async fn candidates_fall_back_to_the_spawn_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let fallback = dir.path().join("fallback");
    let real = dir.path().display().to_string();
    let missing = dir.path().join("missing").display().to_string();
    let directories = candidate_directories(
        &[real.clone(), missing, real.clone()],
        Some("/ignored"),
        &fallback,
    )
    .await;
    assert_eq!(directories, [dir.path().to_owned(), fallback.clone()]);
    assert_eq!(
        candidate_directories(&[], Some(&real), &fallback).await,
        [dir.path().to_owned()]
    );
    assert_eq!(
        candidate_directories(&[], None, &fallback).await,
        [fallback]
    );
}

#[test]
fn migration_facts_merge_like_the_coordinator() {
    let mut facts = MigrationFacts::default();
    facts.record("db1", None);
    facts.record("db1", Some(&json!({"kind": "upgrade", "executedCount": 0, "committedCount": 0, "lastAppliedMigrationId": "0001"})));
    facts.record(
        "db1",
        Some(&json!({"kind": "none", "executedCount": 2, "committedCount": 2})),
    );
    facts.record("db2", Some(&json!({"kind": "initialize", "executedCount": 1, "committedCount": 0, "lastAppliedMigrationId": null})));
    assert_eq!(
        facts.aggregate().unwrap(),
        json!({"kind": "upgrade", "executedCount": 3, "committedCount": 2})
    );
    assert_eq!(
        facts.baselines(),
        vec![
            json!({"databaseId": "db1", "databaseKind": "session", "lastAppliedMigrationId": "0001"}),
            json!({"databaseId": "db2", "databaseKind": "session", "lastAppliedMigrationId": null}),
        ]
    );
}
