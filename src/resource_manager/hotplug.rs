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
//! A device that goes away is not revoked — it is **suspended**: the resource
//! keeps its holder, leaves the advertised list while the device is away, and
//! is handed back to that same client (fresh fd, same name) the moment the
//! device returns. Unplugging a mouse must not cost a running app its mouse for
//! the rest of its life, and it must not cost it a re-acquire that a client with
//! no display could win the race for. See [`PolicyEngine::suspend`] and
//! [`PolicyEngine::resume`].

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
    windowing::{ControlMessage, Policy, PolicyEngine},
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

            // The device is back at the node it had: its holder kept the
            // resource while it was away, so it gets a fresh fd for this device
            // — provided it IS that device's class (see `may_resume`).
            Some(held) if held.suspended && may_resume(&held, device.class) => {
                if !resume(
                    registries,
                    index,
                    advertised,
                    engine,
                    &device.path,
                    &device.path,
                    &device.name,
                    &held,
                )
                .await
                {
                    retry = true;
                }
            }

            // A device of another class took the node over. The suspended claim
            // keeps its name and waits for its own device, and the newcomer is a
            // new resource — never the name of a class it is not.
            Some(held) if held.suspended => {
                info!(
                    "{} ({}) is a {:?}, not the device {:?} waits for: the claim keeps its name and this device is adopted",
                    device.path.display(),
                    device.name,
                    device.class,
                    held.resource
                );
                park(index, &device.path);
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

            // A different device under a name we already use. If the old device
            // is still there (an odd udev event) the holder's fd keeps working:
            // only the server's fd moves to the current node. If it is gone, the
            // device was replaced inside one pass — the holder keeps the name and
            // gets the new device's fd, exactly as if the daemon had seen the
            // gap, but only when the new device is of the same class.
            Some(held) => {
                let replaced = held.device.as_ref().is_some_and(|device| device.exists());
                if !replaced {
                    info!(
                        "{} is now a different device ({:?} was replaced); its holder keeps the name",
                        device.path.display(),
                        held.resource
                    );
                }
                let taken = if replaced {
                    reopen(registries, index, &device.path, &held.resource)
                } else if may_resume(&held, device.class) {
                    resume(
                        registries,
                        index,
                        advertised,
                        engine,
                        &device.path,
                        &device.path,
                        &device.name,
                        &held,
                    )
                    .await
                } else {
                    // Another class took the node and the device this resource
                    // held is gone: suspend it (its holder keeps the name, off
                    // the node) and register the newcomer as a new resource.
                    suspend(registries, index, advertised, engine, &device.path, &held).await;
                    park(index, &device.path);
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
                    .await
                };
                if !taken {
                    retry = true;
                }
            }

            // Something new — unless it is the device a suspended resource is
            // waiting for. A device that comes back on a DIFFERENT node (a replug
            // into another port) still belongs to the client that held that name:
            // it asked for "the mouse", not for a specific devnode.
            None => match suspended_peer(index, device.class) {
                Some((old_path, held)) => {
                    info!(
                        "{} ({}) takes over {:?} from {}: the device is back on another node",
                        device.path.display(),
                        device.name,
                        held.resource,
                        old_path.display()
                    );
                    if !resume(
                        registries,
                        index,
                        advertised,
                        engine,
                        &old_path,
                        &device.path,
                        &device.name,
                        &held,
                    )
                    .await
                    {
                        retry = true;
                    }
                }
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
            },
        }
    }

    // Held devices whose node is not in `/dev/input` this pass.
    let held: Vec<(PathBuf, HeldInput)> = index
        .iter()
        .map(|entry| (entry.key().clone(), entry.value().clone()))
        .collect();
    for (path, held) in held {
        if held.suspended || present.contains(&path) {
            continue;
        }
        // The node is absent, but is the DEVICE still there? A node that is
        // being re-created is missing for a moment while the device itself
        // never moved — that is not a removal, so keep the resource (and with
        // it the name) and re-open the node when it comes back.
        if held.device.as_ref().is_some_and(|device| device.exists()) {
            continue;
        }
        suspend(registries, index, advertised, engine, &path, &held).await;
    }

    // The invariants this module maintains by hand, checked where they can
    // break. Debug builds only: a violation is a bug in the code above, not
    // something to take a board down over.
    #[cfg(debug_assertions)]
    for problem in super::check_consistency(&registries.fds, index, advertised) {
        error!("resource invariant violated: {problem}");
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
            suspended: false,
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
            suspended: false,
        },
    );
    advertised.insert(resource.clone());
    // Offer BEFORE pushing: a client that acts on the pushed list immediately
    // asks for the resource, and that Acquire has to be accepted.
    engine.offer(resource.clone(), policy).await;
    info!(
        "Opened {} ({name}): {resource:?} (fd {raw}, plugged in while running)",
        path.display()
    );
    push(advertised, engine).await;
    true
}

/// The device is gone. Nothing is revoked: the resource, and the client holding
/// it, stay exactly as they are — only the resource leaves the advertised list
/// (it cannot be granted while its device is away). The entry stays in the
/// index, marked suspended, so the device that comes back resumes the same name
/// instead of being adopted as something new.
async fn suspend(
    registries: &ResourceRegistries,
    index: &InputIndex,
    advertised: &AdvertisedResources,
    engine: &PolicyEngine,
    path: &Path,
    held: &HeldInput,
) {
    advertised.remove(&held.resource);
    engine.suspend(held.resource.clone()).await;
    registries.fds.remove(&held.resource);
    index.insert(
        path.to_path_buf(),
        HeldInput {
            suspended: true,
            ..held.clone()
        },
    );
    warn!(
        "Suspended {:?} ({}): the device is gone; its holder keeps it and is told when it is back",
        held.resource,
        path.display()
    );
    push(advertised, engine).await;
}

/// The device is back. Whoever held the resource still holds it — there is no
/// re-acquire, and no window in which another client could take the name — so
/// all that is left is to hand that client a fresh fd for the device that
/// returned. `false` if the node would not open yet.
///
/// Order matters: the fd is registered BEFORE the engine re-grants, because the
/// grant is a dup of that registry entry.
#[allow(clippy::too_many_arguments)]
async fn resume(
    registries: &ResourceRegistries,
    index: &InputIndex,
    advertised: &AdvertisedResources,
    engine: &PolicyEngine,
    old_path: &Path,
    path: &Path,
    name: &str,
    held: &HeldInput,
) -> bool {
    let resource = held.resource.clone();
    let Some((fd, dev, ino)) = input::open_device(path) else {
        return false;
    };
    let raw = std::os::fd::AsRawFd::as_raw_fd(&fd);
    registries.fds.insert(resource.clone(), fd);
    if old_path != path {
        index.remove(old_path);
    }
    index.insert(
        path.to_path_buf(),
        HeldInput {
            resource: resource.clone(),
            dev,
            ino,
            device: input::device_identity(path),
            suspended: false,
        },
    );
    advertised.insert(resource.clone());
    engine.resume(resource.clone()).await;
    info!(
        "Resumed {resource:?} ({} ({name}), fd {raw}): the device is back with its holder — no re-acquire",
        path.display()
    );
    push(advertised, engine).await;
    true
}

/// Tell every connected client what the server offers now. Without this, a
/// client's view is whatever it was handed at connect time — which is what kept
/// a device that appeared later invisible to it.
async fn push(advertised: &AdvertisedResources, engine: &PolicyEngine) {
    engine
        .broadcast(ControlMessage::Advertise {
            available_resources: advertised.snapshot(),
        })
        .await;
}

/// May this device be the one a held (suspended) resource is waiting for? Only
/// a device of the resource's OWN class: the entry holds a name like
/// `Keyboard(2)`, and its class is part of what the resource means to its
/// holder — handing the name to a mouse gives that client a device its class
/// says it is not. A device of another class on the node is a new device.
fn may_resume(held: &HeldInput, class: InputClass) -> bool {
    InputClass::of_resource(&held.resource) == Some(class)
}

/// Move a suspended entry off the devnode it used to sit on: the resource — and
/// with it the holder's claim — is kept under a placeholder key, so the node is
/// free for the device that took it over, and `suspended_peer` still finds the
/// claim by class when the device it waits for comes back, on whatever node
/// that is.
fn park(index: &InputIndex, path: &Path) {
    let Some((_, held)) = index.remove(path) else {
        return;
    };
    let key = PathBuf::from(format!("<suspended>/{:?}", held.resource));
    index.insert(key, held);
}

/// The lowest per-class index no held device uses. A replug therefore lands on
/// the name its device had before, instead of shifting every other device.
fn free_index(index: &InputIndex, class: InputClass) -> Option<u8> {
    let mut used: Vec<u8> = index
        .iter()
        .filter(|entry| InputClass::of_resource(&entry.value().resource) == Some(class))
        .filter_map(|entry| resource_index(&entry.value().resource))
        .collect();
    used.sort_unstable();
    (0u8..=u8::MAX).find(|candidate| !used.contains(candidate))
}

/// A suspended resource of `class` whose device has not come back, lowest name
/// first. Used when a device appears on a node we do not hold: it may be the
/// device a suspended resource is waiting for, returning on a different node.
fn suspended_peer(index: &InputIndex, class: InputClass) -> Option<(PathBuf, HeldInput)> {
    let mut candidates: Vec<(PathBuf, HeldInput)> = index
        .iter()
        .filter(|entry| entry.value().suspended)
        .filter(|entry| InputClass::of_resource(&entry.value().resource) == Some(class))
        .filter(|entry| !entry.key().exists())
        .map(|entry| (entry.key().clone(), entry.value().clone()))
        .collect();
    candidates.sort_by_key(|(_, held)| resource_index(&held.resource));
    candidates.into_iter().next()
}

/// The per-class index of an input resource (`Keyboard(1)` → 1).
fn resource_index(resource: &Resource) -> Option<u8> {
    match resource {
        Resource::Input(
            InputResource::Mouse(index)
            | InputResource::Keyboard(index)
            | InputResource::Touch(index),
        ) => Some(*index),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource_manager::input::new_index;

    fn hold(index: &InputIndex, resource: Resource, path: &str) {
        hold_as(index, resource, path, false);
    }

    fn hold_as(index: &InputIndex, resource: Resource, path: &str, suspended: bool) {
        index.insert(
            PathBuf::from(path),
            HeldInput {
                resource,
                dev: 0,
                ino: 0,
                device: None,
                suspended,
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

    #[test]
    fn a_suspended_resource_only_resumes_its_own_class() {
        let index = new_index();
        hold_as(
            &index,
            Resource::Input(InputResource::Keyboard(2)),
            "/dev/input/event6",
            true,
        );
        let held = index
            .get(Path::new("/dev/input/event6"))
            .expect("held")
            .value()
            .clone();

        assert!(
            may_resume(&held, InputClass::Keyboard),
            "its own class resumes"
        );
        assert!(
            !may_resume(&held, InputClass::Mouse),
            "a mouse must not inherit Keyboard(2)"
        );
        assert!(!may_resume(&held, InputClass::Touch));
    }

    /// A suspended resource is one whose device has not come back: a device that
    /// appears on a node nobody holds may be that device returning somewhere
    /// else, and the name it takes over has to be the one its holder holds.
    /// Lowest name first, and only for a node that is really gone.
    #[test]
    fn a_suspended_resource_waits_for_a_device_of_its_own_class() {
        let index = new_index();
        hold_as(
            &index,
            Resource::Input(InputResource::Mouse(0)),
            "/tmp/sgc-no-such-node-5",
            true,
        );
        hold_as(
            &index,
            Resource::Input(InputResource::Mouse(1)),
            "/tmp/sgc-no-such-node-6",
            true,
        );
        hold(
            &index,
            Resource::Input(InputResource::Keyboard(0)),
            "/dev/input/event1",
        );

        assert_eq!(
            suspended_peer(&index, InputClass::Mouse).map(|(_, held)| held.resource),
            Some(Resource::Input(InputResource::Mouse(0)))
        );
        assert!(suspended_peer(&index, InputClass::Keyboard).is_none());
        assert!(suspended_peer(&index, InputClass::Touch).is_none());
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
