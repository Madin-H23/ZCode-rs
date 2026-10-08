//! `@zcode/rpc` 的 Rust 实现：值编码、消息头与 `SocketProtocol` 分帧。
//!
//! 逐字节对齐 `packages/rpc/src/serialization.ts`、`channelServer.ts` 与 `protocol.ts`；
//! 夹具由 `tests/fixtures/gen.ts` 用真实 TS 实现生成。
pub mod client;
pub mod codec;
pub mod frame;

pub use client::{RpcClient, RpcEvents};
pub use codec::{Reader, Request, RequestKind, RpcError, RpcValue, read_request};
pub use frame::{FrameReader, frame};
