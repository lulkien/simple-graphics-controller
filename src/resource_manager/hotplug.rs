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
//! What wakes the reconciler is an **inotify watch on `/dev/input`** (create,
//! delete, moved-to), so a plug is picked up in ~[`COALESCE`], not on a tick.
//! Two things that watch cannot do, and the reasons the poll survives as a
//! backstop:
//!
//! - events can be missed (an overflowed queue, a watch installed after the
//!   initial enumeration), and a stale view has nothing else to correct it, so a
//!   [`SAFETY_INTERVAL`] pass runs regardless;
//! - an event says the NODE exists, not that it is openable: udev is still
//!   chmod-ing it when the kernel announces it, so a device that will not open
//!   yet is retried after [`OPEN_RETRY`].
//!
//! If the watch cannot be installed at all (no `/dev/input` yet, early boot) the
//! reconciler falls back to polling every [`FALLBACK_INTERVAL`] — the behaviour
//! it had before the watch existed.
//!
//! Removal is not just bookkeeping: a device that is gone must stop being
//! advertised, and whoever holds it must be revoked (the holder's fd is dead —
//! it points at the removed node). The engine handles the revoke handshake; see
//! [`PolicyEngine::withdraw`].

use std::{
    os::fd::{AsFd, OwnedFd},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use nix::{
    errno::Errno,
    sys::inotify::{AddWatchFlags, InitFlags, Inotify},
};
use simple_graphics_protocol::{InputResource, Resource};
use tokio::io::unix::AsyncFd;
use tracing::{debug, error, info, warn};

use super::input::{self, HeldInput, InputClass, InputIndex};
use crate::{
    resource_manager::ResourceRegistries,
    types::AdvertisedResources,
    windowing::{Policy, PolicyEngine},
};

/// The directory whose changes wake the reconciler.
const WATCH_DIR: &str = "/dev/input";

/// The watch mask: a device node appearing, going away, or being put in place by
/// a rename. `IN_ATTRIB` is deliberately absent — udev chmods the node right
/// after the kernel creates it, and that carries no information we act on.
const WATCH_MASK: AddWatchFlags = AddWatchFlags::IN_CREATE
    .union(AddWatchFlags::IN_DELETE)
    .union(AddWatchFlags::IN_MOVED_TO)
    .union(AddWatchFlags::IN_ONLYDIR);

/// How long to let an event burst settle before reconciling. A plug fires
/// several events (the node, udev's chmod, a trigger touching neighbours), and
/// the pass is idempotent: one reconcile per burst is enough.
const COALESCE: Duration = Duration::from_millis(200);

/// The backstop pass, running whether or not events arrive.
const SAFETY_INTERVAL: Duration = Duration::from_secs(60);

/// How long to wait before re-trying a device that a pass could not open yet.
const OPEN_RETRY: Duration = Duration::from_millis(500);

/// How many open-retries a single wake may cost before the backstop takes over
/// (a device that never opens must not spin the loop).
const MAX_RETRIES: u32 = 5;

/// The poll interval used when `/dev/input` cannot be watched.
const FALLBACK_INTERVAL: Duration = Duration::from_secs(2);

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
    // Watching only reports what happens AFTER the watch exists, and the devices
    // adopted before it (at startup) are already in the index — so reconcile
    // once here to close the gap between the daemon's enumeration and the watch.
    match Watch::new(Path::new(WATCH_DIR)) {
        Ok(watch) => {
            info!(
                "Watching {WATCH_DIR} for device changes (safety pass every {SAFETY_INTERVAL:?})"
            );
            run_watched(registries, index, advertised, engine, policy, watch).await;
        }
        Err(e) => {
            warn!(
                "Cannot watch {WATCH_DIR} ({e}); polling every {FALLBACK_INTERVAL:?} instead \
                 (a device that appears will still be adopted)"
            );
            run_polling(registries, index, advertised, engine, policy).await;
        }
    }
}

/// Event-driven: reconcile when `/dev/input` changes, plus the backstop pass.
async fn run_watched(
    registries: ResourceRegistries,
    index: InputIndex,
    advertised: Arc<AdvertisedResources>,
    engine: PolicyEngine,
    policy: Policy,
    watch: Watch,
) {
    let mut safety = tokio::time::interval(SAFETY_INTERVAL);
    safety.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    safety.tick().await; // the first tick is immediate; skip it

    loop {
        let reason = tokio::select! {
            changed = watch.changed() => {
                if let Err(e) = changed {
                    warn!(
                        "Input watch failed ({e}); polling every {FALLBACK_INTERVAL:?} from now on"
                    );
                    return run_polling(registries, index, advertised, engine, policy).await;
                }
                "device change"
            }
            _ = safety.tick() => "safety pass",
        };
        debug!("Reconciling input devices ({reason})");

        let mut attempt = 0;
        loop {
            let retry = reconcile(&registries, &index, &advertised, &engine, policy).await;
            attempt += 1;
            if !retry || attempt >= MAX_RETRIES {
                if retry {
                    warn!(
                        "{WATCH_DIR} has a device that will not open after {MAX_RETRIES} tries; \
                         leaving it to the {SAFETY_INTERVAL:?} pass"
                    );
                }
                break;
            }
            tokio::time::sleep(OPEN_RETRY).await;
        }
    }
}

/// The fallback: the same pass on a timer, used when there is nothing to watch.
async fn run_polling(
    registries: ResourceRegistries,
    index: InputIndex,
    advertised: Arc<AdvertisedResources>,
    engine: PolicyEngine,
    policy: Policy,
) {
    loop {
        tokio::time::sleep(FALLBACK_INTERVAL).await;
        reconcile(&registries, &index, &advertised, &engine, policy).await;
    }
}

/// The inotify watch on `/dev/input`, plus a duplicate of its fd for the async
/// runtime.
///
/// The dup shares the instance's event queue, it only exists because
/// `tokio::io::unix::AsyncFd` wants `AsRawFd` and nix's `Inotify` implements
/// `AsFd` alone. Events are always read through the `Inotify` handle.
struct Watch {
    inotify: Inotify,
    ready: AsyncFd<OwnedFd>,
}

impl Watch {
    fn new(dir: &Path) -> std::io::Result<Self> {
        let inotify = Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC)
            .map_err(std::io::Error::from)?;
        inotify
            .add_watch(dir, WATCH_MASK)
            .map_err(std::io::Error::from)?;
        let ready = AsyncFd::new(inotify.as_fd().try_clone_to_owned()?)?;
        Ok(Self { inotify, ready })
    }

    /// Wait for `/dev/input` to change, then let the burst settle.
    async fn changed(&self) -> std::io::Result<()> {
        loop {
            let mut ready = self.ready.readable().await?;
            match self.inotify.read_events() {
                Ok(events) => {
                    ready.clear_ready();
                    if events
                        .iter()
                        .any(|event| event.mask.contains(AddWatchFlags::IN_Q_OVERFLOW))
                    {
                        // Events were dropped: reconcile from scratch, which is
                        // exactly what the caller does next.
                        warn!("Input event queue overflowed; reconciling from scratch");
                    } else {
                        debug!("{WATCH_DIR} changed ({} event(s))", events.len());
                    }
                    break;
                }
                // Readiness without a readable event: try again.
                Err(Errno::EAGAIN) => ready.clear_ready(),
                Err(e) => return Err(e.into()),
            }
        }
        tokio::time::sleep(COALESCE).await;
        // Discard whatever the burst produced while we slept.
        let _ = self.inotify.read_events();
        Ok(())
    }
}

/// One pass over `/dev/input`: adopt devices that appeared (or were replaced
/// under us), let go of the ones that are gone. Index assignment reuses the
/// index of a device that went away, so a replug keeps its resource name.
///
/// Returns `true` when a device node is there but could not be taken this pass
/// (udev is still setting it up): the caller retries shortly, because with an
/// event-driven wake there may be no next pass for a while.
async fn reconcile(
    registries: &ResourceRegistries,
    index: &InputIndex,
    advertised: &AdvertisedResources,
    engine: &PolicyEngine,
    policy: Policy,
) -> bool {
    let probed = input::probe_all();
    let mut present: Vec<PathBuf> = Vec::new();
    let mut retry = false;

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
                if !reopen(registries, index, &device.path, &held.resource) {
                    retry = true;
                }
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
                if !adopt(
                    registries,
                    index,
                    advertised,
                    engine,
                    policy,
                    &device.path,
                    &device.name,
                    device.class,
                )
                .await
                {
                    retry = true;
                }
            }

            // Something new.
            None => {
                if !adopt(
                    registries,
                    index,
                    advertised,
                    engine,
                    policy,
                    &device.path,
                    &device.name,
                    device.class,
                )
                .await
                {
                    retry = true;
                }
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

    retry
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
/// grant opens the node as it exists now. `false` if the node would not open yet.
fn reopen(
    registries: &ResourceRegistries,
    index: &InputIndex,
    path: &Path,
    resource: &Resource,
) -> bool {
    let Some((fd, dev, ino)) = input::open_device(path) else {
        return false;
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
    true
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
) -> bool {
    let Some(index_in_class) = free_index(index, class) else {
        error!(
            "{}: every {class:?} index is taken; cannot register it",
            path.display()
        );
        return false;
    };
    let input_resource = class.resource(index_in_class);
    let resource = Resource::Input(input_resource);

    // Open BEFORE registering: a device that cannot be opened yet (udev has
    // not finished with it) is simply picked up on the next pass.
    let Some((fd, dev, ino)) = input::open_device(path) else {
        return false;
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
    true
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

    /// The fallback path is chosen by this failing (no `/dev/input` yet, early
    /// boot): it must be an error, never a panic or a watch that reports success.
    #[test]
    fn watch_creation_fails_for_a_missing_directory() {
        let missing = std::env::temp_dir().join("sgc-inotify-does-not-exist");
        assert!(Watch::new(&missing).is_err());
    }

    /// The watch is what wakes the reconciler, so it has to report a device node
    /// appearing (an empty file stands in for `eventN`) and settle the burst.
    #[tokio::test]
    async fn watch_reports_a_new_device_node() {
        let dir = std::env::temp_dir().join(format!("sgc-inotify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let watch = Watch::new(&dir).expect("watch");

        std::fs::write(dir.join("event99"), b"").expect("create node");

        let event = tokio::time::timeout(Duration::from_secs(5), watch.changed()).await;
        std::fs::remove_dir_all(&dir).ok();
        event
            .expect("the watch did not report a new file")
            .expect("watch error");
    }
}
