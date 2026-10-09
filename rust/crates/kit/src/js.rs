//! 与 JavaScript 语义一致的文本工具（docs/specs/rust-host-cli-boundary.md §3.1）：移植 Node 逻辑时
//! CLI 与 Host 都要用的 `String.prototype.trim` / UTF-16 长度、`JSON.stringify` 与
//! `Date.prototype.toISOString`。

mod date;
pub mod json;
pub mod string;

pub use date::iso_timestamp;
