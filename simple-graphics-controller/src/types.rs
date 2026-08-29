use std::{
    collections::HashMap,
    os::fd::OwnedFd,
    sync::{Arc, Mutex, MutexGuard},
};

use dashmap::DashMap;
use nix::libc::pid_t;
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

pub type ResourceRegistry = Arc<DashMap<Resource, OwnedFd>>;
pub type OwnerRegistry = Arc<Mutex<HashMap<Resource, Option<pid_t>>>>;

/// Lock the owner registry, recovering from a poisoned mutex so one panicked
/// task cannot permanently break resource ownership.
pub fn lock_owners(
    reg: &OwnerRegistry,
) -> MutexGuard<'_, HashMap<Resource, Option<pid_t>>> {
    reg.lock().unwrap_or_else(|e| e.into_inner())
}
