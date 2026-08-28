use std::{
    collections::HashMap,
    os::fd::OwnedFd,
    sync::{Arc, Mutex, MutexGuard},
};

use dashmap::DashMap;
use nix::libc::pid_t;
use simple_graphics_protocol::Resource;

pub type ResourceRegistry = Arc<DashMap<Resource, OwnedFd>>;
pub type OwnerRegistry = Arc<Mutex<HashMap<Resource, Option<pid_t>>>>;

/// Lock the owner registry, recovering from a poisoned mutex so one panicked
/// task cannot permanently break resource ownership.
pub fn lock_owners(
    reg: &OwnerRegistry,
) -> MutexGuard<'_, HashMap<Resource, Option<pid_t>>> {
    reg.lock().unwrap_or_else(|e| e.into_inner())
}
