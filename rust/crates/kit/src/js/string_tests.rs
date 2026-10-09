use super::*;

#[test]
fn sanitizes_like_node() {
    assert_eq!(sanitize_message("  a \n\t b\u{feff}"), Some("a b".into()));
    assert_eq!(sanitize_message(" \n "), None);
    let long = "字".repeat(600);
    let cut = sanitize_message(&long).unwrap();
    assert_eq!(cut.chars().count(), 500);
    assert!(cut.ends_with("..."));
    assert_eq!(utf16_prefix("a😀b", 2), "a");
    assert_eq!(trim("\u{85}x\u{feff}"), "\u{85}x");
}
