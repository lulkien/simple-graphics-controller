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
//! [`open_resources`] returns the advertised list in priority order:
//! clients read it top-down, so the first entry of a kind is the best
//! match (the first DRM card is the display card).

#[cfg(feature = "drm")]
mod drm;
#[cfg(feature = "fbdev")]
mod fbdev;
#[cfg(feature = "input")]
pub mod hotplug;
#[cfg(feature = "input")]
mod input;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;
use simple_graphics_protocol::Resource;

use crate::types::{AdvertisedResources, ResourceRegistry};

#[cfg(feature = "drm")]
pub use drm::DrmRegistry;
#[cfg(feature = "input")]
pub use input::InputIndex;

/// The server's grant sources, cloned per client connection.
#[derive(Clone)]
pub struct ResourceRegistries {
    /// Static fds for `Fbdev` and `Input` (grants are dups of these).
    pub fds: ResourceRegistry,
    /// DRM lease factories: each grant creates a fresh lease fd.
    #[cfg(feature = "drm")]
    pub drm: DrmRegistry,
}

/// Everything the daemon holds and offers: the registries it grants from,
/// the list it advertises, and the input devices its reconciler tracks.
pub struct Inventory {
    /// The registries the server grants from.
    pub registries: ResourceRegistries,
    /// Resources in advertised order (priority order — first is best).
    pub advertised: Vec<Resource>,
    /// The input devices the server holds, so the hot-plug reconciler can tell
    /// the node it opened apart from one re-created under the same path.
    #[cfg(feature = "input")]
    pub input_index: InputIndex,
}

/// Open and register every available resource.
///
/// Returns the registries plus the resources in advertised order (priority
/// order — first is best). Backends that are not compiled in contribute
/// nothing.
pub fn open_resources() -> Inventory {
    let resource_reg: ResourceRegistry = Arc::new(DashMap::new());
    // With no backend features the list is never pushed to; the mut keeps
    // the body identical across all feature combinations.
    #[allow(unused_mut)]
    let mut advertised = Vec::new();

    #[cfg(feature = "input")]
    let input_index = input::new_index();

    #[cfg(feature = "fbdev")]
    fbdev::open(resource_reg.clone(), &mut advertised);

    #[cfg(feature = "drm")]
    let drm_registry: DrmRegistry = Arc::new(DashMap::new());

    #[cfg(feature = "drm")]
    drm::open_devices(drm_registry.clone(), &mut advertised);

    #[cfg(feature = "input")]
    input::open_devices(resource_reg.clone(), &mut advertised, &input_index);

    Inventory {
        registries: ResourceRegistries {
            fds: resource_reg,
            #[cfg(feature = "drm")]
            drm: drm_registry,
        },
        advertised,
        #[cfg(feature = "input")]
        input_index,
    }
}

/// Check the invariants that tie the three structures together: one line per
/// violation, empty when the state is coherent.
///
/// These are the rules the module maintains by hand today, and the reason the
/// transitions (suspend / resume / adopt) belong behind methods: a violation
/// means some path updated one structure without the others.
///
/// 1. one index entry per resource, keyed by the devnode it sits on;
/// 2. a LIVE entry (not suspended) is advertised, and its resource has an fd to
///    grant;
/// 3. a SUSPENDED entry is neither advertised nor grantable — the holder keeps
///    the name, the daemon keeps no way to hand it out;
/// 4. every advertised input has an index entry, and every input fd belongs to
///    an advertised resource (never a grant source without a device behind it).
#[cfg(feature = "input")]
pub fn check_consistency(
    fds: &ResourceRegistry,
    index: &InputIndex,
    advertised: &AdvertisedResources,
) -> Vec<String> {
    let list = advertised.snapshot();
    let mut problems = Vec::new();
    let mut seen: HashMap<Resource, PathBuf> = HashMap::new();

    for entry in index.iter() {
        let path = entry.key().clone();
        let held = entry.value();

        if let Some(previous) = seen.insert(held.resource.clone(), path.clone()) {
            problems.push(format!(
                "{:?} is in the index twice: {} and {}",
                held.resource,
                previous.display(),
                path.display()
            ));
        }

        let has_fd = fds.contains_key(&held.resource);
        let advertised = list.contains(&held.resource);
        if held.suspended {
            if has_fd {
                problems.push(format!(
                    "{:?} is suspended but still has an fd",
                    held.resource
                ));
            }
            if advertised {
                problems.push(format!(
                    "{:?} is suspended but still advertised",
                    held.resource
                ));
            }
        } else {
            if !has_fd {
                problems.push(format!(
                    "{:?} ({}) is live but has no fd",
                    held.resource,
                    path.display()
                ));
            }
            if !advertised {
                problems.push(format!(
                    "{:?} ({}) is live but not advertised",
                    held.resource,
                    path.display()
                ));
            }
        }
    }

    for resource in &list {
        if matches!(resource, Resource::Input(_)) && !seen.contains_key(resource) {
            problems.push(format!("{resource:?} is advertised with no index entry"));
        }
    }

    for entry in fds.iter() {
        if matches!(entry.key(), Resource::Input(_)) && !list.contains(entry.key()) {
            problems.push(format!("{:?} has an fd but is not advertised", entry.key()));
        }
    }

    problems
}

#[cfg(all(test, feature = "input"))]
mod tests {
    use super::*;
    use crate::resource_manager::input::{HeldInput, new_index};
    use simple_graphics_protocol::InputResource;

    fn fd() -> std::os::fd::OwnedFd {
        std::fs::File::open("/dev/null").expect("/dev/null").into()
    }

    fn entry(resource: &Resource, suspended: bool) -> HeldInput {
        HeldInput {
            resource: resource.clone(),
            dev: 1,
            ino: 2,
            device: None,
            suspended,
        }
    }

    fn keyboard() -> Resource {
        Resource::Input(InputResource::Keyboard(0))
    }

    fn mouse() -> Resource {
        Resource::Input(InputResource::Mouse(0))
    }

    /// Startup plus one device that went away while somebody held it: the
    /// keyboard is live and advertised, the mouse is suspended and neither.
    fn coherent() -> (ResourceRegistry, InputIndex, AdvertisedResources) {
        let fds: ResourceRegistry = Arc::new(DashMap::new());
        fds.insert(Resource::Fbdev, fd());
        fds.insert(keyboard(), fd());

        let index = new_index();
        index.insert("/dev/input/event0".into(), entry(&keyboard(), false));
        index.insert("/dev/input/event6".into(), entry(&mouse(), true));

        let advertised = AdvertisedResources::new(vec![Resource::Fbdev, keyboard()]);
        (fds, index, advertised)
    }

    #[test]
    fn a_coherent_state_reports_nothing() {
        let (fds, index, advertised) = coherent();
        assert_eq!(
            check_consistency(&fds, &index, &advertised),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_live_input_without_a_grant_source_is_reported() {
        let (fds, index, advertised) = coherent();
        fds.remove(&keyboard());
        let problems = check_consistency(&fds, &index, &advertised);
        assert!(
            problems.iter().any(|p| p.contains("live but has no fd")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_suspended_input_that_is_still_offered_is_reported() {
        let (fds, index, advertised) = coherent();
        advertised.insert(mouse());
        fds.insert(mouse(), fd());
        let problems = check_consistency(&fds, &index, &advertised);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("suspended but still advertised")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("suspended but still has an fd")),
            "{problems:?}"
        );
    }
}
