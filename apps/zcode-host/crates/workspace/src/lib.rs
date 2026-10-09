//! Rust Host services: `workspace` layer (docs/specs/rust-u2-host-structure.md).
pub mod file;
pub mod file_watcher;
pub mod git;
pub mod media_preview;
pub mod prompt_attachment_transfer;
pub mod system;
pub mod terminal;
pub mod workspace;
use zcode_host_base::HostEnvironment;
use zcode_host_base::main_bridge;
use zcode_host_base::store;
use zcode_host_base::support;
#[cfg(test)]
use zcode_host_base::testing;
use zcode_host_providers::providers;
use zcode_host_sessions::agent;
use zcode_host_settings::settings;
