use std::{
    os::fd::OwnedFd,
    sync::Arc,
};

use dashmap::DashMap;
use simple_graphics_protocol::Resource;

/// Server-assigned identity for a connected client.
///
/// Monotonic (allocated by the server, never reused while it runs), unlike
/// `pid_t` which the kernel can recycle — ownership and control-channel
/// lookups keyed by pid become ambiguous once a registry exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(u64);

impl ClientId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }
}

impl std::fmt::Display for ClientId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The server's own fds for each resource. Grants are duplicates of these
/// (`SCM_RIGHTS`); the server never closes them.
pub type ResourceRegistry = Arc<DashMap<Resource, OwnedFd>>;
