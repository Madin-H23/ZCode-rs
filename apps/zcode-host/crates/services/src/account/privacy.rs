//! 反馈材料的隐私脱敏（packages/shared/src/feedbackPrivacy.ts）。
//!
//! 只用于用户反馈的排障材料（工单文本、诊断日志）；不得应用到模型请求或认证载荷。
//! 正则逐条对应 TS；JS `\w` 按 ASCII 处理。
use std::sync::OnceLock;

use regex::{Captures, Regex};
use serde_json::{Map, Value};

const REDACTED: &str = "[REDACTED]";

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static redaction regex"))
}

fn sensitive_key() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(
        &CELL,
        r"(?i)(?:password|passwd|passphrase|secret|token|apikey|accesskey|privatekey|authorization|cookie|credential)",
    )
}

fn diagnostic_body_key() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(
        &CELL,
        r"(?i)^(?:content|messages?|prompt|systemprompt|request|response|body|payload|input|output|toolinput|tooloutput|arguments|args|env|environment|headers|text|completion|result|stdout|stderr|data|params)$",
    )
}

fn normalize_key(key: &str) -> String {
    key.chars().filter(|c| c.is_ascii_alphanumeric()).collect()
}

fn should_redact_key(key: &str, diagnostic: bool) -> bool {
    let normalized = normalize_key(key);
    sensitive_key().is_match(&normalized) || (diagnostic && diagnostic_body_key().is_match(&normalized))
}

struct FieldKey {
    key: String,
    quoted: bool,
    end: usize,
}

/// `fieldKeys`：`"key": `、`'key'=`、`key: ` 形式的字段名及其后位置。
fn field_keys(text: &str) -> Vec<FieldKey> {
    static CELL: OnceLock<Regex> = OnceLock::new();
    let pattern = regex(
        &CELL,
        r#"(?:"((?:\\.|[^"\\])*)"|'([^']*)'|([A-Za-z0-9_.-]+))\s*[:=]\s*"#,
    );
    pattern
        .captures_iter(text)
        .map(|captures| {
            let whole = captures.get(0).expect("match");
            let (key, quoted) = match (captures.get(1), captures.get(2), captures.get(3)) {
                (Some(double), _, _) => {
                    let raw = double.as_str();
                    let decoded = serde_json::from_str::<String>(&format!("\"{raw}\"")).unwrap_or_else(|_| raw.to_owned());
                    (decoded, true)
                }
                (_, Some(single), _) => (single.as_str().to_owned(), true),
                (_, _, Some(bare)) => (bare.as_str().to_owned(), false),
                _ => (String::new(), false),
            };
            FieldKey {
                key,
                quoted,
                end: whole.end(),
            }
        })
        .collect()
}

/// `decodeURIComponent`：非法转义返回 None（JS 抛错）。
fn decode_uri_component(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = text.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn should_redact_url_path(url: &url::Url, diagnostic: bool) -> bool {
    if diagnostic || !url.username().is_empty() || url.password().is_some() || url.host_str() == Some("hooks.slack.com") {
        return true;
    }
    static SIG: OnceLock<Regex> = OnceLock::new();
    let signature = regex(&SIG, r"(?i)^(?:.*signature|sig|code)$");
    if url
        .query_pairs()
        .any(|(key, _)| should_redact_key(&key, false) || signature.is_match(&normalize_key(&key)))
    {
        return true;
    }
    static SEGMENT: OnceLock<Regex> = OnceLock::new();
    let segment_pattern = regex(
        &SEGMENT,
        r"^(?:webhook\w*|(?:password)?reset(?:password)?|invites?|invitations?|callback|downloads?|signed|verify|verification|activate|magiclink)$",
    );
    match decode_uri_component(url.path()) {
        // 无法解码的路径不能证明安全，避免编码形式绕过识别。
        None => true,
        Some(path) => path.split('/').any(|segment| {
            let normalized = normalize_key(segment);
            sensitive_key().is_match(&normalized) || segment_pattern.is_match(&normalized.to_lowercase())
        }),
    }
}

fn redact_values(text: &str, diagnostic: bool) -> String {
    static HEADER: OnceLock<Regex> = OnceLock::new();
    static PRIVATE_KEY: OnceLock<Regex> = OnceLock::new();
    static BEARER: OnceLock<Regex> = OnceLock::new();
    static ASSIGNMENT: OnceLock<Regex> = OnceLock::new();
    static URL: OnceLock<Regex> = OnceLock::new();
    static USER_PATH: OnceLock<Regex> = OnceLock::new();
    static EMAIL: OnceLock<Regex> = OnceLock::new();
    let result = regex(&HEADER, r"(?i)\b((?:Proxy-)?Authorization|Cookie|Set-Cookie)\s*:\s*[^\r\n]+")
        .replace_all(text, format!("$1: {REDACTED}").as_str())
        .into_owned();
    let result = regex(
        &PRIVATE_KEY,
        r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
    )
    .replace_all(&result, REDACTED)
    .into_owned();
    let result = regex(&BEARER, r#"(?i)\bBearer\s+[^\s"'\\,;]+"#)
        .replace_all(&result, format!("Bearer {REDACTED}").as_str())
        .into_owned();
    let result = regex(
        &ASSIGNMENT,
        r#"([A-Za-z0-9_.-]+)(\s*[=:]\s*)(?:"(?:\\.|[^"\\])*"|'[^']*'|[^\s,;"'<>]+)"#,
    )
    .replace_all(&result, |captures: &Captures| {
        if should_redact_key(&captures[1], false) {
            format!("{}{}{REDACTED}", &captures[1], &captures[2])
        } else {
            captures[0].to_owned()
        }
    })
    .into_owned();
    let result = regex(&URL, r#"(?i)\b[a-z][a-z0-9+.-]*://[^\s"'<>\\]+"#)
        .replace_all(&result, |captures: &Captures| {
            let raw = &captures[0];
            let Ok(mut url) = url::Url::parse(raw) else {
                return REDACTED.to_owned();
            };
            // URL 凭据不只在 userinfo / query 里：webhook 与重置链接常把秘密放在路径。
            if should_redact_url_path(&url, diagnostic) && !url.path().is_empty() && url.path() != "/" {
                url.set_path(&format!("/{REDACTED}"));
            }
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        })
        .into_owned();
    let mut result = regex(&USER_PATH, r#"(?i)(?:/(?:Users|home)/|[a-z]:\\Users\\)[^\s"'<>]+"#)
        .replace_all(&result, "[USER_PATH]")
        .into_owned();
    if diagnostic {
        result = regex(&EMAIL, r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b")
            .replace_all(&result, "[EMAIL]")
            .into_owned();
    }
    result
}

fn scrub_value(value: &Value, diagnostic: bool, depth: usize) -> Value {
    if depth > 32 {
        return Value::String(REDACTED.to_owned());
    }
    match value {
        Value::String(text) => Value::String(redact_text(text, diagnostic, depth + 1)),
        Value::Array(items) => Value::Array(items.iter().map(|item| scrub_value(item, diagnostic, depth + 1)).collect()),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, item)| {
                    let scrubbed = if should_redact_key(key, diagnostic) {
                        Value::String(REDACTED.to_owned())
                    } else {
                        scrub_value(item, diagnostic, depth + 1)
                    };
                    (key.clone(), scrubbed)
                })
                .collect::<Map<String, Value>>(),
        ),
        other => other.clone(),
    }
}

fn redact_plain_text(text: &str, diagnostic: bool) -> String {
    let redacted = redact_values(text, diagnostic);
    let lines: Vec<String> = redacted
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .map(|line| {
            let unsafe_line = field_keys(line).iter().any(|field| {
                should_redact_key(&field.key, diagnostic)
                    && (field.quoted || diagnostic || !line[field.end..].starts_with(REDACTED))
            });
            if unsafe_line {
                REDACTED.to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect();
    lines.join("\n")
}

fn json_fragment_end(bytes: &[u8], start: usize) -> usize {
    let mut nesting = 0i32;
    let mut quoted = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().enumerate().skip(start) {
        if quoted {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                quoted = false;
            }
        } else if *byte == b'"' {
            quoted = true;
        } else if *byte == b'{' || *byte == b'[' {
            nesting += 1;
        } else if *byte == b'}' || *byte == b']' {
            nesting -= 1;
            if nesting == 0 {
                return index + 1;
            }
        }
    }
    bytes.len()
}

fn redact_text(text: &str, diagnostic: bool, depth: usize) -> String {
    if depth > 32 {
        return REDACTED.to_owned();
    }
    if let Ok(parsed) = serde_json::from_str::<Value>(text)
        && (parsed.is_object() || parsed.is_array())
    {
        return scrub_value(&parsed, diagnostic, depth + 1).to_string();
    }
    let source = redact_values(text, diagnostic);
    let bytes = source.as_bytes();
    let mut chunks: Vec<String> = Vec::new();
    let mut cursor = 0;
    let mut index = 0;
    while index < bytes.len() {
        if source[index..].starts_with(REDACTED) {
            index += REDACTED.len();
            continue;
        }
        if bytes[index] != b'{' && bytes[index] != b'[' {
            index += 1;
            continue;
        }
        let prefix = &source[cursor..index];
        let mut end = json_fragment_end(bytes, index);
        let line_prefix = &prefix[prefix.rfind('\n').map_or(0, |i| i + 1)..];
        let sensitive_value = field_keys(line_prefix)
            .iter()
            .any(|field| should_redact_key(&field.key, diagnostic));
        if sensitive_value {
            // 字段值可能由文本和对象混合组成：不能只删对象而留下同行尾部。
            end = source[end..].find('\n').map_or(source.len(), |offset| end + offset);
        }
        let fragment = &source[index..end];
        chunks.push(redact_plain_text(prefix, diagnostic));
        if sensitive_value {
            chunks.push(REDACTED.to_owned());
        } else {
            match serde_json::from_str::<Value>(fragment) {
                Ok(parsed) => chunks.push(scrub_value(&parsed, diagnostic, depth + 1).to_string()),
                Err(_) => {
                    // 多行或截断对象不能只丢弃键名所在行，否则下一行的值仍会泄露。
                    let sensitive = field_keys(fragment).iter().any(|field| should_redact_key(&field.key, diagnostic));
                    chunks.push(if sensitive {
                        REDACTED.to_owned()
                    } else {
                        redact_plain_text(fragment, diagnostic)
                    });
                }
            }
        }
        cursor = end;
        index = end;
    }
    chunks.push(redact_plain_text(&source[cursor..], diagnostic));
    chunks.concat()
}

/// `redactFeedbackText`。
pub fn redact_feedback_text(text: &str, diagnostic: bool) -> String {
    redact_text(text, diagnostic, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_credentials_paths_and_json() {
        assert_eq!(
            redact_feedback_text("Authorization: Bearer abc.def\nok line", false),
            "Authorization: [REDACTED]\nok line"
        );
        assert_eq!(redact_feedback_text("token=abc123 rest", false), "token=[REDACTED] rest");
        assert_eq!(
            redact_feedback_text("see https://user:pw@example.com/a/b?x=1#h now", false),
            "see https://example.com/[REDACTED] now"
        );
        assert_eq!(
            redact_feedback_text("open /Users/alice/project/file.ts", false),
            "open [USER_PATH]"
        );
        assert_eq!(
            redact_feedback_text(r#"{"apiKey":"sk","nested":{"name":"x"}}"#, false),
            r#"{"apiKey":"[REDACTED]","nested":{"name":"x"}}"#
        );
        assert_eq!(
            redact_feedback_text("mail a@b.com", true),
            "mail [EMAIL]"
        );
        assert_eq!(
            redact_feedback_text(r#"log {"prompt":"secret stuff"} done"#, true),
            r#"log {"prompt":"[REDACTED]"} done"#
        );
        assert_eq!(redact_feedback_text("plain text", false), "plain text");
    }
}
