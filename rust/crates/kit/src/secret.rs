//! 不会被意外打印的秘密值（docs/specs/rust-u0-foundation.md §3.2）。

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// API key、鉴权 header 等秘密值。
///
/// `Debug` / `Display` 只输出 `<redacted>`，把它放进日志、错误信息或 `{:?}` 都不会泄露原值；
/// serde 按原值透明地序列化与反序列化（发给 provider 时需要原值）。读取原值只能经 [`expose`](Self::expose)，
/// 调用处因此一眼可见。
#[derive(Clone, PartialEq, Eq)]
pub struct RedactedString(String);

impl RedactedString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// 原值。只在真正需要发出秘密值的地方调用（例如构造请求 header）。
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RedactedString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl fmt::Display for RedactedString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl Serialize for RedactedString {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RedactedString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self)
    }
}

#[cfg(test)]
#[path = "secret_tests.rs"]
mod tests;
