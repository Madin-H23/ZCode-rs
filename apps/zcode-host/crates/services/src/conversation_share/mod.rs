//! conversation-share：docs/specs/rust-host-automation.md。
use std::sync::Arc;

use zcode_host_control::ControlRouter;
use zcode_host_kit::ChannelRegistry;

use crate::automation::{Area, Keepalive};

pub fn install(
    _area: &Arc<Area>,
    _registry: &mut ChannelRegistry,
    _router: &mut ControlRouter,
) -> Option<Keepalive> {
    None
}
