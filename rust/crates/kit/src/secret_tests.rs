use serde_json::json;

use super::*;

#[test]
fn debug_and_display_hide_the_value() {
    let secret = RedactedString::new("sk-live-value");
    assert_eq!(format!("{secret:?}"), "<redacted>");
    assert_eq!(format!("{secret}"), "<redacted>");
    assert_eq!(
        format!("{:?}", Some(secret.clone())),
        "Some(<redacted>)",
        "nested Debug output stays redacted"
    );
    assert_eq!(secret.expose(), "sk-live-value");
}

#[test]
fn serde_round_trips_the_raw_value() {
    let secret = RedactedString::new("sk-live-value");
    assert_eq!(
        serde_json::to_value(&secret).unwrap(),
        json!("sk-live-value")
    );
    let parsed: RedactedString = serde_json::from_value(json!("token-1")).unwrap();
    assert_eq!(parsed, RedactedString::new("token-1"));
    assert!(serde_json::from_value::<RedactedString>(json!(1)).is_err());
}
