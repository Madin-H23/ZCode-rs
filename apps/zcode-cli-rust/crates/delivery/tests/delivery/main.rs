//! Delivery driven through its public API only, with a test embedder in place
//! of the stdio front-end (spec rust-u3-stage-b §3.2): what a Host that owns
//! its connections relies on.
mod connections;
mod subscriptions;
mod support;
