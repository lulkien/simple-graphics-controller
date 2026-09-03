//! The `sgc_*` C ABI: thin, panic-safe wrappers over the Rust core.
//!
//! Every public function maps one-to-one onto a `libsgc_rs` call. All
//! fallible functions return `0`/`-1` (or a handle/`NULL`) and, given an
//! `err` buffer, write a NUL-terminated message. No panics cross the ABI
//! (`catch_unwind` at every entry point).
//!
//! Ownership rules (mirrored in `include/libsgc.h`):
//! - the client handle is opaque, created by `sgc_connect`, consumed by
//!   `sgc_release` — one thread per client;
//! - the fd in a `GRANTED` event and the fd from `sgc_fd` are owned by the
//!   caller (close them);
//! - the array from `sgc_advertised` is malloc'd — free it with `sgc_free`.

use std::{
    ffi::{c_char, c_int, c_void},
    os::fd::IntoRawFd,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    time::Duration,
};

use libc::{free, malloc, size_t};
use libsgc_rs::{InputResource, Resource, SgcClient, SgcEvent};

// --- Constants -----------------------------------------------------------

/// Flat resource kinds (`sgc_resource.kind`). The input classes are
/// distinct kinds so kind+index is a total round-trip encoding of
/// [`Resource`] (the 3-level Rust enum cannot flatten to kind+index
/// otherwise).
pub const SGC_RESOURCE_FBDEV: c_int = 0;
pub const SGC_RESOURCE_DRM: c_int = 1;
pub const SGC_RESOURCE_MOUSE: c_int = 2;
pub const SGC_RESOURCE_KEYBOARD: c_int = 3;
pub const SGC_RESOURCE_TOUCH: c_int = 4;

/// Event kinds (`sgc_event.kind`).
pub const SGC_EVENT_REVOKED: c_int = 0;
pub const SGC_EVENT_GRANTED: c_int = 1;

// --- Types ---------------------------------------------------------------

/// Opaque client handle. Never constructed; only cast from/to the private
/// [`Ctx`] behind it.
#[repr(C)]
pub struct sgc_client {
    _private: [u8; 0],
}

/// A resource in the C ABI: kind + index.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct sgc_resource {
    pub kind: c_int,
    /// DRM: card index; MOUSE/KEYBOARD/TOUCH: device index; FBDEV: ignored.
    pub index: c_int,
}

impl sgc_resource {
    fn from_resource(r: &Resource) -> Self {
        match r {
            Resource::Fbdev => Self {
                kind: SGC_RESOURCE_FBDEV,
                index: 0,
            },
            Resource::Drm { card } => Self {
                kind: SGC_RESOURCE_DRM,
                index: *card as c_int,
            },
            Resource::Input(InputResource::Mouse(i)) => Self {
                kind: SGC_RESOURCE_MOUSE,
                index: *i as c_int,
            },
            Resource::Input(InputResource::Keyboard(i)) => Self {
                kind: SGC_RESOURCE_KEYBOARD,
                index: *i as c_int,
            },
            Resource::Input(InputResource::Touch(i)) => Self {
                kind: SGC_RESOURCE_TOUCH,
                index: *i as c_int,
            },
        }
    }

    fn to_resource(self) -> Option<Resource> {
        let index = |i: c_int| u8::try_from(i).ok();
        match (self.kind, self.index) {
            (SGC_RESOURCE_FBDEV, _) => Some(Resource::Fbdev),
            (SGC_RESOURCE_DRM, i) => index(i).map(|card| Resource::Drm { card }),
            (SGC_RESOURCE_MOUSE, i) => index(i).map(|d| Resource::Input(InputResource::Mouse(d))),
            (SGC_RESOURCE_KEYBOARD, i) => {
                index(i).map(|d| Resource::Input(InputResource::Keyboard(d)))
            }
            (SGC_RESOURCE_TOUCH, i) => index(i).map(|d| Resource::Input(InputResource::Touch(d))),
            _ => None,
        }
    }
}

/// One event from [`sgc_pump`]. The `fd` is `>= 0` only for `GRANTED`, and
/// is owned by the caller (close it).
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct sgc_event {
    pub kind: c_int,
    pub resource: sgc_resource,
    pub fd: c_int,
}

/// The private state behind the opaque handle: the Rust client plus the
/// advertised list (the core returns it from `connect` and does not keep
/// it, so the shim does).
struct Ctx {
    client: SgcClient,
    advertised: Vec<sgc_resource>,
}

// --- Helpers -------------------------------------------------------------

/// Write `msg` (truncated) into the caller's error buffer.
fn set_err(err: *mut c_char, err_len: usize, msg: &str) {
    if err.is_null() || err_len == 0 {
        return;
    }
    let n = msg.len().min(err_len - 1);
    // Safety: err/err_len came from the C caller; we write at most
    // err_len - 1 bytes plus the NUL terminator.
    unsafe {
        ptr::copy_nonoverlapping(msg.as_ptr(), err.cast::<u8>(), n);
        *err.add(n) = 0;
    }
}

/// Run a fallible ABI body with panic containment; `-1` + err text on any
/// failure.
fn abi_int(err: *mut c_char, err_len: size_t, f: impl FnOnce() -> Result<c_int, String>) -> c_int {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(v)) => v,
        Ok(Err(msg)) => {
            set_err(err, err_len, &msg);
            -1
        }
        Err(_) => {
            set_err(err, err_len, "internal panic in libsgc");
            -1
        }
    }
}

// --- The ABI -------------------------------------------------------------

/// Connect to the controller's abstract socket `@sgc`. Returns an opaque
/// handle, or `NULL` with an error message on failure.
#[unsafe(no_mangle)]
pub extern "C" fn sgc_connect(err: *mut c_char, err_len: size_t) -> *mut sgc_client {
    let out = catch_unwind(AssertUnwindSafe(|| -> Result<*mut sgc_client, String> {
        let (client, resources) = SgcClient::connect().map_err(|e| e.to_string())?;
        let advertised = resources
            .iter()
            .map(sgc_resource::from_resource)
            .collect::<Vec<_>>();
        let ctx = Box::new(Ctx { client, advertised });
        Ok(Box::into_raw(ctx) as *mut sgc_client)
    }));
    match out {
        Ok(Ok(handle)) => handle,
        Ok(Err(msg)) => {
            set_err(err, err_len, &msg);
            ptr::null_mut()
        }
        Err(_) => {
            set_err(err, err_len, "internal panic in sgc_connect");
            ptr::null_mut()
        }
    }
}

/// Copy the controller's advertised resources into a freshly malloc'd
/// array (`*out`; `NULL` when there are none). The caller owns it and must
/// release it with [`sgc_free`].
#[unsafe(no_mangle)]
pub extern "C" fn sgc_advertised(
    c: *mut sgc_client,
    out: *mut *mut sgc_resource,
    count: *mut size_t,
) -> c_int {
    abi_int(ptr::null_mut(), 0, || unsafe {
        if c.is_null() {
            return Err("null client handle".into());
        }
        if out.is_null() || count.is_null() {
            return Err("null out/count pointer".into());
        }
        // Read-only access to the advertised list.
        let ctx = &*(c as *const Ctx);
        let n = ctx.advertised.len();
        *count = n;
        if n == 0 {
            *out = ptr::null_mut();
            return Ok(0);
        }
        let raw = malloc(n * size_of::<sgc_resource>()) as *mut sgc_resource;
        if raw.is_null() {
            return Err("out of memory".into());
        }
        ptr::copy_nonoverlapping(ctx.advertised.as_ptr(), raw, n);
        *out = raw;
        Ok(0)
    })
}

/// Free a pointer previously returned by [`sgc_advertised`] (plain
/// `malloc`/`free` pairing).
#[unsafe(no_mangle)]
pub extern "C" fn sgc_free(p: *mut c_void) {
    // Safety: p came from malloc in sgc_advertised (or NULL).
    unsafe { free(p) }
}

/// Request `resource` and block until the server answers. On success the
/// client holds the granted fd; borrow it with [`sgc_fd`].
#[unsafe(no_mangle)]
pub extern "C" fn sgc_acquire(
    c: *mut sgc_client,
    r: sgc_resource,
    err: *mut c_char,
    err_len: size_t,
) -> c_int {
    abi_int(err, err_len, || unsafe {
        if c.is_null() {
            return Err("null client handle".into());
        }
        let resource = r
            .to_resource()
            .ok_or_else(|| format!("invalid resource (kind {}, index {})", r.kind, r.index))?;
        let ctx = &mut *(c as *mut Ctx);
        ctx.client.acquire(resource).map_err(|e| e.to_string())?;
        Ok(0)
    })
}

/// Drive the protocol: wait up to `timeout_ms` for one event and store it
/// in `*out`. `-1` blocks until an event or connection error, `0` polls
/// once, `> 0` waits that many milliseconds.
///
/// Returns `1` = an event was stored in `*out` (a `GRANTED` fd is owned by
/// the caller), `0` = nothing happened, `-1` = connection error.
#[unsafe(no_mangle)]
pub extern "C" fn sgc_pump(
    c: *mut sgc_client,
    timeout_ms: c_int,
    out: *mut sgc_event,
    err: *mut c_char,
    err_len: size_t,
) -> c_int {
    abi_int(err, err_len, || unsafe {
        if c.is_null() {
            return Err("null client handle".into());
        }
        if out.is_null() {
            return Err("null event pointer".into());
        }
        let timeout = match timeout_ms {
            -1 => None,
            n if n >= 0 => Some(Duration::from_millis(n as u64)),
            n => return Err(format!("invalid timeout_ms {n}")),
        };
        let ctx = &mut *(c as *mut Ctx);
        match ctx.client.pump(timeout).map_err(|e| e.to_string())? {
            Some(SgcEvent::Revoked { resource }) => {
                *out = sgc_event {
                    kind: SGC_EVENT_REVOKED,
                    resource: sgc_resource::from_resource(&resource),
                    fd: -1,
                };
                Ok(1)
            }
            Some(SgcEvent::Granted { resource, fd }) => {
                *out = sgc_event {
                    kind: SGC_EVENT_GRANTED,
                    resource: sgc_resource::from_resource(&resource),
                    fd: fd.into_raw_fd(),
                };
                Ok(1)
            }
            None => Ok(0),
        }
    })
}

/// Borrow `resource`: returns a dup of the held fd, owned by the caller
/// (close it). `-1` with an error message when the resource is not held.
#[unsafe(no_mangle)]
pub extern "C" fn sgc_fd(
    c: *mut sgc_client,
    r: sgc_resource,
    err: *mut c_char,
    err_len: size_t,
) -> c_int {
    abi_int(err, err_len, || unsafe {
        if c.is_null() {
            return Err("null client handle".into());
        }
        let resource = r
            .to_resource()
            .ok_or_else(|| format!("invalid resource (kind {}, index {})", r.kind, r.index))?;
        let ctx = &mut *(c as *mut Ctx);
        let fd = ctx.client.fd(&resource).map_err(|e| e.to_string())?;
        Ok(fd.into_raw_fd())
    })
}

/// Tear down the session and free the handle. `NULL` is a no-op; the
/// handle must not be used afterwards.
#[unsafe(no_mangle)]
pub extern "C" fn sgc_release(c: *mut sgc_client) {
    if c.is_null() {
        return;
    }
    // Safety: c came from sgc_connect and is consumed exactly once here.
    unsafe { drop(Box::from_raw(c as *mut Ctx)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(r: Resource) {
        assert_eq!(sgc_resource::from_resource(&r).to_resource(), Some(r));
    }

    #[test]
    fn kind_mapping_roundtrips_all_variants() {
        roundtrip(Resource::Fbdev);
        roundtrip(Resource::Drm { card: 0 });
        roundtrip(Resource::Drm { card: 255 });
        roundtrip(Resource::Input(InputResource::Mouse(0)));
        roundtrip(Resource::Input(InputResource::Mouse(255)));
        roundtrip(Resource::Input(InputResource::Keyboard(3)));
        roundtrip(Resource::Input(InputResource::Touch(200)));
    }

    #[test]
    fn kind_mapping_rejects_invalid_encodings() {
        // Unknown kind.
        assert!(sgc_resource { kind: 99, index: 0 }.to_resource().is_none());
        // Index out of u8 range / negative.
        assert!(
            sgc_resource {
                kind: SGC_RESOURCE_DRM,
                index: 256,
            }
            .to_resource()
            .is_none()
        );
        assert!(
            sgc_resource {
                kind: SGC_RESOURCE_MOUSE,
                index: -1,
            }
            .to_resource()
            .is_none()
        );
    }

    #[test]
    fn error_buffer_is_written_and_truncated() {
        let mut buf = [0i8; 8];
        unsafe {
            set_err(buf.as_mut_ptr(), buf.len(), "this message is long");
        }
        let s = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        assert_eq!(s, "this me");
        // Empty buffer: no write, no crash.
        unsafe { set_err(ptr::null_mut(), 0, "x") };
    }
}
