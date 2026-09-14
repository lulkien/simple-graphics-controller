//! Input hot-plug: keep the server's input devices in step with `/dev/input`
//! while it runs.
//!
//! Without this the daemon was a snapshot of boot time: a device plugged in
//! later never existed for it, and a node re-created under a running daemon
//! (a udev trigger — installing anything with udev rules runs one — or a
//! replug) left it holding a deleted inode, which clients then resolved to
//! `/dev/input/eventN (deleted)` and libinput refused. Both are the same
//! problem: the server's view had to be reconciled with reality.
//!
//! Why polling instead of a udev/libudev monitor: this crate deliberately
//! avoids libudev (the `input` dependency is taken with `default-features =
//! false` so `evdev` brings no libudev in), udev's own node setup is
//! asynchronous anyway (a device that is not ready yet is simply retried on
//! the next pass), and the work per pass is a handful of opens. The interval is
//! short enough that a plug shows up before anybody notices.
//!
//! Removal is not just bookkeeping: a device that is gone must stop being
//! advertised, and whoever holds it must be revoked (the holder's fd is dead —
//! it points at the removed node). The engine handles the revoke handshake; see
//! [`PolicyEngine::withdraw`].

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use simple_graphics_protocol::{InputResource, Resource};
use tracing::{error, info, warn};

use super::input::{self, HeldInput, InputClass, InputIndex};
use crate::{
    resource_manager::ResourceRegistries,
    types::AdvertisedResources,
    windowing::{Policy, PolicyEngine},
};

/// How often the device list is reconciled with `/dev/input`.
pub const RECONCILE_INTERVAL: Duration = Duration::from_secs(2);

/// Reconcile the server's input devices with `/dev/input` until the process
/// ends. `policy` is the server's windowing policy (`SGC_POLICY`), applied to
/// every device adopted later exactly as it was to the ones opened at startup.
pub async fn run(
    registries: ResourceRegistries,
    index: InputIndex,
    advertised: Arc<AdvertisedResources>,
    engine: PolicyEngine,
    policy: Policy,
) {
    loop {
        tokio::time::sleep(RECONCILE_INTERVAL).await;
        reconcile(&registries, &index, &advertised, &engine, policy).await;
    }
}

/// One pass over `/dev/input`: adopt devices that appeared (or were replaced
/// under us), let go of the ones that are gone. Index assignment reuses the
/// index of a device that went away, so a replug keeps its resource name.
async fn reconcile(
    registries: &ResourceRegistries,
    index: &InputIndex,
    advertised: &AdvertisedResources,
    engine: &PolicyEngine,
    policy: Policy,
) {
    let probed = input::probe_all();
    let mut present: Vec<PathBuf> = Vec::new();

    for device in probed {
        present.push(device.path.clone());
        let current = input::path_identity(&device.path);
        let held = index.get(&device.path).map(|entry| entry.value().clone());
        match held {
            // Ours, and the node is the same inode: nothing to do — this is
            // every device on every pass.
            Some(held) if current == Some((held.dev, held.ino)) => {}

            // The node was re-created for the SAME device (a udev trigger —
            // installing anything with udev rules runs one — unlinks and
            // re-creates eventN). The device never went away, so a holder's fd
            // still works and revoking it would take working input away for
            // nothing. Only the server's own fd needs replacing: a deleted inode
            // resolves to "eventN (deleted)", which the next client's libinput
            // refuses.
            Some(held) if same_device(&device.path, &held) => {
                reopen(registries, index, &device.path, &held.resource);
            }

            // A different device under a name we already use: the old one is
            // gone (its fd is dead), the new one takes over the resource.
            Some(held) => {
                info!(
                    "{} is now a different device ({:?} replaced); handing the name over",
                    device.path.display(),
                    held.resource
                );
                withdraw(
                    registries,
                    index,
                    advertised,
                    engine,
                    &device.path,
                    &held.resource,
                )
                .await;
                adopt(
                    registries,
                    index,
                    advertised,
                    engine,
                    policy,
                    &device.path,
                    &device.name,
                    device.class,
                )
                .await;
            }

            // Something new.
            None => {
                adopt(
                    registries,
                    index,
                    advertised,
                    engine,
                    policy,
                    &device.path,
                    &device.name,
                    device.class,
                )
                .await;
            }
        }
    }

    // Held devices whose node is not in `/dev/input` this pass.
    let held: Vec<(PathBuf, HeldInput)> = index
        .iter()
        .map(|entry| (entry.key().clone(), entry.value().clone()))
        .collect();
    for (path, held) in held {
        if present.contains(&path) {
            continue;
        }
        // The node is absent, but is the DEVICE still there? A node that is
        // being re-created is missing for a moment while the device itself
        // never moved — that is not a removal, so keep the resource (and with
        // it the name) and re-open the node when it comes back.
        if held.device.as_ref().is_some_and(|device| device.exists()) {
            continue;
        }
        info!(
            "{} is gone; withdrawing {:?}",
            path.display(),
            held.resource
        );
        withdraw(registries, index, advertised, engine, &path, &held.resource).await;
    }
}

/// Is the node at `path` the same device the server already holds? Compares the
/// sysfs device, which survives a devnode being re-created; `false` when sysfs
/// cannot tell us (then the caller treats it as a different device).
fn same_device(path: &Path, held: &HeldInput) -> bool {
    match (&held.device, input::device_identity(path)) {
        (Some(held_device), Some(now)) => held_device == &now,
        _ => false,
    }
}

/// Replace the server's fd for a device whose node was re-created: the resource
/// keeps its name, its holder keeps the fd it has (it still works), and the NEXT
/// grant opens the node as it exists now.
fn reopen(registries: &ResourceRegistries, index: &InputIndex, path: &Path, resource: &Resource) {
    let Some((fd, dev, ino)) = input::open_device(path) else {
        return;
    };
    let raw = std::os::fd::AsRawFd::as_raw_fd(&fd);
    registries.fds.insert(resource.clone(), fd);
    index.insert(
        path.to_path_buf(),
        HeldInput {
            resource: resource.clone(),
            dev,
            ino,
            device: input::device_identity(path),
        },
    );
    info!(
        "{} was re-created for the same device; re-opened as {resource:?} (fd {raw}) for future grants \
         — a current holder keeps the fd it has",
        path.display()
    );
}

/// Open a device and start offering it as an input resource.
#[allow(clippy::too_many_arguments)]
async fn adopt(
    registries: &ResourceRegistries,
    index: &InputIndex,
    advertised: &AdvertisedResources,
    engine: &PolicyEngine,
    policy: Policy,
    path: &Path,
    name: &str,
    class: InputClass,
) {
    let Some(index_in_class) = free_index(index, class) else {
        error!(
            "{}: every {class:?} index is taken; cannot register it",
            path.display()
        );
        return;
    };
    let input_resource = class.resource(index_in_class);
    let resource = Resource::Input(input_resource);

    // Open BEFORE registering: a device that cannot be opened yet (udev has
    // not finished with it) is simply picked up on the next pass.
    let Some((fd, dev, ino)) = input::open_device(path) else {
        return;
    };

    let raw = std::os::fd::AsRawFd::as_raw_fd(&fd);
    registries.fds.insert(resource.clone(), fd);
    index.insert(
        path.to_path_buf(),
        HeldInput {
            resource: resource.clone(),
            dev,
            ino,
            device: input::device_identity(path),
        },
    );
    advertised.insert(resource.clone());
    engine.offer(resource.clone(), policy).await;
    info!(
        "Opened {} ({name}): {resource:?} (fd {raw}, plugged in while running)",
        path.display()
    );
}

/// Stop offering a device and revoke its holder. The fd closes with the
/// registry entry, which also means a later re-`adopt` of the same path opens
/// the CURRENT node.
async fn withdraw(
    registries: &ResourceRegistries,
    index: &InputIndex,
    advertised: &AdvertisedResources,
    engine: &PolicyEngine,
    path: &Path,
    resource: &Resource,
) {
    advertised.remove(resource);
    engine.withdraw(resource.clone()).await;
    registries.fds.remove(resource);
    index.remove(path);
    warn!(
        "Withdrew {resource:?} ({}): the device is gone; a client holding it is being revoked",
        path.display()
    );
}

/// The lowest per-class index no held device uses. A replug therefore lands on
/// the name its device had before, instead of shifting every other device.
fn free_index(index: &InputIndex, class: InputClass) -> Option<u8> {
    let mut used: Vec<u8> = index
        .iter()
        .filter_map(|entry| match &entry.value().resource {
            Resource::Input(input) if InputClass::of(input) == class => Some(match input {
                InputResource::Mouse(index)
                | InputResource::Keyboard(index)
                | InputResource::Touch(index) => *index,
            }),
            _ => None,
        })
        .collect();
    used.sort_unstable();
    (0u8..=u8::MAX).find(|candidate| !used.contains(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource_manager::input::new_index;

    fn hold(index: &InputIndex, resource: Resource, path: &str) {
        index.insert(
            PathBuf::from(path),
            HeldInput {
                resource,
                dev: 0,
                ino: 0,
                device: None,
            },
        );
    }

    /// A replug must land on its own old name, so a device that goes away and
    /// comes back does not shift the names of everything plugged in after it.
    #[test]
    fn free_index_reuses_the_lowest_gap() {
        let index = new_index();
        assert_eq!(free_index(&index, InputClass::Mouse), Some(0));

        hold(
            &index,
            Resource::Input(InputResource::Mouse(0)),
            "/dev/input/event1",
        );
        assert_eq!(free_index(&index, InputClass::Mouse), Some(1));
        assert_eq!(free_index(&index, InputClass::Keyboard), Some(0));

        hold(
            &index,
            Resource::Input(InputResource::Mouse(2)),
            "/dev/input/event3",
        );
        assert_eq!(free_index(&index, InputClass::Mouse), Some(1));
    }
}
