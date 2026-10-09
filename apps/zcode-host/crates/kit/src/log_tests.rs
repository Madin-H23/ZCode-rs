use super::*;

#[test]
fn messages_leaving_the_process_are_redacted() {
    assert_eq!(
        redacted(format_args!(
            "provider failed: Authorization: Bearer abcdefghijklmnopqrstu (key sk-abcdefghijklmnopqrstuvwx)"
        )),
        "provider failed: Authorization: Bearer [REDACTED] (key [REDACTED])"
    );
    assert_eq!(
        redacted("listening on 127.0.0.1:4100"),
        "listening on 127.0.0.1:4100"
    );
}
