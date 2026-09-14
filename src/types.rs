use std::{os::fd::OwnedFd, sync::Arc};

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

/// The server's own fds for `Fbdev` and `Input` resources. Grants are
/// duplicates of these (`SCM_RIGHTS`); the server never closes them. DRM
/// cards are NOT here — their grants are fresh lease fds created per grant
/// by the DRM registry (`crate::resource_manager::DrmRegistry`).
pub type ResourceRegistry = Arc<DashMap<Resource, OwnedFd>>;

/// The resources the server offers, in advertised (priority) order — first is
/// best, which is how clients read the list. Not fixed at startup: the input
/// reconciler adds and drops entries as devices come and go, so a client that
/// connects later is told what the server has NOW rather than what it had at
/// boot.
#[derive(Debug)]
pub struct AdvertisedResources(std::sync::RwLock<Vec<Resource>>);

impl AdvertisedResources {
    pub fn new(resources: Vec<Resource>) -> Self {
        Self(std::sync::RwLock::new(resources))
    }

    /// The current list, in advertised order.
    pub fn snapshot(&self) -> Vec<Resource> {
        self.0
            .read()
            .expect("advertised resources lock poisoned")
            .clone()
    }

    /// Offer `resource` (appended: for its kind, the earlier entries stay the
    /// better match).
    pub fn insert(&self, resource: Resource) {
        let mut list = self.0.write().expect("advertised resources lock poisoned");
        if !list.contains(&resource) {
            list.push(resource);
        }
    }

    /// Stop offering `resource`.
    pub fn remove(&self, resource: &Resource) {
        let mut list = self.0.write().expect("advertised resources lock poisoned");
        list.retain(|entry| entry != resource);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use simple_graphics_protocol::InputResource;

    #[test]
    fn advertised_list_tracks_insert_and_remove() {
        let keyboard = Resource::Input(InputResource::Keyboard(0));
        let advertised = AdvertisedResources::new(vec![Resource::Fbdev]);
        assert_eq!(advertised.snapshot(), vec![Resource::Fbdev]);

        // An adopted device is appended (earlier entries keep priority) and
        // never duplicated.
        advertised.insert(keyboard.clone());
        advertised.insert(keyboard.clone());
        assert_eq!(
            advertised.snapshot(),
            vec![Resource::Fbdev, keyboard.clone()]
        );

        advertised.remove(&keyboard);
        assert_eq!(advertised.snapshot(), vec![Resource::Fbdev]);
    }
}
