//! 文件、路径、秘密值、脱敏与 JS 语义文本工具的公共基础（docs/specs/rust-u0-foundation.md §3.2）。
//!
//! - [`fs::atomic_write`]：唯一的“临时文件 + rename”原子写实现。
//! - [`paths::home_dir`] / [`paths::expand_home`]：用户目录解析与 `~` 展开，绝不返回空路径。
//! - [`secret::RedactedString`]：Debug / Display 不泄露的秘密值。
//! - [`redact::redact_secrets`]：按值脱敏，用于离开进程的文本（日志文件、发给 Main 的日志）。
//! - [`js`]：与 JavaScript 语义一致的字符串、`JSON.stringify` 与 `toISOString`（CLI 与 Host 共用）。

pub mod fs;
pub mod js;
pub mod paths;
pub mod redact;
pub mod secret;
