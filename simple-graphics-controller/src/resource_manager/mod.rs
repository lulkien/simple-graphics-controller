//! Open and register every available resource (video + input). Ownership is
//! NOT tracked here anymore — that is the policy engine's job.
//!
//! Display resources (fbdev, DRM cards) live in the [`display`] submodule,
//! input device discovery (enumeration + classification) in the [`input`]
//! submodule; this module only orchestrates the opens.
//!
//! [`query_resource`] also returns the advertised list in priority order:
//! clients read it top-down, so the first entry of a kind is the best
//! match (the first DRM card is the display card).

mod display;
mod input;

use std::{
    fs::File,
    os::fd::AsRawFd,
    sync::Arc,
};

use dashmap::DashMap;
use simple_graphics_protocol::Resource;
use tracing::{error, info};

use crate::types::ResourceRegistry;

/// Open and register every available resource.
///
/// Returns the registry plus the resources in advertised order (priority
/// order — first is best).
pub fn query_resource() -> (ResourceRegistry, Vec<Resource>) {
    let resource_reg: ResourceRegistry = Arc::new(DashMap::new());
    let mut advertised = Vec::new();

    display::open_fbdev(resource_reg.clone(), &mut advertised);
    display::open_drm_devices(resource_reg.clone(), &mut advertised);
    open_input_devices(resource_reg.clone(), &mut advertised);

    (resource_reg, advertised)
}

/// Open and register every input device the discovery submodule found. Each
/// registry fd is the server's own open; grants dup it (the client parses
/// evdev events straight off the dup — no path needed).
fn open_input_devices(resource_reg: ResourceRegistry, advertised: &mut Vec<Resource>) {
    for device in input::discover() {
        let file = match File::options().read(true).open(&device.path) {
            Ok(file) => file,
            Err(e) => {
                error!("Failed to open {}: {e}", device.path.display());
                continue;
            }
        };

        let resource = Resource::Input(device.resource);
        let fd = file.as_raw_fd();
        resource_reg.insert(resource.clone(), file.into());
        advertised.push(resource.clone());
        info!("Opened {} ({}): {resource:?} (fd {fd})", device.path.display(), device.name);
    }
}
