//! App Server frontend: stdio framing, JSON-RPC routing and host bridging
//! over the runtime transport contract; V4 topic delivery is
//! `zcode-cli-delivery`. It owns no session facts.
use zcode_cli_core_api as contract;
use zcode_cli_domain as domain;
mod outlet;
mod routes;
mod server;
mod sink;
pub mod stdio;
pub use server::serve;
pub use sink::Sink;
pub use stdio::{finish, finish_task, start, start_reader, storage_prepare};
