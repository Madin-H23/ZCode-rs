//! bots 内部的小工具：JS 字符串语义（UTF-16 长度、`encodeURIComponent`）、哈希与随机 id。
use serde_json::Value;
use sha2::{Digest, Sha256};

pub(crate) fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// 非空字符串（`typeof x === "string" && x.trim()`，返回原值）。
pub(crate) fn non_empty<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    text(value, key).filter(|value| !value.trim().is_empty())
}

pub(crate) fn sha256_hex(input: &[u8]) -> String {
    Sha256::digest(input)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `crypto.randomUUID()`。
pub(crate) fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    let _ = getrandom::fill(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

pub(crate) fn random_u32() -> u32 {
    let mut bytes = [0u8; 4];
    let _ = getrandom::fill(&mut bytes);
    u32::from_le_bytes(bytes)
}

/// JS `string.length`（UTF-16 码元数）。
pub(crate) fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// JS `string.slice(start, end)`（按 UTF-16 码元；切开代理对时与 JS 一样得到孤立代理，这里替换为 U+FFFD）。
pub(crate) fn js_slice(text: &str, start: usize, end: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    let end = end.min(units.len());
    let start = start.min(end);
    String::from_utf16_lossy(&units[start..end])
}

/// 按 `limit` 个 UTF-16 码元切分；空串得到 `[""]`（`splitTelegramText` / `splitFeishuText`）。
pub(crate) fn split_utf16(text: &str, limit: usize) -> Vec<String> {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.is_empty() {
        return vec![text.to_owned()];
    }
    units.chunks(limit).map(String::from_utf16_lossy).collect()
}

/// `encodeURIComponent`。
pub(crate) fn encode_uri_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        let unreserved = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            );
        if unreserved {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `decodeURIComponent`；非法序列返回 None（调用方决定是否保留原值）。
pub(crate) fn decode_uri_component(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            out.push(bytes[index]);
            index += 1;
            continue;
        }
        let byte = text
            .get(index + 1..index + 3)
            .and_then(|hex| u8::from_str_radix(hex, 16).ok())?;
        out.push(byte);
        index += 3;
    }
    String::from_utf8(out).ok()
}

/// `value.trim().replace(/\s+/g, " ")`。
pub(crate) fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `value.length > max ? value.slice(0, max - 3) + "..." : value`（UTF-16 计长）。
pub(crate) fn truncate_ellipsis(value: &str, max: usize) -> String {
    if js_len(value) > max {
        format!("{}...", js_slice(value, 0, max - 3))
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_string_helpers_match_javascript() {
        assert_eq!(encode_uri_component("a b/c:é"), "a%20b%2Fc%3A%C3%A9");
        assert_eq!(encode_uri_component("A-z_0.!~*'()"), "A-z_0.!~*'()");
        assert_eq!(
            decode_uri_component("a%20b%2Fc%3A%C3%A9").as_deref(),
            Some("a b/c:é")
        );
        assert_eq!(decode_uri_component("%E0%A4%A"), None);
        assert_eq!(js_len("字😀"), 3);
        assert_eq!(js_slice("abcdef", 1, 3), "bc");
        assert_eq!(split_utf16("", 3), vec![String::new()]);
        assert_eq!(split_utf16("abcdefg", 3), vec!["abc", "def", "g"]);
        assert_eq!(collapse_whitespace("  a \n b\t c "), "a b c");
        assert_eq!(truncate_ellipsis("abcdefghij", 6), "abc...");
        assert_eq!(truncate_ellipsis("abc", 6), "abc");
        let id = uuid_v4();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
    }
}
