//! 与真实 zod 4 的对拍（fixture 由 scripts/rust-host/zod-flatten-fixtures.ts 生成）。
use serde_json::{Value, json};

const FIXTURE: &str = include_str!("fixtures/zod-flatten.json");

fn validate(schema: &str, input: &Value) -> Result<serde_json::Map<String, Value>, Value> {
    match schema {
        "offPeakCreate" => crate::off_peak::reverse::validate_create(input),
        "offPeakList" => crate::off_peak::reverse::validate_list(input),
        "automationCreate" => crate::automation::reverse::validate_create(input),
        "automationUpdate" => crate::automation::reverse::validate_update(input),
        "automationList" => crate::automation::reverse::validate_list(input),
        "automationCheckTaskBinding" => {
            crate::automation::reverse::validate_id(input, "targetTaskId")
        }
        "automationDelete" => crate::automation::reverse::validate_id(input, "automationId"),
        other => panic!("unknown schema {other}"),
    }
}

#[test]
fn flatten_matches_zod() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert!(cases.len() > 30);
    for case in cases {
        // 缺省参数在 ReverseContext 里是 null；fixture 用 `undefined: true` 标记。
        let input = if case["undefined"] == true {
            Value::Null
        } else {
            case["input"].clone()
        };
        let actual = match validate(case["schema"].as_str().unwrap(), &input) {
            Ok(parsed) => json!({"ok": parsed}),
            Err(flatten) => json!({"flatten": flatten}),
        };
        assert_eq!(
            actual.to_string(),
            case["expected"].to_string(),
            "{} {}",
            case["schema"],
            case["input"]
        );
    }
}
