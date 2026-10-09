//! Host 中唯一认识 runtime 的 crate（docs/specs/rust-host-cli-boundary.md §2、§3.2、§3.5）。
//!
//! - 选择：按 Host 环境决定一个 workspace 的 runtime 怎么到达（[`resolve`]、[`RuntimeCommand`]）。
//! - 链路：[`RuntimeCommand::start`] 建立 [`Link`]，两种传输说同一份 App 协议：
//!   - 子进程：CLI 进程的 stdio，总是可用，不链接任何 CLI 代码（[`process`]）；
//!   - 进程内：runtime 作为库运行在 Host 进程里（feature `embedded`，依赖 `zcode-cli-rust`）。
//! - 结束：[`Exit`] 等待与回收 runtime，[`Outcome`] 是结束原因。
//!
//! 链路之上（实例表、代次、惰性重启、订阅归属）归 Host 的 agent 子系统；链路之下归 runtime。
mod link;
pub mod process;
mod select;

#[cfg(feature = "embedded")]
mod embedded;

pub use link::{Exit, Link, Outcome, Start};
#[cfg(feature = "embedded")]
pub use select::Purpose;
pub use select::{ProcessCommand, RuntimeCommand, resolve};

/// 进程内 runtime 的日志：Host 进程安装 runtime 的 subscriber。
#[cfg(feature = "embedded")]
pub use embedded::init_logging;
