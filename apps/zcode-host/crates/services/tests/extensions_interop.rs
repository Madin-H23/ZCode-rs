//! 扩展服务与 TS 实现的跨进程互操作（docs/specs/rust-host-extensions.md §3、§6）：
//! - Rust Host 与 Node 服务同时读改写 `~/.zcode/cli/config.json`，双方的修改都不丢；
//! - Rust 导出的 skill / plugin 归档能被 TS 解包，TS 导出的归档能被 Rust 导入。
//!
//! 需要仓库里的 node 与 tsx（`pnpm install`）；缺失时跳过。所有文件都在临时目录内。
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::{Value, json};
use zcode_host_services::extensions::{Roots, commands, mcp_sync, plugin_sync, skill_sync};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../..")
        .canonicalize()
        .unwrap()
}

#[expect(clippy::print_stderr, reason = "前置条件缺失时提示测试被跳过")]
fn node_available() -> bool {
    let root = repo_root();
    if !root.join("node_modules/tsx/package.json").exists() {
        eprintln!("skip: tsx is not installed (run pnpm install)");
        return false;
    }
    let ok = std::process::Command::new("node")
        .arg("--version")
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !ok {
        eprintln!("skip: node is not on PATH");
    }
    ok
}

fn interop(args: &[&str], home: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("node");
    command
        .current_dir(repo_root())
        .arg("--import")
        .arg("tsx")
        .arg("scripts/rust-host/extensions-interop.ts")
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("ZCODE_DATA_BASE_DIR", home)
        .stdout(Stdio::null());
    command
}

#[tokio::test]
async fn rust_and_typescript_writers_do_not_lose_config_updates() {
    if !node_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().canonicalize().unwrap();
    let config_dir = home.join(".zcode/cli");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.json"),
        "{\n  \"model\": \"keep\"\n}\n",
    )
    .unwrap();
    const COUNT: usize = 150;
    let count = COUNT.to_string();
    let mut child = interop(&["race", home.to_str().unwrap(), &count, "ts"], &home)
        .spawn()
        .unwrap();
    let ready = home.join(".race-ready");
    for _ in 0..600 {
        if ready.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(ready.exists(), "TS writer did not start");
    let roots = Roots::new(&home, &home);
    let commands = commands::CommandsService::new(roots.clone());
    let mcp = mcp_sync::McpSyncService::new(roots, None);
    for index in 0..COUNT {
        if index % 2 == 0 {
            commands
                .set_command_enabled(
                    json!({"commandId":"race","filePath":format!("rs-{index}"),"enabled":false}),
                )
                .await
                .unwrap();
        } else {
            mcp.save(
                json!({"action":"upsert","name":format!("rs-{index}"),"config":{"command":"x"}}),
            )
            .await
            .unwrap();
        }
    }
    let status = child.wait().await.unwrap();
    assert!(status.success(), "TS writer failed: {status}");
    let config: Value =
        serde_json::from_str(&std::fs::read_to_string(config_dir.join("config.json")).unwrap())
            .unwrap();
    assert_eq!(config["model"], "keep");
    let mut missing = Vec::new();
    for prefix in ["ts", "rs"] {
        for index in 0..COUNT {
            let key = format!("{prefix}-{index}");
            let present = if index % 2 == 0 {
                config["command"][&key]["enable"] == false
            } else {
                config["mcp"]["servers"][&key].is_object()
            };
            if !present {
                missing.push(key);
            }
        }
    }
    assert!(missing.is_empty(), "lost updates: {missing:?}");
}

#[tokio::test]
async fn archives_are_interchangeable_with_typescript() {
    if !node_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    // Rust 导出 skill 归档 → TS 解包。
    let home = base.join("rust-home");
    let skill = home.join(".zcode/skills/group/demo");
    std::fs::create_dir_all(skill.join("assets")).unwrap();
    std::fs::write(skill.join("SKILL.md"), "---\nname: demo\n---\nbody\n").unwrap();
    std::fs::write(skill.join("assets/data.txt"), "data").unwrap();
    let rust_sync = skill_sync::SkillSyncService::new(Roots::new(&home, &home));
    let candidates = rust_sync.list_local_candidates().await.unwrap();
    let exported = rust_sync
        .export_archive(json!({"skillIds":[candidates["candidates"][0]["id"]]}))
        .await
        .unwrap();
    let archive = base.join("rust-skills.tar.gz");
    let bytes = zcode_host_services::extensions::bytes_param(Some(&exported["archive"])).unwrap();
    std::fs::write(&archive, bytes).unwrap();
    let out = base.join("ts-extract");
    let status = interop(
        &[
            "extract-skills",
            archive.to_str().unwrap(),
            out.to_str().unwrap(),
        ],
        &base,
    )
    .status()
    .await
    .unwrap();
    assert!(status.success());
    assert_eq!(
        std::fs::read_to_string(out.join("group/demo/assets/data.txt")).unwrap(),
        "data"
    );

    // Rust 导出 plugin 归档 → TS 解包（含元数据与可执行位）。
    let plugin = home.join("plugins/tool");
    std::fs::create_dir_all(plugin.join(".zcode-plugin")).unwrap();
    std::fs::create_dir_all(plugin.join("bin")).unwrap();
    std::fs::write(
        plugin.join(".zcode-plugin/plugin.json"),
        "{\"name\":\"tool\"}",
    )
    .unwrap();
    std::fs::write(plugin.join("bin/run.sh"), "#!/bin/sh\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            plugin.join("bin/run.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    std::fs::create_dir_all(home.join(".zcode/cli")).unwrap();
    std::fs::write(
        home.join(".zcode/cli/config.json"),
        format!("{{\"plugins\":{{\"dirs\":[\"{}\"]}}}}", plugin.display()),
    )
    .unwrap();
    let plugin_sync = plugin_sync::PluginSyncService::new(Roots::new(&home, &home));
    let plugins = plugin_sync.list_local_candidates().await.unwrap();
    let exported = plugin_sync
        .export_plugins(json!({"pluginIds":[plugins["candidates"][0]["id"]]}))
        .await
        .unwrap();
    let archive = base.join("rust-plugins.tar.gz");
    std::fs::write(
        &archive,
        zcode_host_services::extensions::bytes_param(Some(&exported["archive"])).unwrap(),
    )
    .unwrap();
    let out = base.join("ts-plugins");
    let status = interop(
        &[
            "extract-plugins",
            archive.to_str().unwrap(),
            out.to_str().unwrap(),
        ],
        &base,
    )
    .status()
    .await
    .unwrap();
    assert!(status.success());
    let metadata: Value = serde_json::from_str(
        &std::fs::read_to_string(out.join(".zcode-plugin-sync.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(metadata["plugins"][0]["pluginId"], "tool@inline");
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(out.join("tool/bin/run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    // TS 导出 skill 归档 → Rust 导入。
    let ts_home = base.join("ts-home");
    let ts_skill = ts_home.join(".zcode/skills/from-ts");
    std::fs::create_dir_all(&ts_skill).unwrap();
    std::fs::write(
        ts_skill.join("SKILL.md"),
        "---\nname: from-ts\n---\nTS body\n",
    )
    .unwrap();
    let ts_archive = base.join("ts-skills.tar.gz");
    let status = interop(
        &[
            "export-skills",
            ts_home.to_str().unwrap(),
            ts_archive.to_str().unwrap(),
        ],
        &ts_home,
    )
    .status()
    .await
    .unwrap();
    assert!(status.success());
    let remote_home = base.join("remote-home");
    let remote = skill_sync::SkillSyncService::new(Roots::new(&remote_home, &remote_home));
    let imported = remote
        .import_archive(json!({
            "archive": zcode_host_services::extensions::bytes_value(&std::fs::read(&ts_archive).unwrap()),
        }))
        .await
        .unwrap();
    assert_eq!(imported["results"][0]["status"], "synced", "{imported}");
    assert_eq!(
        std::fs::read_to_string(remote_home.join(".zcode/skills/from-ts/SKILL.md")).unwrap(),
        "---\nname: from-ts\n---\nTS body\n"
    );
}
