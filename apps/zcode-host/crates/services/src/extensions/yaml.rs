//! YAML frontmatter 读写（Node 侧用 `yaml` 包，这里用 `yaml-rust2`）。
//!
//! 只用于技能与 Subagent 的 frontmatter：解析结果转成 JSON 值，供与 TS 相同的字段读取逻辑使用；
//! 写出只覆盖 Subagent `mcpServers` 这类简单结构（块风格，需要时双引号）。
use serde_json::{Map, Number, Value};
use yaml_rust2::{Yaml, YamlLoader};

fn to_json(yaml: &Yaml) -> Value {
    match yaml {
        Yaml::Null | Yaml::BadValue | Yaml::Alias(_) => Value::Null,
        Yaml::Boolean(flag) => Value::Bool(*flag),
        Yaml::Integer(int) => Value::Number(Number::from(*int)),
        Yaml::Real(text) => yaml
            .as_f64()
            .or_else(|| text.parse::<f64>().ok())
            .and_then(Number::from_f64)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Yaml::String(text) => Value::String(text.clone()),
        Yaml::Array(items) => Value::Array(items.iter().map(to_json).collect()),
        Yaml::Hash(hash) => {
            let mut map = Map::new();
            for (key, value) in hash {
                let key = match key {
                    Yaml::String(text) | Yaml::Real(text) => text.clone(),
                    Yaml::Integer(int) => int.to_string(),
                    Yaml::Boolean(flag) => flag.to_string(),
                    Yaml::Null => "".to_owned(),
                    other => format!("{other:?}"),
                };
                map.insert(key, to_json(value));
            }
            super::json::js_order(Value::Object(map))
        }
    }
}

/// `parse(text)`：空文档为 `null`；语法错误（含重复键）返回 `Err`。
pub fn parse(text: &str) -> Result<Value, String> {
    let documents = YamlLoader::load_from_str(text).map_err(|error| error.to_string())?;
    if documents.len() > 1 {
        return Err("Source contains multiple documents".to_owned());
    }
    Ok(documents.first().map(to_json).unwrap_or(Value::Null))
}

/// 能否写成 plain scalar（保守：不像数字、布尔、null，不含 YAML 指示符）。
fn plain_safe(text: &str) -> bool {
    if text.is_empty() || text.trim() != text || text.contains('\n') {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "true" | "false" | "null" | "~" | ".inf" | "-.inf" | "+.inf" | ".nan"
    ) {
        return false;
    }
    if text.parse::<f64>().is_ok()
        || text.starts_with("0x")
        || text.starts_with("0o")
        || text.chars().all(|c| c.is_ascii_digit() || c == '_')
    {
        return false;
    }
    let first = text.chars().next().unwrap_or(' ');
    if "*?:,[]{}&!|>'\"%@`#".contains(first) {
        return false;
    }
    if first == '-' && text.chars().nth(1).is_none_or(|c| c == ' ') {
        return false;
    }
    !(text.contains(": ") || text.contains(" #") || text.ends_with(':'))
}

fn scalar(value: &Value) -> Option<String> {
    Some(match value {
        Value::Null => "null".to_owned(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => super::json::stringify(&Value::Number(number.clone())),
        Value::String(text) if plain_safe(text) => text.clone(),
        Value::String(text) => serde_json::to_string(text).unwrap_or_default(),
        Value::Array(items) if items.is_empty() => "[]".to_owned(),
        Value::Object(map) if map.is_empty() => "{}".to_owned(),
        _ => return None,
    })
}

fn emit(out: &mut Vec<String>, value: &Value, indent: usize) {
    let pad = " ".repeat(indent);
    match value {
        Value::Array(items) if !items.is_empty() => {
            for item in items {
                match scalar(item) {
                    Some(text) => out.push(format!("{pad}- {text}")),
                    None => {
                        let mut nested = Vec::new();
                        emit(&mut nested, item, indent + 2);
                        if let Some(first) = nested.first_mut() {
                            *first = format!("{pad}- {}", first.trim_start());
                        }
                        out.extend(nested);
                    }
                }
            }
        }
        Value::Object(map) if !map.is_empty() => {
            for (key, item) in map {
                let key = scalar(&Value::String(key.clone())).unwrap_or_default();
                match scalar(item) {
                    Some(text) => out.push(format!("{pad}{key}: {text}")),
                    None => {
                        out.push(format!("{pad}{key}:"));
                        emit(out, item, indent + 2);
                    }
                }
            }
        }
        other => out.push(format!("{pad}{}", scalar(other).unwrap_or_default())),
    }
}

/// `stringify({ [key]: value }, { lineWidth: 0 }).trimEnd()` 的行。
pub fn stringify_entry(key: &str, value: &Value) -> Vec<String> {
    let mut map = Map::new();
    map.insert(key.to_owned(), value.clone());
    let mut out = Vec::new();
    emit(&mut out, &Value::Object(map), 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_scalars_like_the_yaml_package() {
        let value = parse("name: demo\ncount: 3\nratio: 1.5\nflag: true\nempty:\nlist: [a, b]\n")
            .unwrap();
        assert_eq!(
            value,
            json!({"name":"demo","count":3,"ratio":1.5,"flag":true,"empty":null,"list":["a","b"]})
        );
        assert!(parse("a: 1\na: 2\n").is_err());
        assert!(parse("description: 触发: x\n").is_err());
    }

    #[test]
    fn emits_block_style() {
        let lines = stringify_entry(
            "mcpServers",
            &json!([{"name":"fs","args":["-y","pkg"],"env":{"A":"1"},"on":true}]),
        );
        assert_eq!(
            lines,
            vec![
                "mcpServers:",
                "  - name: fs",
                "    args:",
                "      - -y",
                "      - pkg",
                "    env:",
                "      A: \"1\"",
                "    on: true",
            ]
        );
    }
}
