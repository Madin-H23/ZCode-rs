//! Rust Host 的 service 框架：channel 抽象、契约 stub、参数解码、事件分发与日志。
//!
//! 各 service 用 [`ServiceBuilder`] 按方法注册实现；没实现的成员回退到契约 stub
//! （立即回 `NotImplemented`），保证 Renderer 的任何调用都不会挂起。
#![deny(clippy::unwrap_used, clippy::expect_used)]
pub mod args;
pub mod builder;
pub mod channel;
pub mod contract;
pub mod events;
pub mod log;

pub use args::Args;
pub use builder::ServiceBuilder;
pub use channel::{
    CallContext, CallFuture, ChannelRegistry, EventStream, FlowState, NOT_IMPLEMENTED_CODE, Reply,
    ServiceChannel, StubChannel,
};
pub use events::{EventHub, replies};
pub use zcode_host_rpc::codec::base64_encode;
pub use zcode_host_rpc::{RpcError, RpcValue};
