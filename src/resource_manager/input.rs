//! Input device discovery and registration: enumerate `/dev/input/event*`,
//! classify each device, and register the classifiable ones as
//! `Resource::Input(_)`. Compiled only with the `input` feature (default).

use std::{fs::File, os::fd::AsRawFd, path::PathBuf};

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

/// The class of an input device, before its per-class index is assigned.
enum InputClass {
    Mouse,
    Keyboard,
    Touch,
}

/// Open and register every input device the discovery [`discover`] found.
/// Each registry fd is the server's own open; grants dup it (the client
/// parses evdev events straight off the dup — no path needed).
pub(super) fn open_devices(resource_reg: ResourceRegistry, advertised: &mut Vec<Resource>) {
    for device in discover() {
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
        info!(
            "Opened {} ({}): {resource:?} (fd {fd})",
            device.path.display(),
            device.name
        );
    }
}

/// Enumerate and classify every `/dev/input/event*` device. Devices of the
/// same class are indexed (`Mouse(0)`, `Mouse(1)`, ...) so the registry can
/// hold several at once. Unclassifiable devices are skipped.
pub fn discover() -> Vec<DiscoveredDevice> {
    let Ok(paths) = input_event_paths() else {
        error!("Failed to enumerate /dev/input");
        return Vec::new();
    };

    let mut mouse = 0u8;
    let mut keyboard = 0u8;
    let mut touch = 0u8;

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

        let resource = match class {
            InputClass::Mouse => {
                let index = mouse;
                mouse += 1;
                InputResource::Mouse(index)
            }
            InputClass::Keyboard => {
                let index = keyboard;
                keyboard += 1;
                InputResource::Keyboard(index)
            }
            InputClass::Touch => {
                let index = touch;
                touch += 1;
                InputResource::Touch(index)
            }
        };

        devices.push(DiscoveredDevice {
            path: path.clone(),
            name: device.name().unwrap_or("unnamed").to_string(),
            resource,
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
