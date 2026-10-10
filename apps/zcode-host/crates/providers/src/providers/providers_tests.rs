use std::sync::Mutex;

use super::*;

#[tokio::test]
async fn revision_increases_only_when_files_change() {
    let dir = tempfile::tempdir().unwrap();
    let personal = dir.path().join("provider_config.json");
    let runtime =
        ProviderRuntime::new(&zcode_host_base::testing::spawner(), None, personal.clone());
    assert_eq!(runtime.snapshot().await.0, 1);
    assert_eq!(runtime.reload().await.0, 1);
    let mut changes = Box::pin(runtime.changes().subscribe());
    std::fs::write(&personal, r#"{"schemaVersion":1,"config":{}}"#).unwrap();
    assert_eq!(runtime.reload().await.0, 2);
    assert_eq!(changes.next().await, Some(()));
}

/// 随包内置配置在、个人配置不在的一份 runtime，已完成首轮轮询。
async fn polled_runtime() -> (tempfile::TempDir, ProviderRuntime, ChangeGate) {
    let dir = tempfile::tempdir().unwrap();
    let builtin = dir.path().join("zcode-builtin.json");
    std::fs::write(&builtin, r#"{"schemaVersion":1,"revision":1,"config":{}}"#).unwrap();
    let runtime = ProviderRuntime::new(
        &zcode_host_base::testing::spawner(),
        Some(builtin),
        dir.path().join("provider_config.json"),
    );
    let mut gate = ChangeGate::new(FULL_READ_EVERY);
    assert!(runtime.poll(&mut gate).await);
    assert!(!runtime.poll(&mut gate).await);
    (dir, runtime, gate)
}

/// 每秒的轮询先比元数据：没变就不读；出现、原地改写、rename 覆盖、删除都在下一轮读到
/// （docs/specs/rust-provider-config-poll.md）。
#[tokio::test]
async fn polling_reads_only_when_the_files_appear_change_get_replaced_or_vanish() {
    let (dir, runtime, mut gate) = polled_runtime().await;
    let personal = dir.path().join("provider_config.json");
    let mut revisions = vec![runtime.snapshot().await.0];
    std::fs::write(&personal, r#"{"schemaVersion":1,"config":{}}"#).unwrap();
    assert!(runtime.poll(&mut gate).await);
    revisions.push(runtime.snapshot().await.0);
    std::fs::write(&personal, r#"{"schemaVersion":1,"config":{ }}"#).unwrap();
    assert!(runtime.poll(&mut gate).await);
    revisions.push(runtime.snapshot().await.0);
    let temp = dir.path().join(".provider_config.json.tmp");
    std::fs::write(&temp, r#"{"schemaVersion":1,"config":{  }}"#).unwrap();
    std::fs::rename(&temp, &personal).unwrap();
    assert!(runtime.poll(&mut gate).await);
    revisions.push(runtime.snapshot().await.0);
    std::fs::remove_file(&personal).unwrap();
    assert!(runtime.poll(&mut gate).await);
    revisions.push(runtime.snapshot().await.0);
    assert!(!runtime.poll(&mut gate).await);
    assert_eq!(revisions, vec![1, 2, 3, 4, 5]);
}

/// 原子写换了 inode：长度与 mtime 都和原来相同也读得到。
#[cfg(unix)]
#[tokio::test]
async fn polling_reads_a_same_size_replacement_with_the_old_mtime() {
    let (dir, runtime, mut gate) = polled_runtime().await;
    let builtin = dir.path().join("zcode-builtin.json");
    let modified = std::fs::metadata(&builtin).unwrap().modified().unwrap();
    let temp = dir.path().join(".zcode-builtin.json.tmp");
    std::fs::write(&temp, r#"{"schemaVersion":1,"revision":2,"config":{}}"#).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&temp)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    std::fs::rename(&temp, &builtin).unwrap();
    assert!(runtime.poll(&mut gate).await);
    assert_eq!(runtime.snapshot().await.0, 2);
}

/// 读文件出错（不是“不存在”）时不记录元数据：下一轮照常重读，与不比元数据时相同。
#[tokio::test]
async fn polling_retries_every_tick_while_a_file_cannot_be_read() {
    let (dir, runtime, mut gate) = polled_runtime().await;
    // 目录读不成文件内容：Unix 是 EISDIR，Windows 是拒绝访问。
    std::fs::create_dir(dir.path().join("provider_config.json")).unwrap();
    assert!(runtime.poll(&mut gate).await);
    assert!(runtime.poll(&mut gate).await);
    std::fs::remove_dir(dir.path().join("provider_config.json")).unwrap();
    std::fs::write(
        dir.path().join("provider_config.json"),
        r#"{"schemaVersion":1,"config":{}}"#,
    )
    .unwrap();
    assert!(runtime.poll(&mut gate).await);
    assert!(!runtime.poll(&mut gate).await);
}

/// Endpoint 切换换了 Active 路径：文件都没变，轮询也要读，revision（含路径）随之更新。
#[tokio::test]
async fn polling_follows_an_endpoint_switch_to_another_active_file() {
    let dir = tempfile::tempdir().unwrap();
    let bundled = dir.path().join("bundled.json");
    let release = json!({"schemaVersion": 1, "revision": 5, "config": {
        "providerConfigRules": {"templateRules": [], "providerRules": []},
        "modelConfigRules": {},
    }});
    std::fs::write(&bundled, release.to_string()).unwrap();
    let runtime = ProviderRuntime::new(
        &zcode_host_base::testing::spawner(),
        Some(bundled),
        dir.path().join("provider_config.json"),
    );
    let origin = Arc::new(Mutex::new("http://127.0.0.1:9".to_owned()));
    let resolve_origin: builtin::ResolveOrigin = {
        let origin = origin.clone();
        Arc::new(move || {
            let origin = origin.lock().unwrap().clone();
            Box::pin(async move { Ok(origin) })
        })
    };
    runtime.set_builtin_environment(builtin::BuiltinEnvironment {
        config_root: dir.path().join("v2"),
        platform: "darwin-aarch64".into(),
        app_version: "1.2.3".into(),
        resolve_origin,
        fetch_release: Arc::new(|_| Box::pin(async { Ok(None) })),
    });
    let mut gate = ChangeGate::new(FULL_READ_EVERY);
    // 首读物化 Active（写了新文件），下一轮看到它出现再读一次，之后不再读。
    assert!(runtime.poll(&mut gate).await);
    assert!(runtime.poll(&mut gate).await);
    assert!(!runtime.poll(&mut gate).await);
    let (before, _) = runtime.builtin_snapshot().await;
    *origin.lock().unwrap() = "http://127.0.0.1:10".to_owned();
    assert!(runtime.poll(&mut gate).await);
    let (after, _) = runtime.builtin_snapshot().await;
    assert_ne!(after, before);
    assert_eq!(
        runtime.builtin_file(),
        Some(
            builtin::cache_paths(
                &dir.path().join("v2"),
                "darwin-aarch64",
                "1.2.3",
                "http://127.0.0.1:10"
            )
            .unwrap()
            .active
        )
    );
}
