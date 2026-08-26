use std::{
    fs::{File, read_dir},
    sync::Arc,
};

use dashmap::DashMap;
use simple_graphics_protocol::Resource;
use tracing::{error, info};

use crate::types::{OwnerRegistry, ResourceRegistry};

pub fn query_resource() -> (ResourceRegistry, OwnerRegistry) {
    let resource_reg: ResourceRegistry = Arc::new(DashMap::new());
    let owner_reg: OwnerRegistry = Arc::new(DashMap::new());

    open_fbdev(resource_reg.clone(), owner_reg.clone());
    open_drm_devices(resource_reg.clone(), owner_reg.clone());

    (resource_reg, owner_reg)
}

fn open_fbdev(resource_reg: ResourceRegistry, owner_reg: OwnerRegistry) {
    match File::options().read(true).write(true).open("/dev/fb0") {
        Ok(file) => {
            resource_reg.insert(Resource::Fbdev, file.into());
            owner_reg.insert(Resource::Fbdev, None);
            info!("Opened /dev/fb0");
        }
        Err(e) => {
            error!("Failed to open /dev/fb0: {e}");
        }
    }
}

#[allow(unused)]
fn open_drm_devices(resource_reg: ResourceRegistry, owner_reg: OwnerRegistry) {
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
    info!("Found DRM devices: {:?}", paths);
}
