//! Agent 子系统：每个 workspace 一个 CLI 进程，经 stdio NDJSON 通信（docs/specs/rust-host.md R2）。
pub mod client;
pub mod command;
pub mod process;
pub mod runtime;
pub mod service;

pub use runtime::{
    AgentProcess, AgentRuntime, AgentRuntimeConfig, Frame, FrameKind, RuntimePolicy,
    WorkspaceTarget,
};
