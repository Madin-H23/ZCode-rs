//! `String.prototype.localeCompare` 的近似（ICU root collation，非“shifted”模式）。
//!
//! 先比主权重（忽略大小写；空白 < 标点符号 < 数字 < 字母），全部相同再按大小写（小写在前），
//! 最后按码点。ASCII 范围内与 ICU 一致；非 ASCII 字母按小写码点排在 ASCII 字母之后（ICU 会把
//! 带重音字母归到基础字母旁，这里不处理）。只用于名称排序里的并列项。
use std::cmp::Ordering;

/// CLDR root 中 ASCII 空白与标点符号的主序。
const SYMBOL_ORDER: &str = "\t\n\u{b}\u{c}\r _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";

fn primary(c: char) -> (u8, u32) {
    if let Some(index) = SYMBOL_ORDER.find(c) {
        return (0, index as u32);
    }
    if c.is_ascii_digit() {
        return (1, c as u32);
    }
    if c.is_alphabetic() {
        let lower = c.to_lowercase().next().unwrap_or(c);
        return (if c.is_ascii() { 2 } else { 3 }, lower as u32);
    }
    (4, c as u32)
}

pub fn locale_compare(left: &str, right: &str) -> Ordering {
    let primary_key = |text: &str| text.chars().map(primary).collect::<Vec<_>>();
    let case_key = |text: &str| text.chars().map(char::is_uppercase).collect::<Vec<_>>();
    primary_key(left)
        .cmp(&primary_key(right))
        .then_with(|| case_key(left).cmp(&case_key(right)))
        .then_with(|| left.cmp(right))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_like_icu_root_for_ascii() {
        let mut names = vec!["b", "B", "a-b", "ab", "_x", "10", "2", "Main", "main", "a"];
        names.sort_by(|left, right| locale_compare(left, right));
        assert_eq!(
            names,
            vec!["_x", "10", "2", "a", "a-b", "ab", "b", "B", "main", "Main"]
        );
    }
}
