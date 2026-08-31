//! Display resources: fbdev (`/dev/fb0`) and DRM cards (`/dev/dri/cardN`).
//!
//! Registration order IS the advertised priority order (first is best): the
//! DRM card selection logic lives here with the open.

use std::{
    fs::{File, read_dir, read_to_string},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

use simple_graphics_protocol::{DisplayResource, Resource};
use tracing::{debug, error, info};

use crate::types::ResourceRegistry;

/// Open `/dev/fb0` and register it as `Display(Fbdev)`.
pub(super) fn open_fbdev(resource_reg: ResourceRegistry, advertised: &mut Vec<Resource>) {
    match File::options().read(true).write(true).open("/dev/fb0") {
        Ok(file) => {
            let fd = file.as_raw_fd();
            let resource = Resource::Display(DisplayResource::Fbdev);
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

/// Open and register every DRM card that can present a display, as
/// `Display(Drm { card })` — each physical `/dev/dri/cardN` becomes one
/// resource, registered in priority order (first is best). Render nodes
/// (`renderDNN`) are skipped — they cannot modeset.
///
/// The daemon opens the card as root, so its open file becomes DRM master;
/// granted dups share that open file description and carry master with them,
/// letting clients modeset directly. The daemon keeps its fd for the
/// registry's lifetime and grants dups, like every other resource.
pub(super) fn open_drm_devices(resource_reg: ResourceRegistry, advertised: &mut Vec<Resource>) {
    let entries = match read_dir("/dev/dri/") {
        Ok(entries) => entries,
        Err(e) => {
            error!("Failed to read /dev/dri: {e}");
            return;
        }
    };

    let mut cards = entries
        .filter_map(Result::ok)
        .filter_map(|entry| card_index(&entry.path()))
        .collect::<Vec<u8>>();
    cards.sort_unstable();

    for card in drm_display_cards(&cards) {
        let path = PathBuf::from(format!("/dev/dri/card{card}"));

        let file = match File::options().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(e) => {
                error!("Failed to open {}: {e}", path.display());
                continue;
            }
        };

        let resource = Resource::Display(DisplayResource::Drm { card });
        let fd = file.as_raw_fd();
        resource_reg.insert(resource.clone(), file.into());
        advertised.push(resource.clone());
        info!("Opened {} ({resource:?}) (fd {fd})", path.display());
    }
}

/// Display-capable cards in priority order: a card with a connected
/// connector first, then the lowest index. A card is display-capable if it
/// has at least one display connector (writeback connectors are
/// capture-only and don't count).
fn drm_display_cards(cards: &[u8]) -> Vec<u8> {
    let mut candidates = cards
        .iter()
        .copied()
        .map(|card| (card, drm_connectors(card)))
        .filter(|(_, connectors)| !connectors.is_empty())
        .collect::<Vec<_>>();

    candidates.sort_by(|a, b| {
        is_any_connected(&b.1)
            .cmp(&is_any_connected(&a.1))
            .then(a.0.cmp(&b.0))
    });

    candidates.into_iter().map(|(card, _)| card).collect()
}

/// The connector sysfs dirs of a card (`/sys/class/drm/cardN-*`), minus
/// writeback connectors (capture-only; cannot present a display).
fn drm_connectors(card: u8) -> Vec<PathBuf> {
    let prefix = format!("card{card}-");
    let Ok(entries) = read_dir("/sys/class/drm/") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                return false;
            };
            name.starts_with(&prefix) && !name.contains("Writeback")
        })
        .collect()
}

/// Does any of the card's connectors report a connected display?
fn is_any_connected(connectors: &[PathBuf]) -> bool {
    connectors.iter().any(|connector| {
        read_to_string(connector.join("status")).is_ok_and(|status| status.trim() == "connected")
    })
}

/// Parse the card index out of a `/dev/dri/cardN` path (`card0` -> `0`).
fn card_index(path: &Path) -> Option<u8> {
    let name = path.file_name()?.to_str()?;
    name.strip_prefix("card")?.parse().ok()
}
