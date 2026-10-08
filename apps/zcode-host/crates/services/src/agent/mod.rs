//! Agent 子系统：每个 workspace 一个 CLI 进程，经 stdio NDJSON 通信（docs/specs/rust-host.md R2）。
pub mod client;
pub mod command;
pub mod extensions;
mod flow;
pub mod process;
pub mod runtime;
pub mod service;
pub mod session;
pub mod sessions_index;
pub mod wire;

pub use runtime::{
    AgentProcess, AgentRuntime, AgentRuntimeConfig, Frame, FrameKind, RuntimePolicy,
    WorkspaceTarget,
};
