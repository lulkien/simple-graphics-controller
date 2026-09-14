//! Input device discovery and registration: enumerate `/dev/input/event*`,
//! classify each device, and register the classifiable ones as
//! `Resource::Input(_)`. Compiled only with the `input` feature (default).

use std::{
    fs::File,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::fs::MetadataExt,
    },
    path::{Path, PathBuf},
    sync::Arc,
};

use dashmap::DashMap;
use evdev::{AbsoluteAxisType, Device as EvdevDevice, Key, RelativeAxisType};
use simple_graphics_protocol::{InputResource, Resource};
use tracing::{debug, error, info};

use crate::types::ResourceRegistry;

/// One discovered input device, ready to be opened and registered.
pub struct DiscoveredDevice {
    pub path: PathBuf,
    pub name: String,
    pub resource: InputResource,
}

/// A classifiable device found under `/dev/input`, before any per-class index
/// is assigned. [`discover`] adds the indices; the hot-plug reconciler assigns
/// them itself so it can reuse the index of a device that went away.
pub struct ProbedDevice {
    pub path: PathBuf,
    pub name: String,
    pub(crate) class: InputClass,
}

/// The class of an input device, before its per-class index is assigned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputClass {
    Mouse,
    Keyboard,
    Touch,
}

impl InputClass {
    /// The resource this class maps to at `index`.
    pub(crate) fn resource(self, index: u8) -> InputResource {
        match self {
            InputClass::Mouse => InputResource::Mouse(index),
            InputClass::Keyboard => InputResource::Keyboard(index),
            InputClass::Touch => InputResource::Touch(index),
        }
    }

    /// The class of `resource`, for index bookkeeping.
    pub(crate) fn of(resource: &InputResource) -> Self {
        match resource {
            InputResource::Mouse(_) => InputClass::Mouse,
            InputResource::Keyboard(_) => InputClass::Keyboard,
            InputResource::Touch(_) => InputClass::Touch,
        }
    }
}

/// One input device the server holds: which resource it is, the identity of the
/// inode it opened, and the sysfs device behind the node.
///
/// The inode tells a RE-CREATED node from the one already held (same path,
/// different inode). The DEVICE tells a re-created node of the same device from
/// a different device: a udev trigger re-creates `eventN` for a device that
/// never moved, and revoking a holder in that case would take working input away
/// for nothing. See [`super::hotplug`].
#[derive(Clone, Debug)]
pub struct HeldInput {
    pub resource: Resource,
    pub dev: u64,
    pub ino: u64,
    /// `/sys/class/input/eventN/device` resolved — `None` when sysfs has no
    /// answer (then a re-created node is treated as a different device).
    pub device: Option<PathBuf>,
}

/// The held input devices, keyed by devnode path.
pub type InputIndex = Arc<DashMap<PathBuf, HeldInput>>;

/// A fresh, empty input index.
pub fn new_index() -> InputIndex {
    Arc::new(DashMap::new())
}

/// Open and register every input device the discovery [`discover`] found.
/// Each registry fd is the server's own open; grants dup it (the client
/// parses evdev events straight off the dup — no path needed).
pub(super) fn open_devices(
    resource_reg: ResourceRegistry,
    advertised: &mut Vec<Resource>,
    index: &InputIndex,
) {
    for device in discover() {
        let Some((fd, dev, ino)) = open_device(&device.path) else {
            continue;
        };

        let resource = Resource::Input(device.resource);
        let raw = fd.as_raw_fd();
        resource_reg.insert(resource.clone(), fd);
        advertised.push(resource.clone());
        index.insert(
            device.path.clone(),
            HeldInput {
                resource: resource.clone(),
                dev,
                ino,
                device: device_identity(&device.path),
            },
        );
        info!(
            "Opened {} ({}): {resource:?} (fd {raw})",
            device.path.display(),
            device.name
        );
    }
}

/// Open one device node, and check that the fd we got still IS that node: a
/// device re-created between the probe and the open (udev doing its slow thing)
/// would otherwise leave the server holding a device that no longer exists.
/// Returns the fd plus the (dev, ino) identity of the inode behind it.
pub fn open_device(path: &Path) -> Option<(OwnedFd, u64, u64)> {
    let file = match File::options().read(true).open(path) {
        Ok(file) => file,
        Err(e) => {
            error!("Failed to open {}: {e}", path.display());
            return None;
        }
    };
    let fd: OwnedFd = file.into();
    let Ok(stat) = nix::sys::stat::fstat(fd.as_raw_fd()) else {
        error!("Failed to stat the fd for {}", path.display());
        return None;
    };
    let identity = stat_identity(stat.st_dev, stat.st_ino);
    match std::fs::metadata(path) {
        Ok(meta) if (meta.dev(), meta.ino()) == identity => Some((fd, identity.0, identity.1)),
        Ok(_) => {
            error!(
                "{} was replaced while opening it; leaving it for the next pass",
                path.display()
            );
            None
        }
        Err(e) => {
            error!("{} disappeared while opening it: {e}", path.display());
            None
        }
    }
}

fn stat_identity(dev: u64, ino: u64) -> (u64, u64) {
    (dev, ino)
}

/// The (dev, ino) identity of the node currently at `path`.
pub fn path_identity(path: &Path) -> Option<(u64, u64)> {
    std::fs::metadata(path)
        .ok()
        .map(|meta| stat_identity(meta.dev(), meta.ino()))
}

/// The sysfs identity of the input DEVICE behind a devnode:
/// `/sys/class/input/eventN/device` resolved (the `inputN` directory).
///
/// Stable across a devnode being re-created, different for a different device —
/// which is exactly the distinction the reconciler needs. `None` when sysfs
/// gives no answer (no udev, no /sys mount): callers then fall back to treating
/// a re-created node as a different device.
pub fn device_identity(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?;
    let class = Path::new("/sys/class/input").join(name);
    std::fs::canonicalize(class.join("device"))
        .or_else(|_| std::fs::canonicalize(&class))
        .ok()
}

/// Enumerate and classify every `/dev/input/event*` device, assigning the
/// per-class indices in enumeration order (the startup path: the first device
/// of a kind is the best match for a client asking by index).
pub fn discover() -> Vec<DiscoveredDevice> {
    let mut mouse = 0u8;
    let mut keyboard = 0u8;
    let mut touch = 0u8;

    probe_all()
        .into_iter()
        .map(|probed| {
            let index = match probed.class {
                InputClass::Mouse => {
                    let index = mouse;
                    mouse += 1;
                    index
                }
                InputClass::Keyboard => {
                    let index = keyboard;
                    keyboard += 1;
                    index
                }
                InputClass::Touch => {
                    let index = touch;
                    touch += 1;
                    index
                }
            };
            DiscoveredDevice {
                path: probed.path,
                name: probed.name,
                resource: probed.class.resource(index),
            }
        })
        .collect()
}

/// Enumerate and classify every `/dev/input/event*` device WITHOUT assigning
/// indices: the hot-plug reconciler picks a free index per class itself, so a
/// device that replaces one that went away can keep its name.
pub fn probe_all() -> Vec<ProbedDevice> {
    let Ok(paths) = input_event_paths() else {
        error!("Failed to enumerate /dev/input");
        return Vec::new();
    };

    let mut devices = Vec::new();
    for path in &paths {
        // The evdev Device is only for probing capabilities; the caller
        // opens its own plain File for the registry.
        let device = match EvdevDevice::open(path) {
            Ok(device) => device,
            Err(e) => {
                error!("Failed to open {}: {e}", path.display());
                continue;
            }
        };

        let Some(class) = classify(&device) else {
            debug!(
                "Skipping {} ({}): not a mouse/keyboard/touch",
                path.display(),
                device.name().unwrap_or("unnamed")
            );
            continue;
        };

        devices.push(ProbedDevice {
            path: path.clone(),
            name: device.name().unwrap_or("unnamed").to_string(),
            class,
        });
    }
    devices
}

/// All `/dev/input/event*` paths, sorted for a stable resource order.
fn input_event_paths() -> std::io::Result<Vec<PathBuf>> {
    let mut paths = std::fs::read_dir("/dev/input")?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("event"))
        })
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

/// Classify an evdev device. Order matters: a touchscreen also reports keys
/// (BTN_TOUCH) and often ABS_X/ABS_Y; a mouse reports REL_X/REL_Y plus
/// buttons. Priority: touch > mouse > keyboard.
fn classify(device: &EvdevDevice) -> Option<InputClass> {
    let abs = device.supported_absolute_axes();
    // Multi-touch (e.g. a touchscreen with ABS_MT_* slots).
    if abs.is_some_and(|axes| axes.contains(AbsoluteAxisType::ABS_MT_POSITION_X)) {
        return Some(InputClass::Touch);
    }
    // Single-touch (ABS_X + ABS_Y).
    if abs.is_some_and(|axes| {
        axes.contains(AbsoluteAxisType::ABS_X) && axes.contains(AbsoluteAxisType::ABS_Y)
    }) {
        return Some(InputClass::Touch);
    }

    let rel = device.supported_relative_axes();
    if rel.is_some_and(|axes| {
        axes.contains(RelativeAxisType::REL_X) && axes.contains(RelativeAxisType::REL_Y)
    }) && device.supported_keys().is_some_and(|keys| {
        // A mouse has buttons: at least one in the BTN_MOUSE range
        // (0x110..=0x117). A relative-axis device without buttons (e.g. a
        // bare trackpoint) is not a usable mouse.
        (0x110..=0x117).any(|code| keys.contains(Key(code)))
    }) {
        return Some(InputClass::Mouse);
    }

    // Keyboard: a real typing key (ESC through Space, i.e. 1..=57). This
    // excludes power buttons, hotkey arrays, and mice/touch buttons
    // (BTN_*, 0x100+), which report keys outside that range.
    if let Some(keys) = device.supported_keys()
        && (1..=57).any(|code| keys.contains(Key(code)))
    {
        return Some(InputClass::Keyboard);
    }

    None
}
