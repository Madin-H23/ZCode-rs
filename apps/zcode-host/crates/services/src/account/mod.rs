//! 账号栈（docs/specs/rust-host-account.md）：凭据、网络客户端、OAuth 与账号相关 channel。
pub mod credential;
mod legacy_stub;

pub use legacy_stub::*;
