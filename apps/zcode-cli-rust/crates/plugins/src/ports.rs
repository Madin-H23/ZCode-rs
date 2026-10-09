//! External effects of plugin management: storage root, working directory,
//! cancellation, HTTP and system Git (spec rust-m10-4-plugin-sources §2).
use std::path::Path;
use tokio_util::sync::CancellationToken;

pub struct Ports<'a> {
    pub storage: &'a Path,
    /// Base of relative local sources (Node `process.cwd()`): the workspace
    /// root of the request, never the process cwd (spec rust-u1-runtime-library L4).
    pub cwd: &'a Path,
    pub cancel: &'a CancellationToken,
    pub http: &'a dyn crate::http::Http,
    pub git: &'a crate::git::Git,
}
