//! 设置语义与 TS 实现的差分测试（夹具由 fixtures/settings-gen.ts 用真实 zod schema 生成）。
use serde_json::Value;
use zcode_host_services::settings::schema;

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/settings.json")).unwrap()
}

#[test]
fn reads_match_typescript() {
    let fixture = fixture();
    let mut failures = Vec::new();
    for case in fixture["reads"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let migrated = schema::migrate_legacy_account_connections(&case["raw"]);
        let actual = schema::parse_settings(&migrated).unwrap_or(Value::Null);
        if actual != case["expected"] {
            failures.push(format!(
                "{name}:\n  expected {}\n  actual   {}",
                case["expected"], actual
            ));
        }
        // 键顺序也要一致：写回文件时与 TS 产出的 JSON 相同。
        if let (Some(actual), Some(expected)) = (actual.as_object(), case["expected"].as_object()) {
            let actual_keys: Vec<&String> = actual.keys().collect();
            let expected_keys: Vec<&String> = expected.keys().collect();
            if actual_keys != expected_keys {
                failures.push(format!(
                    "{name}: key order {actual_keys:?} != {expected_keys:?}"
                ));
            }
        }
        if schema::has_incomplete_legacy_team_connections(&case["raw"])
            != case["incompleteLegacyTeam"].as_bool().unwrap()
        {
            failures.push(format!("{name}: incomplete legacy team mismatch"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn patches_match_typescript() {
    let fixture = fixture();
    let current = &fixture["current"];
    let mut failures = Vec::new();
    for case in fixture["patches"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let actual = match schema::parse_patch(&case["patch"]) {
            Err(_) => serde_json::json!({"error": true}),
            Ok(patch) => match schema::merge(current, &patch) {
                Some(merged) => serde_json::json!({"merged": merged}),
                None => serde_json::json!({"error": true}),
            },
        };
        if actual != case["expected"] {
            failures.push(format!(
                "{name}:\n  expected {}\n  actual   {}",
                case["expected"], actual
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
