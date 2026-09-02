//! Fbdev backend: open `/dev/fb0` and register it as `Resource::Fbdev`.
//!
//! Compiled only with the `fbdev` feature (off by default; the primary
//! display path is DRM). The granted fd is a dup of the server's open —
//! same semantics as input grants.

use std::{fs::File, os::fd::AsRawFd};

use simple_graphics_protocol::Resource;
use tracing::{debug, error, info};

use crate::types::ResourceRegistry;

/// Open `/dev/fb0` and register it as `Resource::Fbdev`.
pub(super) fn open(resource_reg: ResourceRegistry, advertised: &mut Vec<Resource>) {
    match File::options().read(true).write(true).open("/dev/fb0") {
        Ok(file) => {
            let fd = file.as_raw_fd();
            let resource = Resource::Fbdev;
            resource_reg.insert(resource.clone(), file.into());
            advertised.push(resource);
            info!("Opened /dev/fb0");
            debug!("Registered resource Fbdev (fd {fd})");
        }
        Err(e) => {
            error!("Failed to open /dev/fb0: {e}");
        }
    }
}
