use std::{os::fd::OwnedFd, sync::Arc};

use dashmap::DashMap;
use nix::libc::pid_t;
use simple_graphics_protocol::Resource;

pub type ResourceRegistry = Arc<DashMap<Resource, OwnedFd>>;
pub type OwnerRegistry = Arc<DashMap<Resource, Option<pid_t>>>;
