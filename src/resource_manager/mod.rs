//! Open and register every available resource (video + input). Ownership is
//! NOT tracked here anymore — that is the policy engine's job.
//!
//! Backends are compile-time features (see docs/resource-manager.md):
//! `fbdev`, `drm`, and `input` each live in their own module behind a
//! `#[cfg(feature = ...)]`. Default features: `drm` + `input` — an unbuilt
//! backend is never opened, registered, or advertised; the engine denies
//! Acquires against it with "not registered". The protocol crate is
//! deliberately ungated (the wire format must not depend on the build).
//!
//! [`query_resource`] returns the advertised list in priority order:
//! clients read it top-down, so the first entry of a kind is the best
//! match (the first DRM card is the display card).

#[cfg(feature = "drm")]
mod drm;
#[cfg(feature = "fbdev")]
mod fbdev;
#[cfg(feature = "input")]
mod input;

use std::sync::Arc;

use dashmap::DashMap;
use simple_graphics_protocol::Resource;

use crate::types::ResourceRegistry;

#[cfg(feature = "drm")]
pub use drm::DrmRegistry;

/// The server's grant sources, cloned per client connection.
#[derive(Clone)]
pub struct ResourceRegistries {
    /// Static fds for `Fbdev` and `Input` (grants are dups of these).
    pub fds: ResourceRegistry,
    /// DRM lease factories: each grant creates a fresh lease fd.
    #[cfg(feature = "drm")]
    pub drm: DrmRegistry,
}

/// Everything the server opened at startup.
pub struct OpenedResources {
    /// The registries the server grants from.
    pub registries: ResourceRegistries,
    /// Resources in advertised order (priority order — first is best).
    pub advertised: Vec<Resource>,
}

/// Open and register every available resource.
///
/// Returns the registries plus the resources in advertised order (priority
/// order — first is best). Backends that are not compiled in contribute
/// nothing.
pub fn query_resource() -> OpenedResources {
    let resource_reg: ResourceRegistry = Arc::new(DashMap::new());
    // With no backend features the list is never pushed to; the mut keeps
    // the body identical across all feature combinations.
    #[allow(unused_mut)]
    let mut advertised = Vec::new();

    #[cfg(feature = "fbdev")]
    fbdev::open(resource_reg.clone(), &mut advertised);

    #[cfg(feature = "drm")]
    let drm_registry: DrmRegistry = Arc::new(DashMap::new());

    #[cfg(feature = "drm")]
    drm::open_devices(drm_registry.clone(), &mut advertised);

    #[cfg(feature = "input")]
    input::open_devices(resource_reg.clone(), &mut advertised);

    OpenedResources {
        registries: ResourceRegistries {
            fds: resource_reg,
            #[cfg(feature = "drm")]
            drm: drm_registry,
        },
        advertised,
    }
}
