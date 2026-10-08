//! 远端 workspace（docs/specs/rust-host-remote.md）：窗口级连接注册表、远端 `@zcode/rpc` 连接、
//! scoped channel 表、window-controller 远端 source 与远端媒体预览代理。
pub mod registry;
pub mod target;
pub mod transport;

#[cfg(test)]
mod registry_tests;
