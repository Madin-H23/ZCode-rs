//! Rust Host services: `automation` layer (docs/specs/rust-u2-host-structure.md).
pub mod automation;
pub mod bots;
pub mod conversation_share;
pub mod cua_permission;
pub mod integration;
pub mod off_peak;
use zcode_host_account::account;
#[cfg(test)]
use zcode_host_base::HostEnvironment;
use zcode_host_base::ServiceContext;
use zcode_host_base::main_bridge;
use zcode_host_base::store;
use zcode_host_base::support;
use zcode_host_providers::providers;
use zcode_host_sessions::agent;
use zcode_host_sessions::tasks;
use zcode_host_sessions::tasks_index;
use zcode_host_settings::settings;

#[cfg(test)]
mod reverse_zod_tests;
