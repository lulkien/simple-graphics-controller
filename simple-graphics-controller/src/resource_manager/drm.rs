//! DRM backend: open display-capable cards (`/dev/dri/cardN`) as DRM master
//! and register each as `Resource::Drm { card }`.
//!
//! Compiled only with the `drm` feature (default). The master fd never
//! leaves the server: every grant creates a FRESH lease over the card's
//! objects (see [`DrmDevice::grant_lease`]) and revocation is
//! kernel-enforced. Registration order IS the advertised priority order
//! (first is best): the card selection logic lives here with the open.

use std::{
    fs::{File, read_dir},
    io,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd},
        unix::fs::OpenOptionsExt,
    },
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use dashmap::DashMap;
use drm::Device;
use drm::control::{
    Device as ControlDevice, LeaseId, RawResourceHandle, ResourceHandles, connector, crtc, plane,
};
use nix::fcntl::OFlag;
use simple_graphics_protocol::Resource;
use tracing::{debug, error, info};

/// The server's DRM master handle for one card: a plain fd wrapper that
/// implements the `drm` crate's `Device` traits (the crate deliberately
/// does not open device nodes for you).
///
/// The master is what creates leases. It is held by the server for the
/// whole run: closing it would destroy every lease it created.
#[derive(Debug)]
pub struct DrmCard(File);

impl AsFd for DrmCard {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl AsRawFd for DrmCard {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl drm::Device for DrmCard {}
impl drm::control::Device for DrmCard {}

impl DrmCard {
    /// Open a DRM card node the way a display server should: `O_RDWR` for
    /// modesetting, `O_CLOEXEC` so the fd never leaks into children, and
    /// `O_NONBLOCK` so event reads on the fd can never block.
    fn open(path: &Path) -> io::Result<Self> {
        let file = File::options()
            .read(true)
            .write(true)
            .custom_flags((OFlag::O_CLOEXEC | OFlag::O_NONBLOCK).bits())
            .open(path)?;
        Ok(Self(file))
    }
}

/// One opened card, probed and ready to be registered.
struct OpenedCard {
    index: u8,
    card: DrmCard,
    crtcs: Vec<crtc::Handle>,
    connectors: Vec<connector::Handle>,
    planes: Vec<plane::Handle>,
    any_connected: bool,
}

/// The server's DRM cards, keyed by their resource. The master fd inside
/// each device creates leases on demand; clients never see it.
pub type DrmRegistry = Arc<DashMap<Resource, DrmDevice>>;

/// A display-capable card the server owns as DRM master.
///
/// Clients never get the master fd. Each grant creates a fresh lease over
/// the card's objects and hands the client the lease fd; the lease is
/// revoked (kernel-enforced) when the resource is released or revoked, so
/// the server can reclaim the card at any time regardless of client
/// cooperation.
#[derive(Debug)]
pub struct DrmDevice {
    index: u8,
    card: DrmCard,
    crtcs: Vec<crtc::Handle>,
    connectors: Vec<connector::Handle>,
    planes: Vec<plane::Handle>,
    /// The lease currently granted on this card, if any. At most one client
    /// owns the resource at a time (the policy engine guarantees it), so a
    /// single slot per card is enough.
    active_lease: Mutex<Option<LeaseId>>,
}

impl DrmDevice {
    /// Create a fresh lease for a grant and return its fd. Any still-active
    /// lease is revoked first (a previous owner may have died without
    /// releasing): the kernel cannot lease the same objects twice.
    pub fn grant_lease(&self) -> io::Result<OwnedFd> {
        let mut active = self.active_lease.lock().unwrap();
        if let Some(prev) = active.take() {
            match self.card.revoke_lease(prev) {
                Ok(()) => debug!("Revoked stale lease {prev} on card{}", self.index),
                Err(e) => debug!("Stale lease {prev} on card{} already gone: {e}", self.index),
            }
        }

        let objects: Vec<RawResourceHandle> = self
            .crtcs
            .iter()
            .copied()
            .map(RawResourceHandle::from)
            .chain(self.connectors.iter().copied().map(RawResourceHandle::from))
            .chain(self.planes.iter().copied().map(RawResourceHandle::from))
            .collect();

        let (lease_id, fd) = self.card.create_lease(&objects, 0)?;
        info!(
            "Created lease {lease_id} on card{} ({} objects: {} crtcs, {} connectors, {} planes)",
            self.index,
            objects.len(),
            self.crtcs.len(),
            self.connectors.len(),
            self.planes.len()
        );
        *active = Some(lease_id);
        Ok(fd)
    }

    /// Revoke the granted lease right now, without waiting for the client
    /// to close its fd. A no-op when no lease is active.
    pub fn revoke_lease(&self) {
        let mut active = self.active_lease.lock().unwrap();
        if let Some(lease_id) = active.take() {
            match self.card.revoke_lease(lease_id) {
                Ok(()) => info!("Revoked lease {lease_id} on card{}", self.index),
                Err(e) => error!(
                    "Failed to revoke lease {lease_id} on card{}: {e}",
                    self.index
                ),
            }
        }
    }
}

/// Open every DRM card that can present a display and register it as
/// `Resource::Drm { card }` — each physical `/dev/dri/cardN` becomes one
/// resource, registered in priority order (first is best). Render nodes
/// (`renderDNN`) are skipped — they cannot modeset.
///
/// The server opens each card as DRM master and KEEPS the master fd for
/// the server's lifetime. It creates no lease at startup: a fresh lease is
/// created per grant (see [`DrmDevice::grant_lease`]) and revoked when the
/// client releases the resource, so the server can reclaim the card at any
/// time — enforced by the kernel, no client cooperation needed.
pub(super) fn open_devices(drm_reg: DrmRegistry, advertised: &mut Vec<Resource>) {
    let entries = match read_dir("/dev/dri/") {
        Ok(entries) => entries,
        Err(e) => {
            error!("Failed to read /dev/dri: {e}");
            return;
        }
    };

    let mut indices = entries
        .filter_map(Result::ok)
        .filter_map(|entry| card_index(&entry.path()))
        .collect::<Vec<u8>>();
    indices.sort_unstable();

    // Open and probe every card node (master + object handles), then order
    // the display-capable ones by priority. Probing needs the open fd, so
    // the ordering happens after all opens, not while listing.
    let mut opened = Vec::new();
    for index in indices {
        let path = PathBuf::from(format!("/dev/dri/card{index}"));

        let card = match DrmCard::open(&path) {
            Ok(card) => card,
            Err(e) => {
                error!("Failed to open {}: {e}", path.display());
                continue;
            }
        };

        // The kernel hands master to the first O_RDWR opener, but only an
        // explicit acquire fails loudly when another process (e.g. a real
        // display server) already holds it — do not fight it, skip.
        if let Err(e) = card.acquire_master_lock() {
            error!("Failed to acquire DRM master on {}: {e}", path.display());
            continue;
        }

        let handles = match card.resource_handles() {
            Ok(handles) => handles,
            Err(e) => {
                error!("Failed to query resources on {}: {e}", path.display());
                continue;
            }
        };
        let planes = match card.plane_handles() {
            Ok(planes) => planes,
            Err(e) => {
                error!("Failed to query planes on {}: {e}", path.display());
                continue;
            }
        };

        let connectors = display_connectors(&card, &handles);
        let any_connected = connectors.iter().any(|handle| {
            card.get_connector(*handle, false)
                .is_ok_and(|info| info.state() == connector::State::Connected)
        });

        opened.push(OpenedCard {
            index,
            card,
            crtcs: handles.crtcs,
            connectors,
            planes,
            any_connected,
        });
    }

    // Priority order: a card with a connected display first, then the
    // lowest index. Cards without a display connector are not
    // display-capable and drop out below.
    opened.sort_by(|a, b| {
        b.any_connected
            .cmp(&a.any_connected)
            .then(a.index.cmp(&b.index))
    });

    for opened in opened {
        let path = format!("/dev/dri/card{}", opened.index);
        if opened.connectors.is_empty() {
            debug!("Skipping {path}: no display connectors");
            continue;
        }

        let resource = Resource::Drm { card: opened.index };
        let fd = opened.card.as_raw_fd();
        info!("Opened {path} ({resource:?}) (master fd {fd})");
        drm_reg.insert(
            resource.clone(),
            DrmDevice {
                index: opened.index,
                card: opened.card,
                crtcs: opened.crtcs,
                connectors: opened.connectors,
                planes: opened.planes,
                active_lease: Mutex::new(None),
            },
        );
        advertised.push(resource);
    }
}

/// The card's display connectors (writeback connectors are capture-only
/// and cannot present a display). A connector whose probe failed is kept
/// rather than silently dropped.
fn display_connectors(card: &DrmCard, handles: &ResourceHandles) -> Vec<connector::Handle> {
    handles
        .connectors
        .iter()
        .copied()
        .filter(|handle| match card.get_connector(*handle, false) {
            Ok(info) => info.interface() != connector::Interface::Writeback,
            Err(_) => true,
        })
        .collect()
}

/// Parse the card index out of a `/dev/dri/cardN` path (`card0` -> `0`).
fn card_index(path: &Path) -> Option<u8> {
    let name = path.file_name()?.to_str()?;
    name.strip_prefix("card")?.parse().ok()
}
