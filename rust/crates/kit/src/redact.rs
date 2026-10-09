//! 按值脱敏（docs/specs/rust-u0-foundation.md §3.2）。
//!
//! 规则移植自 Codex `codex-rs/secrets/src/sanitizer.rs`（Bearer、OpenAI 风格 key、AWS access key id、
//! `api_key=…` 一类赋值），并补充 GitHub token 与带连字符的 `sk-` key（`sk-proj-…`、`sk-ant-…`）。
//! 尽力而为：只覆盖形态明确的秘密值；按字段名脱敏仍由各日志出口负责。

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

const REDACTED: &str = "[REDACTED]";

/// （模式，替换文本）。Bearer 在前：`Bearer sk-…` 整体替换为 `Bearer [REDACTED]`，不会留下两段标记。
const RULES: &[(&str, &str)] = &[
    // 只有关键字大小写不敏感；token 字符类保持 ASCII，避免 (?i) 让 `[A-Za-z]` 匹配到 K（U+212A）这类字符。
    (
        r"(?i:\bBearer)[ \t]+[A-Za-z0-9._~+/-]{16,}=*",
        "Bearer [REDACTED]",
    ),
    (r"\bsk-[A-Za-z0-9_-]{20,}", REDACTED),
    (r"\bAKIA[0-9A-Z]{16}\b", REDACTED),
    (r"\bgh[pousr]_[A-Za-z0-9]{36,}\b", REDACTED),
    (r"\bgithub_pat_[A-Za-z0-9_]{22,}\b", REDACTED),
    (
        r#"(?i)\b(api[_-]?key|token|secret|password)\b(\s*[:=]\s*)(["']?)[^\s"']{8,}"#,
        "$1$2$3[REDACTED]",
    ),
];

static COMPILED: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    // 模式是上面的常量，每条都有单元测试覆盖；编译失败的规则只会在开发期被测试发现，运行期不 panic。
    RULES
        .iter()
        .filter_map(|(pattern, replacement)| {
            Regex::new(pattern).ok().map(|regex| (regex, *replacement))
        })
        .collect()
});

/// 把文本中形如秘密值的片段替换为 `[REDACTED]`。没有命中时返回 `Cow::Borrowed`，不分配。
pub fn redact_secrets(text: &str) -> Cow<'_, str> {
    let mut redacted = Cow::Borrowed(text);
    for (regex, replacement) in COMPILED.iter() {
        if let Cow::Owned(replaced) = regex.replace_all(&redacted, *replacement) {
            redacted = Cow::Owned(replaced);
        }
    }
    redacted
}

#[cfg(test)]
#[path = "redact_tests.rs"]
mod tests;
