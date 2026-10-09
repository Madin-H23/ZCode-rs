//! Agent 子系统：每个 workspace 一个 runtime 实例，经 NDJSON 行通信（docs/specs/rust-host.md R2）。
//! 到 runtime 的链路（选择、子进程或进程内、结束与回收）在 `zcode-host-runtime-link`
//! （docs/specs/rust-host-cli-boundary.md §3.2），这里不区分传输。
pub mod api;
pub mod api_plugins;
pub mod browser;
pub mod client;
pub mod extensions;
mod flow;
pub mod members;
pub mod notifications;
pub mod process_events;
pub mod runtime;
pub mod schema;
pub mod service;
pub mod session;
pub mod session_events;
pub mod sessions_index;
pub mod storage_prep;
pub mod wire;

pub use runtime::{
    AgentProcess, AgentRuntime, AgentRuntimeConfig, Frame, FrameKind, Lane, RuntimePolicy,
    WorkspaceTarget,
};

#[cfg(test)]
mod lane_tests;
