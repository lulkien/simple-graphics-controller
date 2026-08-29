use std::{
    fs::{File, read_dir},
    os::fd::AsRawFd,
    sync::Arc,
};

use dashmap::DashMap;
use simple_graphics_protocol::Resource;
use tracing::{debug, error, info};

use crate::types::ResourceRegistry;

/// Open and register every available graphics resource. Ownership is NOT
/// tracked here anymore — that is the policy engine's job.
pub fn query_resource() -> ResourceRegistry {
    let resource_reg: ResourceRegistry = Arc::new(DashMap::new());

    open_fbdev(resource_reg.clone());
    open_drm_devices(resource_reg.clone());

    resource_reg
}

fn open_fbdev(resource_reg: ResourceRegistry) {
    match File::options().read(true).write(true).open("/dev/fb0") {
        Ok(file) => {
            let fd = file.as_raw_fd();
            resource_reg.insert(Resource::Fbdev, file.into());
            info!("Opened /dev/fb0");
            debug!("Registered resource Fbdev (fd {fd})");
        }
        Err(e) => {
            error!("Failed to open /dev/fb0: {e}");
        }
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
