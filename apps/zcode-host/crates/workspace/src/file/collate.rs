//! `String.prototype.localeCompare`（ICU 根排序）的近似：先按主键比较（标点 < 数字 < 字母，
//! 字母不分大小写），主键相同再让小写排在大写前面。用于目录列表与索引排序的显示顺序；
//! 不追求与 ICU 在所有脚本上完全一致。
use std::cmp::Ordering;

/// CLDR 根排序里 ASCII 标点 / 符号的相对顺序（空白最前）。
const PUNCTUATION_ORDER: &str = " _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";

fn primary(c: char) -> (u8, u32) {
    if let Some(index) = PUNCTUATION_ORDER.find(c) {
        return (0, index as u32);
    }
    if c.is_ascii_digit() {
        return (1, c as u32);
    }
    if c.is_alphabetic() {
        let lower = c.to_lowercase().next().unwrap_or(c);
        return (2, lower as u32);
    }
    if c.is_whitespace() || c.is_ascii_punctuation() {
        return (0, 1000 + c as u32);
    }
    (3, c as u32)
}

pub fn locale_compare(left: &str, right: &str) -> Ordering {
    let primary_order = left.chars().map(primary).cmp(right.chars().map(primary));
    if primary_order != Ordering::Equal {
        return primary_order;
    }
    // 第三级：同一字母小写在前（ICU 根排序 "a" < "A"）。
    for (a, b) in left.chars().zip(right.chars()) {
        if a != b {
            return match (a.is_lowercase(), b.is_lowercase()) {
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                _ => a.cmp(&b),
            };
        }
    }
    left.len().cmp(&right.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_like_icu_root_for_common_names() {
        let mut names = vec!["b", "B", "a", "_x", "10", "9", ".env", "Z", "a-b", "ab"];
        names.sort_by(|a, b| locale_compare(a, b));
        assert_eq!(
            names,
            ["_x", ".env", "10", "9", "a", "a-b", "ab", "b", "B", "Z"]
        );
    }
}
