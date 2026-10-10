//! 与真实 TS settings-sync 服务的对拍（fixture 由 scripts/rust-host/settings-sync-fixtures.ts 生成）：
//! 同一棵来源树上逐步执行 detect / importSelected，结果与最终落盘状态逐项相同。
use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Value, json};

use super::SettingsSync;
use crate::settings::SettingStore;

const FIXTURE: &str = include_str!("fixtures/parity.json");

fn dump(base: &Path, root: &Path, out: &mut Vec<Value>) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    // 与 TS 的 `readdir().sort()`（UTF-16 码元序）一致：这里的名字都是 ASCII。
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        let path = base.join(&name);
        let relative = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let meta = std::fs::symlink_metadata(&path).unwrap();
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&path).unwrap();
            out.push(
                json!({"path": relative, "type": "symlink", "target": target.to_string_lossy()}),
            );
        } else if meta.is_dir() {
            out.push(json!({"path": relative, "type": "dir"}));
            dump(&path, root, out);
        } else {
            let content = std::fs::read_to_string(&path).unwrap();
            out.push(json!({"path": relative, "type": "file", "content": content}));
        }
    }
}

#[tokio::test]
async fn manual_import_matches_typescript() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let root_text = root.to_string_lossy().into_owned();
    for (path, content) in fixture["sourceTree"].as_object().unwrap() {
        let target = root.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(
            &target,
            content.as_str().unwrap().replace("<ROOT>", &root_text),
        )
        .unwrap();
    }
    let home = root.join("home");
    let vars = BTreeMap::from([("HOME".to_owned(), home.to_string_lossy().into_owned())]);
    let sync = SettingsSync::new(
        SettingStore::new(&zcode_host_base::testing::spawner(), &home),
        &vars,
        &home,
    );
    let mut results = Vec::new();
    for op in fixture["ops"].as_array().unwrap() {
        let request: Value =
            serde_json::from_str(&op["request"].to_string().replace("<ROOT>", &root_text)).unwrap();
        let result = match op["op"].as_str().unwrap() {
            "detect" => sync.detect(&request).await,
            _ => sync.import_selected(&request).await,
        };
        results.push(result);
    }
    let mut tree = Vec::new();
    dump(&root.join("home/.zcode"), root, &mut tree);
    dump(&root.join("ws/.zcode"), root, &mut tree);
    let actual: Value = serde_json::from_str(
        &json!({"results": results, "tree": tree})
            .to_string()
            .replace(&root_text, "<ROOT>"),
    )
    .unwrap();
    for (index, expected) in fixture["results"].as_array().unwrap().iter().enumerate() {
        assert_eq!(
            actual["results"][index].to_string(),
            expected.to_string(),
            "op #{index}: {}",
            fixture["ops"][index]
        );
    }
    assert_eq!(actual["tree"], fixture["tree"]);
}
