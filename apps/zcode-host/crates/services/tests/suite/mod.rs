// 每个模块对应合并前的一个测试二进制；测试名形如 `suite::tasks_cli::…`。
mod account_cli;
mod account_interop;
mod bots_cli;
mod common;
// 只在 Unix 上编译（文件内 `#![cfg(unix)]`）。
mod extensions_interop;
mod off_peak_account;
mod provider_interop;
mod remote_bots;
mod remote_server;
mod scheduler_install;
mod setting_control;
mod settings_parity;
mod task_queue_cli;
mod tasks_cli;
