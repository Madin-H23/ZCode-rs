use super::*;
use serde_json::json;

#[test]
fn numbers_follow_ecmascript() {
    let cases = [
        (1.0, "1"),
        (-0.0, "0"),
        (0.1, "0.1"),
        (1.5, "1.5"),
        (123_456_789.0, "123456789"),
        (1e21, "1e+21"),
        (1.5e21, "1.5e+21"),
        (1e20, "100000000000000000000"),
        (0.000001, "0.000001"),
        (1e-7, "1e-7"),
        (2.5e-8, "2.5e-8"),
        (-3.25, "-3.25"),
        (f64::NAN, "null"),
    ];
    for (value, expected) in cases {
        assert_eq!(js_double(value), expected, "{value}");
    }
    let parsed: Value = serde_json::from_str(r#"[1.0,2e2,-0.0,9007199254740993]"#).unwrap();
    assert_eq!(stringify(&parsed), "[1,200,0,9007199254740992]");
}

#[test]
fn objects_follow_js_property_order() {
    let parsed: Value =
        serde_json::from_str(r#"{"b":1,"10":2,"a":{"2":0,"x":1,"1":2},"01":3,"0":4}"#).unwrap();
    assert_eq!(
        stringify(&parsed),
        r#"{"0":4,"10":2,"b":1,"a":{"1":2,"2":0,"x":1},"01":3}"#
    );
    assert_eq!(
        stringify(&json!({"s":"a\"\\\n\u{1}\u{7f}/\u{2028}é"})),
        "{\"s\":\"a\\\"\\\\\\n\\u0001\u{7f}/\u{2028}é\"}"
    );
    assert_eq!(encode(&Value::Null), None);
    assert_eq!(encode(&json!({})).as_deref(), Some("{}"));
}
