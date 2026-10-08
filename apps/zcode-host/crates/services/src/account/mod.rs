//! 账号栈（docs/specs/rust-host-account.md）：凭据、网络客户端、OAuth 与账号相关 channel。
pub mod account_providers;
pub mod auth;
pub mod availability;
pub mod credential;
pub mod endpoint;
pub mod net;
pub mod oauth;
pub mod official_mcp;
pub mod oauth_adapters;
pub mod oauth_repo;
pub mod onboarding;
mod legacy_stub;

pub use legacy_stub::*;
#[cfg(test)]
pub(crate) mod oauth_tests;
