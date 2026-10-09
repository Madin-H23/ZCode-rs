//! zod 4 严格对象校验的最小子集（docs/specs/rust-host-integration.md §2.5）。
//!
//! 只覆盖 CLI → Host 反向请求参数用到的类型：`string().trim().min(n)`、`number().int()` 与
//! 上下界、`boolean()`、`enum()`、`nullable()`、`.strict()` 对象与对象级 `refine`。产出与 zod 相同的
//! issue 顺序和英文默认文案，`flatten()` 结果供 JSON-RPC 错误的 `data` 使用；与真实 zod 的输出由
//! `scripts/rust-host/zod-flatten-fixtures.ts` 生成的 fixture 对拍。
//!
//! 规则（与 zod 4 一致）：字段按 schema 顺序校验，严格对象的未知键在本对象字段之后报一条
//! `unrecognized_keys`；`invalid_type`（含 `int` 检查）与 `invalid_value` 会中止，此时对象级
//! `refine` 不执行；`too_small` / `too_big` / 未知键 / `custom` 不中止。
use serde_json::{Map, Value, json};
use zcode_kit::js::string::trim as js_trim;

/// 数值检查（按声明顺序执行）。
#[derive(Debug, Clone, Copy)]
pub enum NumCheck {
    /// `.positive()` / `.gt(n)`。
    Gt(f64),
    /// `.min(n)` / `.nonnegative()`。
    Gte(f64),
    /// `.max(n)`。
    Lte(f64),
}

#[derive(Debug, Clone)]
pub enum Schema {
    /// `z.string()`，可选 `.trim()` 与 `.min(n)`（长度按 UTF-16 码元）。
    Str {
        trim: bool,
        min: Option<usize>,
    },
    /// `z.number()`，可选 `.int()` 与数值检查。
    Num {
        int: bool,
        checks: Vec<NumCheck>,
    },
    Bool,
    Enum(&'static [&'static str]),
    Nullable(Box<Schema>),
    /// `.strict()` 对象。
    Object(Vec<Field>),
}

#[derive(Debug, Clone)]
pub struct Field {
    pub name: &'static str,
    pub schema: Schema,
    pub optional: bool,
}

pub fn field(name: &'static str, schema: Schema) -> Field {
    Field {
        name,
        schema,
        optional: false,
    }
}

pub fn optional(name: &'static str, schema: Schema) -> Field {
    Field {
        name,
        schema,
        optional: true,
    }
}

/// `z.string().trim().min(1)`（zcode-protocol 的 `nonEmptyString`）。
pub fn non_empty_string() -> Schema {
    Schema::Str {
        trim: true,
        min: Some(1),
    }
}

pub fn string() -> Schema {
    Schema::Str {
        trim: false,
        min: None,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Issue {
    pub path: Vec<String>,
    pub message: String,
    /// 中止型 issue（zod 的 `continue !== true`）：之后的对象级 refine 不执行。
    pub aborting: bool,
}

/// zod 的 `parsedType`。
fn received(value: Option<&Value>) -> &'static str {
    match value {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
        Some(Value::String(_)) => "string",
        Some(Value::Number(_)) => "number",
        Some(Value::Bool(_)) => "boolean",
    }
}

/// JS `Number.prototype.toString` 在这里只会遇到整数与简单小数。
fn js_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e21 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

struct Checker {
    issues: Vec<Issue>,
}

impl Checker {
    fn push(&mut self, path: &[String], message: String, aborting: bool) {
        self.issues.push(Issue {
            path: path.to_vec(),
            message,
            aborting,
        });
    }

    fn invalid_type(&mut self, path: &[String], expected: &str, value: Option<&Value>) {
        self.push(
            path,
            format!(
                "Invalid input: expected {expected}, received {}",
                received(value)
            ),
            true,
        );
    }

    fn check(&mut self, schema: &Schema, value: Option<&Value>, path: &[String]) -> Option<Value> {
        match schema {
            Schema::Nullable(inner) => match value {
                Some(Value::Null) => Some(Value::Null),
                other => self.check(inner, other, path),
            },
            Schema::Str { trim, min } => {
                let Some(text) = value.and_then(Value::as_str) else {
                    self.invalid_type(path, "string", value);
                    return None;
                };
                let text = if *trim { js_trim(text) } else { text };
                if let Some(min) = min
                    && text.encode_utf16().count() < *min
                {
                    self.push(
                        path,
                        format!("Too small: expected string to have >={min} characters"),
                        false,
                    );
                }
                Some(json!(text))
            }
            Schema::Num { int, checks } => {
                let Some(number) = value.and_then(Value::as_f64) else {
                    self.invalid_type(path, "number", value);
                    return None;
                };
                if *int && number.fract() != 0.0 {
                    self.push(
                        path,
                        "Invalid input: expected int, received number".into(),
                        true,
                    );
                    return None;
                }
                for check in checks {
                    match *check {
                        NumCheck::Gt(limit) if number <= limit => self.push(
                            path,
                            format!("Too small: expected number to be >{}", js_number(limit)),
                            false,
                        ),
                        NumCheck::Gte(limit) if number < limit => self.push(
                            path,
                            format!("Too small: expected number to be >={}", js_number(limit)),
                            false,
                        ),
                        NumCheck::Lte(limit) if number > limit => self.push(
                            path,
                            format!("Too big: expected number to be <={}", js_number(limit)),
                            false,
                        ),
                        _ => {}
                    }
                }
                value.cloned()
            }
            Schema::Bool => match value {
                Some(Value::Bool(flag)) => Some(Value::Bool(*flag)),
                other => {
                    self.invalid_type(path, "boolean", other);
                    None
                }
            },
            Schema::Enum(options) => match value.and_then(Value::as_str) {
                Some(text) if options.contains(&text) => Some(json!(text)),
                _ => {
                    let expected: Vec<String> = options
                        .iter()
                        .map(|option| format!("\"{option}\""))
                        .collect();
                    self.push(
                        path,
                        format!("Invalid option: expected one of {}", expected.join("|")),
                        true,
                    );
                    None
                }
            },
            Schema::Object(fields) => {
                let Some(input) = value.and_then(Value::as_object) else {
                    self.invalid_type(path, "object", value);
                    return None;
                };
                let mut out = Map::new();
                for field in fields {
                    let child = input.get(field.name);
                    if child.is_none() && field.optional {
                        continue;
                    }
                    let mut child_path = path.to_vec();
                    child_path.push(field.name.to_owned());
                    if let Some(parsed) = self.check(&field.schema, child, &child_path) {
                        out.insert(field.name.to_owned(), parsed);
                    }
                }
                let unknown: Vec<String> = input
                    .keys()
                    .filter(|key| fields.iter().all(|field| field.name != key.as_str()))
                    .map(|key| format!("\"{key}\""))
                    .collect();
                if !unknown.is_empty() {
                    let noun = if unknown.len() == 1 { "key" } else { "keys" };
                    self.push(
                        path,
                        format!("Unrecognized {noun}: {}", unknown.join(", ")),
                        false,
                    );
                }
                Some(Value::Object(out))
            }
        }
    }
}

/// 对象级 refine：`(message, path)` 在校验失败时返回。
pub type Refine = fn(&Map<String, Value>) -> Option<(&'static str, &'static [&'static str])>;

/// `schema.safeParse(value)`：成功返回解析后的对象，失败返回全部 issue。
pub fn parse(
    schema: &Schema,
    value: Option<&Value>,
    refines: &[Refine],
) -> Result<Map<String, Value>, Vec<Issue>> {
    let mut checker = Checker { issues: Vec::new() };
    let parsed = checker.check(schema, value, &[]);
    if !checker.issues.iter().any(|issue| issue.aborting)
        && let Some(Value::Object(output)) = &parsed
    {
        for refine in refines {
            if let Some((message, path)) = refine(output) {
                checker.push(
                    &path
                        .iter()
                        .map(|segment| (*segment).to_owned())
                        .collect::<Vec<_>>(),
                    message.to_owned(),
                    false,
                );
            }
        }
    }
    match parsed {
        Some(Value::Object(output)) if checker.issues.is_empty() => Ok(output),
        _ => Err(checker.issues),
    }
}

/// `error.flatten()`：`{formErrors, fieldErrors}`，字段键按首次出现的顺序。
pub fn flatten(issues: &[Issue]) -> Value {
    let mut form = Vec::new();
    let mut fields: Map<String, Value> = Map::new();
    for issue in issues {
        match issue.path.first() {
            None => form.push(json!(issue.message)),
            Some(key) => {
                let entry = fields.entry(key.clone()).or_insert_with(|| json!([]));
                if let Some(list) = entry.as_array_mut() {
                    list.push(json!(issue.message));
                }
            }
        }
    }
    json!({"formErrors": form, "fieldErrors": fields})
}
