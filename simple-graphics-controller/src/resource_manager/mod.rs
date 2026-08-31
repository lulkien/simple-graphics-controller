//! Open and register every available resource (video + input). Ownership is
//! NOT tracked here anymore — that is the policy engine's job.
//!
//! Input device discovery (enumeration + classification) lives in the
//! [`input`] submodule; this file only opens what it finds.

mod input;

use std::{
    fs::{File, read_dir},
    os::fd::AsRawFd,
    sync::Arc,
};

use dashmap::DashMap;
use simple_graphics_protocol::{DisplayResource, Resource};
use tracing::{debug, error, info};

use crate::types::ResourceRegistry;

/// Open and register every available resource.
pub fn query_resource() -> ResourceRegistry {
    let resource_reg: ResourceRegistry = Arc::new(DashMap::new());

    open_fbdev(resource_reg.clone());
    open_input_devices(resource_reg.clone());
    open_drm_devices(resource_reg.clone());

    resource_reg
}

fn open_fbdev(resource_reg: ResourceRegistry) {
    match File::options().read(true).write(true).open("/dev/fb0") {
        Ok(file) => {
            let fd = file.as_raw_fd();
            resource_reg.insert(Resource::Display(DisplayResource::Fbdev), file.into());
            info!("Opened /dev/fb0");
            debug!("Registered resource Fbdev (fd {fd})");
        }
        Err(e) => {
            error!("Failed to open /dev/fb0: {e}");
        }
    }
}

/// Open and register every input device the discovery submodule found. Each
/// registry fd is the server's own open; grants dup it (the client parses
/// evdev events straight off the dup — no path needed).
fn open_input_devices(resource_reg: ResourceRegistry) {
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
        info!("Opened {} ({}): {resource:?} (fd {fd})", device.path.display(), device.name);
    }
}

#[allow(unused)]
fn open_drm_devices(resource_reg: ResourceRegistry) {
    let entries = match read_dir("/dev/dri/") {
        Ok(entries) => entries,
        Err(e) => {
            error!("Failed to read /dev/dri: {e}");
            return;
        }
    };

    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                return false;
            };

            name.starts_with("card")
        })
        .collect::<Vec<_>>();

    paths.sort();
    debug!(
        "DRM devices present: {:?} (not exposed as resources yet)",
        paths
    );
}
