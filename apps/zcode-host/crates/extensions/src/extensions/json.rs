//! `JSON.parse` / `JSON.stringify` 语义的 JSON 读写（docs/specs/rust-host-extensions.md §2.1）。
//!
//! 与 Node 写入方共享的配置文件必须逐字节一致：
//! - 读取：`serde_json`（preserve_order）对重复键的处理与 `JSON.parse` 相同（取最后的值、保留首次
//!   出现的位置）；之后把数组下标形态的键按数值升序提到最前，与 JS 对象的属性顺序一致；
//! - 写出：`JSON.stringify(value, null, 2)`，数字按 ECMAScript `Number::toString` 输出。
use serde_json::{Map, Value};
use zcode_kit::js::json as js_json;

/// 数组下标形态的键：`0` 或无前导零、小于 2^32 − 1 的十进制整数。
fn array_index(key: &str) -> Option<u32> {
    if key.is_empty() || key.len() > 10 || (key.len() > 1 && key.starts_with('0')) {
        return None;
    }
    if !key.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    key.parse::<u64>()
        .ok()
        .filter(|v| *v < u64::from(u32::MAX))
        .map(|v| v as u32)
}

/// 按 JS 属性遍历顺序重排对象键（递归）。
pub fn js_order(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(js_order).collect()),
        Value::Object(map) => {
            let mut indexed: Vec<(u32, String, Value)> = Vec::new();
            let mut rest: Vec<(String, Value)> = Vec::new();
            for (key, value) in map {
                match array_index(&key) {
                    Some(index) => indexed.push((index, key, js_order(value))),
                    None => rest.push((key, js_order(value))),
                }
            }
            indexed.sort_by_key(|(index, _, _)| *index);
            let mut out = Map::new();
            for (_, key, value) in indexed {
                out.insert(key, value);
            }
            for (key, value) in rest {
                out.insert(key, value);
            }
            Value::Object(out)
        }
        other => other,
    }
}

/// `JSON.parse`；失败时返回 serde 的错误文本。
pub fn parse(text: &str) -> Result<Value, String> {
    serde_json::from_str::<Value>(text)
        .map(js_order)
        .map_err(|error| error.to_string())
}

/// `JSON.parse(text)` 且结果是对象（`isRecord`）。
pub fn parse_record(text: &str) -> Option<Map<String, Value>> {
    match parse(text) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// `JSON.stringify(value)`（紧凑）。
pub fn stringify(value: &Value) -> String {
    js_json::stringify(value)
}

fn write_string(out: &mut String, text: &str) {
    out.push_str(&serde_json::to_string(text).unwrap_or_default());
}

fn write_number(out: &mut String, number: &serde_json::Number) {
    // js_json::stringify 对单个数字给出 JS 形态（安全整数原样，其余按 Number::toString）。
    out.push_str(&js_json::stringify(&Value::Number(number.clone())));
}

fn ordered_entries(map: &Map<String, Value>) -> Vec<(&String, &Value)> {
    let mut indexed: Vec<(u32, &String, &Value)> = map
        .iter()
        .filter_map(|(key, value)| array_index(key).map(|index| (index, key, value)))
        .collect();
    indexed.sort_by_key(|(index, _, _)| *index);
    indexed
        .into_iter()
        .map(|(_, key, value)| (key, value))
        .chain(map.iter().filter(|(key, _)| array_index(key).is_none()))
        .collect()
}

fn write_pretty(out: &mut String, value: &Value, depth: usize) {
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        for _ in 0..depth {
            out.push_str("  ");
        }
    };
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => write_number(out, number),
        Value::String(text) => write_string(out, text),
        Value::Array(items) if items.is_empty() => out.push_str("[]"),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                newline(out, depth + 1);
                write_pretty(out, item, depth + 1);
            }
            newline(out, depth);
            out.push(']');
        }
        Value::Object(map) if map.is_empty() => out.push_str("{}"),
        Value::Object(map) => {
            out.push('{');
            for (index, (key, item)) in ordered_entries(map).into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                newline(out, depth + 1);
                write_string(out, key);
                out.push_str(": ");
                write_pretty(out, item, depth + 1);
            }
            newline(out, depth);
            out.push('}');
        }
    }
}

/// `JSON.stringify(value, null, 2)`。
pub fn pretty(value: &Value) -> String {
    let mut out = String::new();
    write_pretty(&mut out, value, 0);
    out
}

/// `${JSON.stringify(value, null, 2)}\n`：配置文件的标准落盘形态。
pub fn pretty_line(value: &Value) -> String {
    let mut out = pretty(value);
    out.push('\n');
    out
}

/// JS number → JSON 值：安全整数写成整数（否则 serde 会输出 `5000.0`），其余为浮点。
pub fn number(value: f64) -> Value {
    const SAFE: f64 = 9_007_199_254_740_991.0;
    if value.fract() == 0.0 && value.abs() <= SAFE {
        return Value::Number(serde_json::Number::from(value as i64));
    }
    serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
}

pub fn is_record(value: &Value) -> bool {
    value.is_object()
}

/// JS `delete object[key]`：保持其余键的顺序（`Map::remove` 是 swap_remove，会打乱顺序）。
pub fn delete(map: &mut Map<String, Value>, key: &str) -> Option<Value> {
    map.shift_remove(key)
}

/// `isRecord(value) ? value : {}` 的只读视图。
pub fn record(value: Option<&Value>) -> Map<String, Value> {
    match value {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    }
}

/// `typeof value === "string" ? value : undefined`。
pub fn str_of(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str)
}

/// `Array.isArray(value) ? value.filter(isString) : []`。
pub fn string_array(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// `Object.entries(value)` 中布尔值的条目。
pub fn boolean_record(value: Option<&Value>) -> Vec<(String, bool)> {
    match value {
        Some(Value::Object(map)) => map
            .iter()
            .filter_map(|(key, value)| value.as_bool().map(|flag| (key.clone(), flag)))
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_and_pretty_follow_javascript() {
        let parsed = parse(r#"{"b":1.0,"10":2,"a":{"x":[],"y":{}},"01":3,"0":4,"b":5}"#).unwrap();
        assert_eq!(
            pretty(&parsed),
            "{\n  \"0\": 4,\n  \"10\": 2,\n  \"b\": 5,\n  \"a\": {\n    \"x\": [],\n    \"y\": {}\n  },\n  \"01\": 3\n}"
        );
        assert_eq!(pretty(&json!(1e21)), "1e+21");
        assert_eq!(pretty_line(&json!({})), "{}\n");
        assert_eq!(stringify(&json!({"a":[1,"x"]})), r#"{"a":[1,"x"]}"#);
    }
}
