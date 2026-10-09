//! Subagent Markdown 的解析与生成（packages/services/src/subagents/subagentMarkdown.ts）。
//! 解析按 runtime 的宽松 frontmatter 语义（YAML 解析结果被宽松读取覆盖），保证 CLI 能加载的
//! profile 在设置页同样可见。
use serde_json::{Map, Value, json};
use zcode_kit::js::string::trim as js_trim;

use super::super::{json as js, yaml};
use super::selection;

const COLORS: [&str; 8] = [
    "red", "blue", "green", "yellow", "purple", "orange", "pink", "cyan",
];
const PERMISSION_MODES: [&str; 2] = ["auto", "plan"];

pub enum Parsed {
    Agent(Map<String, Value>),
    Diagnostic(Value),
}

fn diagnostic(code: &str, message: String, path: &str) -> Parsed {
    Parsed::Diagnostic(json!({ "code": code, "message": message, "path": path }))
}

/// `createAgentStateId`。
pub fn agent_state_id(name: &str, scope: &str, source: &str) -> String {
    format!("{source}:{scope}:{}", js_trim(name).to_lowercase())
}

/// `createPluginAgentStateId`。
pub fn plugin_agent_state_id(plugin_id: &str, name: &str) -> String {
    format!("plugin:{plugin_id}:{}", js_trim(name).to_lowercase())
}

fn split_frontmatter(content: &str) -> (Option<String>, String) {
    let normalized = content
        .strip_prefix('\u{feff}')
        .unwrap_or(content)
        .replace("\r\n", "\n");
    if !normalized.starts_with("---") {
        return (None, normalized);
    }
    let lines: Vec<&str> = normalized.split('\n').collect();
    if js_trim(lines[0]) != "---" {
        return (None, normalized.clone());
    }
    let Some(end) = lines
        .iter()
        .enumerate()
        .position(|(index, line)| index > 0 && js_trim(line) == "---")
    else {
        return (None, normalized.clone());
    };
    (Some(lines[1..end].join("\n")), lines[end + 1..].join("\n"))
}

fn is_ws(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

/// `stripInlineComment`：引号外、前面是空白的 `#` 起为注释。
fn strip_inline_comment(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut quote: Option<char> = None;
    for index in 0..chars.len() {
        let c = chars[index];
        if (c == '"' || c == '\'') && (index == 0 || chars[index - 1] != '\\') {
            quote = match quote {
                Some(open) if open == c => None,
                Some(open) => Some(open),
                None => Some(c),
            };
        }
        if quote.is_none() && c == '#' && index > 0 && is_ws(chars[index - 1]) {
            return chars[..index]
                .iter()
                .collect::<String>()
                .trim_end_matches(is_ws)
                .to_owned();
        }
    }
    value.to_owned()
}

fn split_top_level(value: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut depth = 0usize;
    for c in value.chars() {
        if (c == '"' || c == '\'') && quote.is_none() {
            quote = Some(c);
            current.push(c);
            continue;
        }
        if quote == Some(c) {
            quote = None;
            current.push(c);
            continue;
        }
        if quote.is_none() && c == '(' {
            depth += 1;
        }
        if quote.is_none() && c == ')' {
            depth = depth.saturating_sub(1);
        }
        if quote.is_none() && depth == 0 && c == ',' {
            if !js_trim(&current).is_empty() {
                items.push(js_trim(&current).to_owned());
            }
            current.clear();
            continue;
        }
        current.push(c);
    }
    if !js_trim(&current).is_empty() {
        items.push(js_trim(&current).to_owned());
    }
    items
}

fn unquote(value: &str) -> String {
    let quoted = !value.is_empty()
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')));
    if quoted {
        let inner: Vec<char> = value.chars().collect();
        if inner.len() < 2 {
            return String::new();
        }
        return inner[1..inner.len() - 1].iter().collect();
    }
    value.to_owned()
}

fn scalar_value(raw: &str) -> Value {
    let value = strip_inline_comment(js_trim(raw));
    match value.as_str() {
        "true" => return json!(true),
        "false" => return json!(false),
        _ => {}
    }
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        return js::number(value.parse::<f64>().unwrap_or(0.0));
    }
    if value.starts_with('[') && value.ends_with(']') && value.len() >= 2 {
        return Value::Array(
            split_top_level(&value[1..value.len() - 1])
                .iter()
                .map(|item| json!(unquote(&strip_inline_comment(js_trim(item)))))
                .collect(),
        );
    }
    if value.starts_with('{') && value.ends_with('}') {
        // 非法 inline mapping 保留原字符串；下游 schema 会拒绝它，而不是让整个文件消失。
        return js::parse(&value).unwrap_or(Value::String(value));
    }
    Value::String(unquote(&value))
}

/// `parseLooseFrontmatter`。
fn loose_frontmatter(text: &str) -> Map<String, Value> {
    let mut values = Map::new();
    let mut pending: Option<String> = None;
    for raw in text.split('\n') {
        let line = raw
            .strip_suffix('\r')
            .unwrap_or(raw)
            .trim_end_matches(is_ws);
        if js_trim(line).is_empty() || line.trim_start_matches(is_ws).starts_with('#') {
            continue;
        }
        let stripped = line.trim_start_matches(is_ws);
        if let Some(pending_key) = &pending
            && let Some(rest) = stripped.strip_prefix('-')
            && rest.starts_with(is_ws)
        {
            let item = rest.trim_start_matches(is_ws);
            let mut list = values
                .get(pending_key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            list.push(scalar_value(item));
            values.insert(pending_key.clone(), Value::Array(list));
            continue;
        }
        pending = None;
        let key_len = line
            .char_indices()
            .take_while(|(index, c)| {
                if *index == 0 {
                    c.is_ascii_alphabetic()
                } else {
                    c.is_ascii_alphanumeric() || *c == '_' || *c == '-'
                }
            })
            .count();
        if key_len == 0 {
            continue;
        }
        let key = &line[..key_len];
        let after = line[key_len..].trim_start_matches(is_ws);
        let Some(value) = after.strip_prefix(':') else {
            continue;
        };
        let value = value.trim_start_matches(is_ws);
        if js_trim(value).is_empty() {
            values.insert(key.to_owned(), json!([]));
            pending = Some(key.to_owned());
            continue;
        }
        values.insert(key.to_owned(), scalar_value(value));
    }
    values
}

fn runtime_frontmatter(text: &str) -> Map<String, Value> {
    let loose = loose_frontmatter(text);
    let yaml_values = match yaml::parse(text) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    };
    let mut merged = yaml_values.clone();
    for (key, value) in loose {
        merged.insert(key, value);
    }
    if let Some(Value::Array(servers)) = yaml_values.get("mcpServers") {
        merged.insert("mcpServers".into(), Value::Array(servers.clone()));
    }
    merged
}

fn scalar_string(value: Option<&Value>) -> Option<String> {
    let trimmed = js_trim(value?.as_str()?);
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn optional_bool(value: Option<&Value>) -> Option<bool> {
    match value? {
        Value::Bool(flag) => Some(*flag),
        Value::String(text) if text.eq_ignore_ascii_case("true") => Some(true),
        Value::String(text) if text.eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    }
}

fn positive_integer(value: Option<&Value>) -> Option<Value> {
    match value? {
        Value::Number(number) => {
            let n = number.as_f64()?;
            (n.fract() == 0.0 && n > 0.0).then(|| js::number(n))
        }
        Value::String(text) if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => {
            let n = text.parse::<f64>().ok()?;
            (n > 0.0).then(|| js::number(n))
        }
        _ => None,
    }
}

fn non_empty(values: Vec<String>) -> Option<Vec<String>> {
    let filtered: Vec<String> = values.into_iter().filter(|item| !item.is_empty()).collect();
    (!filtered.is_empty()).then_some(filtered)
}

fn string_list(value: Option<&Value>) -> Option<Vec<String>> {
    match value? {
        Value::Array(items) => non_empty(
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|item| js_trim(item).to_owned())
                .collect(),
        ),
        Value::String(text) => non_empty(
            text.split(|c: char| c == ',' || is_ws(c))
                .map(|item| js_trim(item).to_owned())
                .filter(|item| !item.is_empty())
                .collect(),
        ),
        _ => None,
    }
}

fn tool_list(value: Option<&Value>) -> Option<Vec<String>> {
    match value? {
        Value::Array(_) => string_list(value),
        Value::String(text) => {
            let mut result = Vec::new();
            let mut current = String::new();
            let mut depth = 0usize;
            for c in text.chars() {
                if c == '(' {
                    depth += 1;
                }
                if c == ')' {
                    depth = depth.saturating_sub(1);
                }
                if depth == 0 && (c == ',' || is_ws(c)) {
                    if !js_trim(&current).is_empty() {
                        result.push(js_trim(&current).to_owned());
                    }
                    current.clear();
                    continue;
                }
                current.push(c);
            }
            if !js_trim(&current).is_empty() {
                result.push(js_trim(&current).to_owned());
            }
            non_empty(result)
        }
        _ => None,
    }
}

/// `parseSubagentMarkdown`。
pub fn parse(content: &str, path: &str, scope: &str) -> Parsed {
    let (frontmatter, body) = split_frontmatter(content);
    let Some(frontmatter) = frontmatter else {
        return diagnostic(
            "agent_missing_frontmatter",
            format!("Agent Markdown must include frontmatter: {path}"),
            path,
        );
    };
    let fm = runtime_frontmatter(&frontmatter);
    let Some(name) = scalar_string(fm.get("name")) else {
        return diagnostic(
            "agent_missing_required_frontmatter",
            format!("Agent frontmatter must include name: {path}"),
            path,
        );
    };
    let Some(description) =
        scalar_string(fm.get("description")).map(|text| text.replace("\\n", "\n"))
    else {
        return diagnostic(
            "agent_missing_required_frontmatter",
            format!("Agent frontmatter must include description: {path}"),
            path,
        );
    };
    let source = if scope == "built-in" {
        "built-in"
    } else {
        "user"
    };
    let enumerated = |key: &str, allowed: &[&str]| {
        scalar_string(fm.get(key)).filter(|value| allowed.contains(&value.as_str()))
    };
    let mut agent = Map::new();
    agent.insert("id".into(), json!(agent_state_id(&name, scope, source)));
    agent.insert("name".into(), json!(name));
    agent.insert("description".into(), json!(description));
    agent.insert("systemPrompt".into(), json!(js_trim(&body)));
    if let Some(color) = enumerated("color", &COLORS) {
        agent.insert("color".into(), json!(color));
    }
    if let Some(selection) =
        selection::parse_markdown_selection(fm.get("model"), fm.get("thoughtLevel"))
    {
        agent.insert("modelSelection".into(), selection);
    }
    if let Some(tools) = tool_list(fm.get("tools")) {
        agent.insert("tools".into(), json!(tools));
    }
    if let Some(tools) = tool_list(fm.get("disallowedTools")) {
        agent.insert("disallowedTools".into(), json!(tools));
    }
    if let Some(skills) = string_list(fm.get("skills")) {
        agent.insert("skills".into(), json!(skills));
    }
    if let Some(mode) = enumerated("permissionMode", &PERMISSION_MODES) {
        agent.insert("permissionMode".into(), json!(mode));
    }
    if let Some(turns) = positive_integer(fm.get("maxTurns")) {
        agent.insert("maxTurns".into(), turns);
    }
    if let Some(flag) = optional_bool(fm.get("background")) {
        agent.insert("background".into(), json!(flag));
    }
    if let Some(flag) = optional_bool(fm.get("injectAgentsMd")) {
        agent.insert("injectAgentsMd".into(), json!(flag));
    }
    if let Some(Value::Array(servers)) = fm.get("mcpServers") {
        agent.insert("mcpServers".into(), Value::Array(servers.clone()));
    }
    agent.insert("path".into(), json!(path));
    agent.insert("scope".into(), json!(scope));
    agent.insert("source".into(), json!(source));
    agent.insert("enabled".into(), json!(true));
    // 只有内置 agent 真正只读；插件 agent 由发现阶段显式覆盖为只读。
    agent.insert("readOnly".into(), json!(scope == "built-in"));
    Parsed::Agent(agent)
}

fn escape_yaml(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// `isSafePlainYamlScalar`。
fn safe_plain(value: &str) -> bool {
    if value.is_empty() || js_trim(value) != value {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    if matches!(lower.as_str(), "false" | "null" | "true" | "~") {
        return false;
    }
    if looks_numeric(&lower) {
        return false;
    }
    if value.starts_with(|c: char| "*?:,[]{}&!|>'\"%@`-".contains(c)) {
        return false;
    }
    let colon_space = value
        .char_indices()
        .any(|(index, c)| c == ':' && value[index + 1..].chars().next().is_some_and(is_ws));
    if value.contains('#') || colon_space {
        return false;
    }
    let mut chars = value.chars();
    let first = chars.next().unwrap_or(' ');
    (first.is_ascii_alphanumeric() || "_./@*".contains(first))
        && chars.all(|c| c.is_ascii_alphanumeric() || "_./@*()-".contains(c) || is_ws(c))
}

/// `/^[-+]?(?:\d+|\d*\.\d+)(?:e[-+]?\d+)?$/i`（已小写）。
fn looks_numeric(value: &str) -> bool {
    let rest = value.strip_prefix(['-', '+']).unwrap_or(value);
    let (mantissa, exponent) = match rest.find('e') {
        Some(index) => (&rest[..index], Some(&rest[index + 1..])),
        None => (rest, None),
    };
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = digits(mantissa)
        || mantissa
            .split_once('.')
            .is_some_and(|(int, frac)| (int.is_empty() || digits(int)) && digits(frac));
    let exponent_ok =
        exponent.is_none_or(|exp| digits(exp.strip_prefix(['-', '+']).unwrap_or(exp)));
    mantissa_ok && exponent_ok
}

fn format_scalar(value: &str) -> String {
    if safe_plain(value) {
        value.to_owned()
    } else {
        format!("\"{}\"", escape_yaml(value))
    }
}

fn append_scalar(lines: &mut Vec<String>, key: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        lines.push(format!("{key}: {}", format_scalar(value)));
    }
}

fn append_list(lines: &mut Vec<String>, key: &str, values: Option<&Vec<Value>>) {
    let Some(values) = values.filter(|values| !values.is_empty()) else {
        return;
    };
    lines.push(format!("{key}:"));
    for value in values {
        let text = match value {
            Value::String(text) => text.clone(),
            other => js::stringify(other),
        };
        lines.push(format!("  - {}", format_scalar(&text)));
    }
}

/// `serializeSubagentMarkdown`。
pub fn serialize(config: &Map<String, Value>) -> String {
    let text = |key: &str| {
        config
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let mut lines = vec![
        format!("name: \"{}\"", escape_yaml(&text("name"))),
        format!("description: \"{}\"", escape_yaml(&text("description"))),
    ];
    append_scalar(
        &mut lines,
        "color",
        config.get("color").and_then(Value::as_str),
    );
    if let Some(selection) = config
        .get("modelSelection")
        .filter(|value| value.is_object())
    {
        append_scalar(
            &mut lines,
            "model",
            Some(&selection::format_markdown_model(selection)),
        );
        append_scalar(
            &mut lines,
            "thoughtLevel",
            selection
                .get("options")
                .and_then(|options| options.get("reasoningLevel"))
                .and_then(Value::as_str),
        );
    }
    append_list(
        &mut lines,
        "tools",
        config.get("tools").and_then(Value::as_array),
    );
    append_list(
        &mut lines,
        "disallowedTools",
        config.get("disallowedTools").and_then(Value::as_array),
    );
    append_list(
        &mut lines,
        "skills",
        config.get("skills").and_then(Value::as_array),
    );
    append_scalar(
        &mut lines,
        "permissionMode",
        config.get("permissionMode").and_then(Value::as_str),
    );
    if let Some(turns) = config.get("maxTurns").filter(|value| !value.is_null()) {
        lines.push(format!("maxTurns: {}", js::stringify(turns)));
    }
    for key in ["background", "injectAgentsMd"] {
        if let Some(flag) = config.get(key).filter(|value| !value.is_null()) {
            lines.push(format!(
                "{key}: {}",
                if flag == &Value::Bool(true) {
                    "true"
                } else {
                    "false"
                }
            ));
        }
    }
    if let Some(servers) = config
        .get("mcpServers")
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
    {
        if servers.iter().all(Value::is_string) {
            append_list(&mut lines, "mcpServers", Some(servers));
        } else {
            // 对象数组按 YAML 结构写回，避免编辑其他字段时清空 MCP 配置。
            lines.extend(yaml::stringify_entry(
                "mcpServers",
                &Value::Array(servers.clone()),
            ));
        }
    }
    let prompt = js_trim(&text("systemPrompt")).to_owned();
    let body = if prompt.is_empty() {
        "\n".to_owned()
    } else {
        format!("\n{prompt}\n")
    };
    format!("---\n{}\n---\n{body}", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_loose_frontmatter_like_runtime() {
        let content = "---\nname: reviewer\ndescription: Reviews code\\nCarefully\ntools: Read, Bash(git diff:*) Grep\nskills:\n  - a\n  - b # c\nmaxTurns: 5\nbackground: \"true\"\ncolor: teal\nmodel: openai/gpt-5$high\n---\nPrompt body\n";
        let Parsed::Agent(agent) = parse(content, "/u/reviewer.md", "user") else {
            panic!("expected agent");
        };
        assert_eq!(agent["id"], "user:user:reviewer");
        assert_eq!(agent["description"], "Reviews code\nCarefully");
        assert_eq!(agent["tools"], json!(["Read", "Bash(git diff:*)", "Grep"]));
        assert_eq!(agent["skills"], json!(["a", "b"]));
        assert_eq!(agent["maxTurns"], 5);
        assert_eq!(agent["background"], true);
        assert!(agent.get("color").is_none());
        assert_eq!(agent["modelSelection"]["options"]["reasoningLevel"], "high");
        assert_eq!(agent["systemPrompt"], "Prompt body");
        let Parsed::Diagnostic(missing) = parse("---\nname: x\n---\n", "/p.md", "user") else {
            panic!("expected diagnostic");
        };
        assert_eq!(missing["code"], "agent_missing_required_frontmatter");
    }

    #[test]
    fn serializes_like_typescript() {
        let config = json!({
            "name": "reviewer",
            "description": "Says \"hi\"",
            "systemPrompt": "  Do it  ",
            "tools": ["*", "Read"],
            "modelSelection": {"providerId":"openai","modelId":"gpt","options":{"reasoningLevel":"high"}},
            "maxTurns": 3,
            "background": false,
        });
        assert_eq!(
            serialize(config.as_object().unwrap()),
            "---\nname: \"reviewer\"\ndescription: \"Says \\\"hi\\\"\"\nmodel: openai/gpt\nthoughtLevel: high\ntools:\n  - \"*\"\n  - Read\nmaxTurns: 3\nbackground: false\n---\n\nDo it\n"
        );
        assert!(looks_numeric("1e5") && looks_numeric(".5") && !looks_numeric("1.2.3"));
    }
}
