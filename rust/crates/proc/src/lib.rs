//! 子进程的唯一入口（docs/specs/rust-u0-foundation.md §3.4、§3.5）。
//!
//! 两个应用 workspace 中创建子进程只能经过 [`command`] 与 [`std_command`]：仓库根 `clippy.toml`
//! 的 `disallowed-methods` 禁止在其他位置调用 `Command::new`。本期它们不改变任何默认行为；
//! U1 起在这里注入运行时环境。
//!
//! - [`shell`]：跑一段 shell 文本（Unix `/bin/bash -c`，Windows `cmd.exe /d /s /c`）。
//! - [`group`]：让子进程自成新会话与进程组，脱离控制终端。
//! - [`ledger`]：runtime 的工具进程树（Bash、hooks、MCP stdio）的唯一启动入口：自成会话或进程组，
//!   Unix 上写账本记录，所在进程被强杀后由下一个 runtime 实例清扫；Windows 上放进随本进程结束的 Job。
//! - [`tree`]：Unix 上按身份追踪并结束整棵进程树。
//! - `job`（Windows）：Job Object，整棵树随 Job 一起结束。

use std::ffi::OsStr;

pub mod group;
#[cfg(windows)]
pub mod job;
pub mod ledger;
pub mod shell;
pub mod tree;

/// 创建 `tokio` 子进程命令。与 `tokio::process::Command::new` 完全相同，不改变任何默认行为
/// （不改环境、不设 `kill_on_drop`）。
#[expect(
    clippy::disallowed_methods,
    reason = "zcode-proc 是唯一允许直接创建 Command 的位置（spec §3.5）"
)]
pub fn command(program: impl AsRef<OsStr>) -> tokio::process::Command {
    tokio::process::Command::new(program)
}

/// 创建同步子进程命令，用于不在 tokio 运行时中的代码。与 `std::process::Command::new` 完全相同。
#[expect(
    clippy::disallowed_methods,
    reason = "zcode-proc 是唯一允许直接创建 Command 的位置（spec §3.5）"
)]
pub fn std_command(program: impl AsRef<OsStr>) -> std::process::Command {
    std::process::Command::new(program)
}
