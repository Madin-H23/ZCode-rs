use std::borrow::Cow;

use super::*;

#[test]
fn redacts_bearer_tokens() {
    let cases = [
        (
            "Bearer abcde+fghijklmnopqrstuvwxyz012345",
            "Bearer [REDACTED]",
        ),
        (
            "Bearer sk-abcdefghijklmnopqrst+secret_suffix",
            "Bearer [REDACTED]",
        ),
        (
            "Bearer AbcdefghijklMN09._~+/-==; echo done",
            "Bearer [REDACTED]; echo done",
        ),
        (
            "authorization: bEaReR\tabcdefghijklmnop",
            "authorization: Bearer [REDACTED]",
        ),
        ("Bearer   abcdefghijklmnop", "Bearer [REDACTED]"),
    ];
    for (input, expected) in cases {
        assert_eq!(redact_secrets(input), expected, "input {input:?}");
    }
}

#[test]
fn avoids_bearer_false_positives() {
    for input in [
        "Bearer of good news",
        "Bearer abcdefghijklmno",
        "NotABearer abcdefghijklmnop",
        "Bearerabcdefghijklmnop",
        "Bearer\nabcdefghijklmnop",
        "Bearer\u{a0}abcdefghijklmnop",
        "Bearer abcdefghijklmno\u{212a}",
    ] {
        assert_eq!(redact_secrets(input), Cow::Borrowed(input));
    }
}

#[test]
fn redacts_provider_keys_and_tokens() {
    let cases = [
        (
            "key sk-abcdefghijklmnopqrstuvwxyz done",
            "key [REDACTED] done",
        ),
        (
            "anthropic sk-ant-api03-abcdefghijklmnop_qrstu-vw",
            "anthropic [REDACTED]",
        ),
        ("openai sk-proj-abcdefghijklmnopqrstuv", "openai [REDACTED]"),
        ("aws AKIAABCDEFGHIJKLMNOP id", "aws [REDACTED] id"),
        (
            "gh ghp_abcdefghijklmnopqrstuvwxyz0123456789 ok",
            "gh [REDACTED] ok",
        ),
        (
            "pat github_pat_11ABCDEFG0123456789_abcdefghijklmnop",
            "pat [REDACTED]",
        ),
        (
            "api_key=abcdefgh123 password: 'hunter2hunter2' TOKEN = \"tok_12345678\"",
            "api_key=[REDACTED] password: '[REDACTED]' TOKEN = \"[REDACTED]\"",
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(redact_secrets(input), expected, "input {input:?}");
    }
}

#[test]
fn leaves_ordinary_text_borrowed() {
    for input in [
        "",
        "task-abcdefghijklmnopqrstuvwxyz is a word, not a key",
        "AKIA123 is too short",
        "token=short",
        "the secret ingredient",
    ] {
        assert!(
            matches!(redact_secrets(input), Cow::Borrowed(text) if text == input),
            "input {input:?}"
        );
    }
}
