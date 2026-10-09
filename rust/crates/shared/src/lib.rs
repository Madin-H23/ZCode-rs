//! CLI 与 Host 共用的产品级格式与规则（docs/specs/rust-host-cli-boundary.md §3.1）。
//!
//! 只有数据与规则：不做 IO、不持有状态，也不依赖任何应用 crate。
//!
//! - [`workspace_identity`]：workspace 身份与远程 identity 的解析（Node `remote-workspace-identity.ts`）。
//! - [`option_map`]：provider 选项映射的受限 CEL 表达式。
//! - [`hook_trust`]：项目 hook 信任记录与信任存储文件的格式。

pub mod hook_trust;
pub mod option_map;
pub mod workspace_identity;
