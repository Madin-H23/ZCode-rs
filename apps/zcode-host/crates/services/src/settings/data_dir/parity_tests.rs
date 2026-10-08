//! 与真实 TS 实现的对拍（fixture 由 scripts/rust-host/setting-data-dir-fixtures.ts 生成）：
//! `path.win32` 的 normalize / resolve / join、`validateDataBaseDirTarget`、`copyDataDirectory`（fs.cp）
//! 与 `updateDataBaseDir` 的返回值、错误和落盘树逐项相同。复制场景用到软链、FIFO 与 mode，只在 unix 上跑。
use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::{GuardInput, validate, win32_path};

const FIXTURE: &str = include_str!("../fixtures/data-dir-parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).unwrap()
}

fn string_map(value: &Value) -> BTreeMap<String, String> {
    value
        .as_object()
        .map(|object| {
            object
                .iter()
                .map(|(key, value)| (key.clone(), value.as_str().unwrap().to_owned()))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn win32_paths_match_node() {
    let fixture = fixture();
    let win32 = &fixture["win32"];
    let cwd = win32["cwd"].as_str().unwrap();
    let drive_cwds = string_map(&win32["driveCwds"]);
    for case in win32["paths"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        assert_eq!(
            win32_path::normalize(input),
            case["normalize"].as_str().unwrap(),
            "normalize({input:?})"
        );
        assert_eq!(
            win32_path::resolve(&[input], cwd, |key| drive_cwds.get(key).cloned()),
            case["resolve"].as_str().unwrap(),
            "resolve({input:?})"
        );
    }
    for case in win32["joins"].as_array().unwrap() {
        let parts: Vec<&str> = case["parts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|part| part.as_str().unwrap())
            .collect();
        assert_eq!(
            win32_path::join(&parts),
            case["join"].as_str().unwrap(),
            "join({parts:?})"
        );
    }
}

#[test]
fn install_dir_guard_matches_node() {
    let fixture = fixture();
    let default_cwd = fixture["win32"]["cwd"].as_str().unwrap().to_owned();
    for case in fixture["validation"].as_array().unwrap() {
        let env = string_map(&case["env"]);
        let input = GuardInput {
            windows: case["platform"]
                .as_str()
                .is_none_or(|platform| platform == "win32"),
            env: &env,
            app_install_dir: case["appInstallDir"].as_str(),
            cwd: case["cwd"].as_str().unwrap_or(&default_cwd),
        };
        let actual = match validate(case["target"].as_str().unwrap(), &input) {
            Ok(()) => json!({"ok": true}),
            Err(forbidden) => json!({
                "ok": false,
                "code": super::FORBIDDEN_CODE,
                "forbiddenDir": forbidden.forbidden_dir,
            }),
        };
        assert_eq!(actual, case["result"], "{}", case["name"]);
    }
}

#[cfg(unix)]
mod copy_cases {
    use std::collections::BTreeMap;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;

    use serde_json::{Value, json};
    use zcode_host_kit::RpcError;

    use super::fixture;
    use crate::settings::SettingStore;
    use crate::settings::data_dir::{DataDirMove, copy_data_directory};

    /// 与 TS `buildTree` 相同：建树 → 目录 0755、普通文件 0644 → 按条目设定 mode（深的先设）。
    fn build_tree(root: &Path, entries: &[Value]) {
        let root_text = root.to_string_lossy();
        for entry in entries {
            let target = root.join(entry["path"].as_str().unwrap());
            if entry["type"] == "dir" {
                std::fs::create_dir_all(&target).unwrap();
                continue;
            }
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            match entry["type"].as_str().unwrap() {
                "file" => std::fs::write(
                    &target,
                    entry["content"]
                        .as_str()
                        .unwrap()
                        .replace("<ROOT>", &root_text),
                )
                .unwrap(),
                "symlink" => symlink(
                    entry["target"]
                        .as_str()
                        .unwrap()
                        .replace("<ROOT>", &root_text),
                    &target,
                )
                .unwrap(),
                _ => {
                    let status = std::process::Command::new("mkfifo")
                        .arg(&target)
                        .status()
                        .unwrap();
                    assert!(status.success());
                }
            }
        }
        let mut dirs = Vec::new();
        collect_dirs(root, &mut dirs);
        for dir in &dirs {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            for child in std::fs::read_dir(dir).unwrap() {
                let child = child.unwrap().path();
                if std::fs::symlink_metadata(&child).unwrap().is_file() {
                    std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o644))
                        .unwrap();
                }
            }
        }
        let mut with_mode: Vec<&Value> = entries
            .iter()
            .filter(|entry| entry["mode"].is_u64())
            .collect();
        with_mode.sort_by_key(|entry| {
            std::cmp::Reverse(entry["path"].as_str().unwrap().split('/').count())
        });
        for entry in with_mode {
            let mode = entry["mode"].as_u64().unwrap() as u32;
            std::fs::set_permissions(
                root.join(entry["path"].as_str().unwrap()),
                std::fs::Permissions::from_mode(mode),
            )
            .unwrap();
        }
    }

    fn collect_dirs(path: &Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return;
        };
        if !meta.is_dir() {
            return;
        }
        out.push(path.to_owned());
        for child in std::fs::read_dir(path).unwrap() {
            collect_dirs(&child.unwrap().path(), out);
        }
    }

    /// 与 TS `modeTracked` 相同：只比较 `.zcode/v2` 子树内的 mode，`setting.json` 除外。
    fn mode_tracked(rel: &str) -> bool {
        let in_v2 = rel == ".zcode/v2"
            || rel.starts_with(".zcode/v2/")
            || rel.ends_with("/.zcode/v2")
            || rel.contains("/.zcode/v2/");
        in_v2 && !rel.ends_with("/setting.json")
    }

    fn dump(root: &Path, path: &Path, out: &mut Vec<Value>) {
        let rel = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            out.push(json!({"path": rel, "type": "missing"}));
            return;
        };
        let mut entry = serde_json::Map::new();
        entry.insert("path".into(), json!(rel));
        let file_type = meta.file_type();
        let tracked = mode_tracked(&rel);
        if file_type.is_symlink() {
            entry.insert("type".into(), json!("symlink"));
            let target = std::fs::read_link(path).unwrap();
            entry.insert("target".into(), json!(target.to_string_lossy()));
            out.push(Value::Object(entry));
        } else if file_type.is_dir() {
            entry.insert("type".into(), json!("dir"));
            if tracked {
                entry.insert("mode".into(), json!(meta.permissions().mode() & 0o777));
            }
            out.push(Value::Object(entry));
            let mut names: Vec<String> = std::fs::read_dir(path)
                .unwrap()
                .map(|child| child.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            for name in names {
                dump(root, &path.join(name), out);
            }
        } else if !file_type.is_file() {
            entry.insert("type".into(), json!("fifo"));
            out.push(Value::Object(entry));
        } else {
            entry.insert("type".into(), json!("file"));
            if tracked {
                entry.insert("mode".into(), json!(meta.permissions().mode() & 0o777));
            }
            entry.insert(
                "content".into(),
                json!(std::fs::read_to_string(path).unwrap()),
            );
            out.push(Value::Object(entry));
        }
    }

    /// 只读目录先放开权限，tempdir 才删得掉。
    fn unlock(root: &Path) {
        let mut dirs = Vec::new();
        collect_dirs(root, &mut dirs);
        for dir in dirs {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
        }
    }

    fn outcome(result: Result<(), RpcError>, expected: &Value) -> Value {
        match result {
            Ok(()) => json!({"ok": true}),
            // ZodError 的文案是 zod issues 的 JSON，Rust 只保证错误类型一致（见规格偏差表）。
            Err(error) if error.name == "ZodError" => json!({
                "ok": false,
                "error": {"name": "ZodError", "message": expected["error"]["message"]},
            }),
            Err(error) => {
                let mut record = serde_json::Map::new();
                record.insert("name".into(), json!(error.name));
                if let Some(code) = error.code_str() {
                    record.insert("code".into(), json!(code));
                }
                record.insert("message".into(), json!(error.message));
                json!({"ok": false, "error": record})
            }
        }
    }

    #[tokio::test]
    async fn copy_and_update_match_node() {
        let fixture = fixture();
        for case in fixture["cases"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let root_text = root.to_string_lossy().into_owned();
            build_tree(root, case["tree"].as_array().unwrap());
            let sub = |value: &str| value.replace("<ROOT>", &root_text);
            let joined = |rel: &str| crate::support::path::resolve(&root_text, &[rel]);

            let result = if case["kind"] == "copy" {
                copy_data_directory(
                    &joined(case["from"].as_str().unwrap()),
                    &joined(case["to"].as_str().unwrap()),
                )
                .await
                .map_err(RpcError::from)
            } else {
                let home = root.join("home");
                let mover = DataDirMove::with_paths(
                    SettingStore::new(&home),
                    &joined(case["current"].as_str().unwrap()),
                    &home.to_string_lossy(),
                    BTreeMap::new(),
                    false,
                );
                mover
                    .update_data_base_dir(case["newDir"].as_str().map(sub))
                    .await
            };

            let mut after = Vec::new();
            for path in case["dump"].as_array().unwrap() {
                dump(root, &root.join(path.as_str().unwrap()), &mut after);
            }
            let actual: Value = serde_json::from_str(
                &json!({"outcome": outcome(result, &case["outcome"]), "after": after})
                    .to_string()
                    .replace(&root_text, "<ROOT>"),
            )
            .unwrap();
            unlock(root);
            assert_eq!(actual["outcome"], case["outcome"], "{name}: outcome");
            assert_eq!(actual["after"], case["after"], "{name}: tree");
        }
    }
}
