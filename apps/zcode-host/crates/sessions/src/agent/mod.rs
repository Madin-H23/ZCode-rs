//! Agent 子系统：每个 workspace 一个 runtime 实例（CLI 子进程，或进程内的嵌入式 runtime），经
//! NDJSON 行通信（docs/specs/rust-host.md R2、rust-u3-single-process.md）。
pub mod api;
pub mod api_plugins;
pub mod browser;
pub mod client;
pub mod command;
pub mod embedded;
pub mod extensions;
mod flow;
pub mod members;
pub mod notifications;
pub mod process;
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
