//! 与 TS 服务的对拍（docs/specs/rust-host-extensions.md §6）：在与 fixture 相同的文件树上，经真实
//! channel 依次执行同样的调用，结果与文件副作用逐项与 TS 实现的输出相同。
//! fixture 由 `pnpm exec tsx scripts/rust-host/extensions-fixtures.ts` 生成。
use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Map, Value, json};
use zcode_host_control::ClientMode;
use zcode_host_kit::{CallContext, Reply, RpcValue, ServiceChannel};

use super::*;

const FIXTURE: &str = include_str!("fixtures/extensions-parity.json");

fn substitute(text: &str, root: &str) -> String {
    text.replace("${HOME}", &format!("{root}/home"))
        .replace("${WS}", &format!("{root}/ws"))
        .replace("${ROOT}", root)
}

fn pointer<'a>(value: &'a Value, path: &str) -> &'a Value {
    if path.is_empty() || path == "/" {
        return value;
    }
    value.pointer(path).unwrap_or(&Value::Null)
}

fn resolve_params(value: &Value, raw: &[Value], root: &str) -> Value {
    match value {
        Value::String(text) if text.starts_with("$ref:") => {
            let rest = &text["$ref:".len()..];
            let (index, path) = rest.split_once(':').unwrap();
            pointer(&raw[index.parse::<usize>().unwrap()], path).clone()
        }
        Value::String(text) if text.starts_with("$find:") => {
            let rest = &text["$find:".len()..];
            let (index, rest) = rest.split_once(':').unwrap();
            let (array_path, rest) = rest.split_once(':').unwrap();
            let (field, rest) = rest.split_once('=').unwrap();
            let split = rest.rfind(":/").unwrap();
            let (expected, path) = (&rest[..split], &rest[split + 1..]);
            let items = pointer(&raw[index.parse::<usize>().unwrap()], array_path)
                .as_array()
                .unwrap_or_else(|| panic!("fixture reference {text} has no array"));
            let item = items
                .iter()
                .find(|item| item[field] == expected)
                .unwrap_or_else(|| panic!("fixture reference not found: {text}"));
            pointer(item, path).clone()
        }
        Value::String(text) => Value::String(substitute(text, root)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| resolve_params(item, raw, root))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), resolve_params(item, raw, root)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn is_hex(text: &str) -> bool {
    text.bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// 与 extensions-fixtures.ts 的 `normalize` 逐条对应。
fn normalize(value: &Value, root: &str, key: &str) -> Value {
    if let Value::Object(map) = value
        && map.get("__zcode_rpc_nested_uint8array_v1") == Some(&Value::Bool(true))
    {
        return json!("<BYTES>");
    }
    if key == "updatedAt" || key == "archiveBytes" {
        return json!("<N>");
    }
    if key == "discoveredAt" {
        return json!("<TIME>");
    }
    match value {
        Value::String(text) => {
            let mut text = text.replace(root, "<ROOT>");
            if text.starts_with("glm:") && text.len() > 13 {
                let (head, tail) = text.split_at(text.len() - 13);
                if tail.starts_with(':') && is_hex(&tail[1..]) && head["glm:".len()..].contains(':')
                {
                    text = format!("{head}:<H>");
                }
            }
            if key == "id" && text.len() == 64 && is_hex(&text) {
                text = "<SHA>".to_owned();
            }
            Value::String(text)
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| normalize(item, root, "")).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(entry, item)| (entry.clone(), normalize(item, root, entry)))
                .collect::<Map<String, Value>>(),
        ),
        other => other.clone(),
    }
}

fn normalize_file(text: &str, root: &str) -> String {
    let text = text.replace(root, "<ROOT>");
    let mut out = String::new();
    let mut rest = text.as_str();
    while let Some(at) = rest.find("\"createdAt\":") {
        out.push_str(&rest[..at]);
        let after = &rest[at + "\"createdAt\":".len()..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 {
            out.push_str("\"createdAt\":\"<T>\"");
        } else {
            out.push_str("\"createdAt\":");
        }
        rest = &after[digits..];
    }
    out.push_str(rest);
    out
}

fn channels(roots: &Roots) -> HashMap<&'static str, Arc<dyn ServiceChannel>> {
    let mut map: HashMap<&'static str, Arc<dyn ServiceChannel>> = HashMap::new();
    map.insert(
        "skills",
        skills::service(skills::SkillsService::new(roots.clone())).build(None),
    );
    map.insert(
        "skill-sync",
        skill_sync::service(skill_sync::SkillSyncService::new(roots.clone())).build(None),
    );
    map.insert("plugins", plugins::service().build(None));
    map.insert(
        "plugin-sync",
        plugin_sync::service(plugin_sync::PluginSyncService::new(roots.clone())).build(None),
    );
    map.insert(
        "commands",
        commands::service(commands::CommandsService::new(roots.clone())).build(None),
    );
    map.insert(
        "memory",
        memory::service(memory::MemoryService::new(&roots.data_root)).build(None),
    );
    map.insert(
        "hooks",
        hooks::service(hooks::HooksService::new(roots.clone(), None)).build(None),
    );
    map.insert(
        "mcp-sync",
        mcp_sync::service(mcp_sync::McpSyncService::new(roots.clone(), None)).build(None),
    );
    map.insert(
        "subagents",
        subagents::service(subagents::SubagentsService::new(roots.clone())).build(None),
    );
    map
}

#[tokio::test]
async fn matches_typescript_services() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path())
        .unwrap()
        .display()
        .to_string();
    for file in fixture["files"].as_array().unwrap() {
        let target = std::path::Path::new(&root).join(file["path"].as_str().unwrap());
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        match file.get("symlink").and_then(Value::as_str) {
            Some(link) => std::os::unix::fs::symlink(substitute(link, &root), &target).unwrap(),
            None => std::fs::write(
                &target,
                substitute(file["content"].as_str().unwrap_or(""), &root),
            )
            .unwrap(),
        }
    }
    let home = std::path::Path::new(&root).join("home");
    let channels = channels(&Roots::new(
        &zcode_host_base::testing::spawner(),
        &home,
        &home,
    ));
    let context = CallContext {
        connection: 1,
        attachment_id: "parity".into(),
        client_mode: ClientMode::DesktopContinuous,
        window_id: None,
    };
    let expected = fixture["expected"]["results"].as_array().unwrap();
    let mut raw: Vec<Value> = Vec::new();
    let mut mismatches = Vec::new();
    for (index, call) in fixture["calls"].as_array().unwrap().iter().enumerate() {
        let channel = &channels[call["channel"].as_str().unwrap()];
        let method = call["method"].as_str().unwrap();
        let args = match call.get("params") {
            Some(params) => vec![RpcValue::Json(resolve_params(params, &raw, &root))],
            None => Vec::new(),
        };
        let actual = match channel.call(&context, method, args).await {
            Ok(Reply::Value(value)) => {
                raw.push(value.clone());
                json!({ "result": normalize(&value, &root, "") })
            }
            Ok(_) => {
                raw.push(Value::Null);
                json!({ "result": null })
            }
            Err(error) => {
                raw.push(Value::Null);
                let mut failure = Map::new();
                failure.insert("name".into(), json!(error.name));
                failure.insert(
                    "message".into(),
                    normalize(&json!(error.message), &root, ""),
                );
                if let Some(code) = error.code_str() {
                    failure.insert("code".into(), json!(code));
                }
                json!({ "error": failure })
            }
        };
        if actual != expected[index] {
            mismatches.push(format!(
                "call #{index} {}.{method}\n  rust: {actual}\n  ts:   {}",
                call["channel"], expected[index]
            ));
        }
    }
    for (path, want) in fixture["expected"]["files"].as_object().unwrap() {
        let actual = std::fs::read_to_string(std::path::Path::new(&root).join(path))
            .ok()
            .map(|text| normalize_file(&text, &root));
        if actual.as_deref() != want.as_str() {
            mismatches.push(format!("file {path}\n  rust: {actual:?}\n  ts:   {want}"));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} mismatches:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}
