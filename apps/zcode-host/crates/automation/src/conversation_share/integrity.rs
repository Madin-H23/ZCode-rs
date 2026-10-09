//! 规范化 JSON 摘要（conversationShareIntegrity.ts）：键按 UTF-16 码元排序、数字按 JS 序列化，
//! 对服务端**原样发来**的 rows / artifacts 复核 projection 与 artifact set 两个 sha256。
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::support::collate::locale_compare;

/// 不参与 artifact set 摘要的签名 URL 字段（服务端读取时附加）。
const ARTIFACT_URL_KEYS: [&str; 4] = [
    "download_url",
    "download_url_expires_at",
    "url",
    "url_expires_at",
];

/// JS `JSON.stringify(number)`：整数原样，`|x| >= 1e21` 或 `< 1e-6` 用指数形式。
pub fn js_number(value: f64) -> Result<String, String> {
    if !value.is_finite() {
        return Err("Conversation share JSON numbers must be finite".into());
    }
    if value == 0.0 {
        return Ok("0".into());
    }
    let exponential = format!("{value:e}");
    let (mantissa, exponent) = exponential.split_once('e').unwrap_or((&exponential, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let sign = if negative { "-" } else { "" };
    let abs = value.abs();
    if (1e-6..1e21).contains(&abs) {
        let point = exponent + 1;
        let text = if point <= 0 {
            format!("0.{}{}", "0".repeat((-point) as usize), digits)
        } else if point as usize >= digits.len() {
            format!("{digits}{}", "0".repeat(point as usize - digits.len()))
        } else {
            format!(
                "{}.{}",
                &digits[..point as usize],
                &digits[point as usize..]
            )
        };
        return Ok(format!("{sign}{text}"));
    }
    let (head, tail) = digits.split_at(1);
    let fraction = if tail.is_empty() {
        String::new()
    } else {
        format!(".{tail}")
    };
    let exponent_sign = if exponent < 0 { "-" } else { "+" };
    Ok(format!(
        "{sign}{head}{fraction}e{exponent_sign}{}",
        exponent.abs()
    ))
}

/// JS 默认排序：按 UTF-16 码元比较。
fn utf16_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}

/// `canonicalizeValue`。
pub fn canonicalize(value: &Value) -> Result<String, String> {
    Ok(match value {
        Value::Null => "null".into(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => match (number.as_i64(), number.as_u64()) {
            (Some(int), _) => int.to_string(),
            (_, Some(uint)) => uint.to_string(),
            _ => js_number(number.as_f64().unwrap_or(f64::NAN))?,
        },
        Value::String(text) => serde_json::to_string(text).map_err(|error| error.to_string())?,
        Value::Array(items) => {
            let parts: Result<Vec<String>, String> = items.iter().map(canonicalize).collect();
            format!("[{}]", parts?.join(","))
        }
        Value::Object(object) => {
            let mut keys: Vec<&String> = object.keys().collect();
            keys.sort_by(|left, right| utf16_cmp(left, right));
            let mut parts = Vec::with_capacity(keys.len());
            for key in keys {
                let key_text = serde_json::to_string(key).map_err(|error| error.to_string())?;
                parts.push(format!("{key_text}:{}", canonicalize(&object[key])?));
            }
            format!("{{{}}}", parts.join(","))
        }
    })
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `sha256ConversationShareJson`。
pub fn sha256_json(value: &Value) -> Result<String, String> {
    Ok(sha256_hex(canonicalize(value)?.as_bytes()))
}

/// `verifyConversationShareIntegrity`。
pub fn verify(raw_rows: &Value, raw_artifacts: &Value, integrity: &Value) -> bool {
    let artifacts = match raw_artifacts.as_array() {
        Some(items) => {
            let mut items = items.clone();
            items.sort_by(|left, right| {
                locale_compare(
                    left["artifact_id"].as_str().unwrap_or_default(),
                    right["artifact_id"].as_str().unwrap_or_default(),
                )
            });
            Value::Array(
                items
                    .into_iter()
                    .map(|mut item| {
                        if let Some(object) = item.as_object_mut() {
                            for key in ARTIFACT_URL_KEYS {
                                object.remove(key);
                            }
                        }
                        item
                    })
                    .collect(),
            )
        }
        None => raw_artifacts.clone(),
    };
    sha256_json(raw_rows).ok().as_deref() == integrity["projection_sha256"].as_str()
        && sha256_json(&artifacts).ok().as_deref() == integrity["artifact_set_sha256"].as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_json_matches_javascript() {
        assert_eq!(js_number(1e21).unwrap(), "1e+21");
        assert_eq!(js_number(1.5).unwrap(), "1.5");
        assert_eq!(js_number(1e-7).unwrap(), "1e-7");
        assert_eq!(js_number(0.000001).unwrap(), "0.000001");
        assert_eq!(js_number(123456.789).unwrap(), "123456.789");
        assert_eq!(
            canonicalize(&json!({"b": [1, "x\u{2028}"], "a": {"d": null, "c": true}})).unwrap(),
            "{\"a\":{\"c\":true,\"d\":null},\"b\":[1,\"x\u{2028}\"]}"
        );
        assert_eq!(
            sha256_json(&json!([])).unwrap(),
            "4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945"
        );
    }
}
