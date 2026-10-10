// 每个模块对应合并前的一个测试二进制；测试名形如 `suite::end_to_end::…`。
//
// 按 feature `embedded-runtime` 分支的模块在各自文件里用 `#![cfg(…)]` 标注：`agent_cli`、`cli_entry`、
// `restart_settlement` 只在合并形态（缺省）编译，`host_only` 只在“只有 Host”形态
// （`--no-default-features`）编译，所以那一步用 `binary(all) & test(/^suite::host_only::/)` 选它。
mod agent_cli;
mod agent_startup;
mod bots_shutdown;
mod cli_entry;
mod close_reasons;
mod common;
mod control_latency;
mod end_to_end;
mod host_only;
mod restart_settlement;
mod startup_retry;
