//! The render task: full KMS modeset on the granted DRM card fd via raw
//! libc ioctls (no libdrm, no C deps), then a single-buffer paint loop.
//!
//! Flow: `GETRESOURCES` -> `GETCONNECTOR` -> `GETENCODER` -> pick a mode ->
//! `CREATE_DUMB` + `MAP_DUMB` + `mmap` -> `ADDFB` -> paint -> `SETCRTC`.
//! One dumb buffer, painted in place every frame — no page flip; tearing is
//! fine for a demo, and a flip path can layer on later.
//!
//! The fd arrives as a DUP lent by the client (client-ownership model): the
//! renderer owns its dup and tears the display down (`RMFB` + `munmap`) on
//! drop — revoke, exit, or any error after the framebuffer exists.
//!
//! Requires a DRM lease fd: the server holds DRM master and grants each
//! client a lease over the card's objects, so the ioctls below work within
//! the lease. The server can revoke the lease at any time.

use std::{
    io,
    mem::size_of,
    os::fd::{AsRawFd, OwnedFd, RawFd},
    ptr,
    sync::mpsc,
    time::Duration,
};

use libsgc_rs::Resource;

pub const DRAW_INTERVAL: Duration = Duration::from_millis(33);

/// Commands from main (the client's event loop) to the render task.
pub enum RenderCmd {
    /// A grant (initial or re-grant): modeset and draw with this fd.
    Draw { resource: Resource, fd: OwnedFd },
    /// Revoked: stop rendering; dropping the renderer tears the display down.
    Stop { resource: Resource },
    /// Main is done (connection lost): exit the loop.
    Exit,
}

// ---------------------------------------------------------------------------
// DRM ioctl plumbing (ioctl numbers are built like the kernel's _IOC macro:
// dir(30) | type(8) | nr(0) | size(16), type = 'd').
// ---------------------------------------------------------------------------

fn drm_ioctl(dir: u32, nr: u32, size: usize) -> libc::c_ulong {
    ((dir as u64) << 30) | (('d' as u64) << 8) | (nr as u64) | ((size as u64) << 16)
}

/// `DRM_IOWR` ioctl (kernel writes the struct back).
unsafe fn ioctl_wr<T>(fd: RawFd, nr: u32, arg: &mut T) -> io::Result<()> {
    let req = drm_ioctl(3, nr, size_of::<T>());
    if unsafe { libc::ioctl(fd, req, arg) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// `DRM_IOW` ioctl (write-only).
unsafe fn ioctl_w<T>(fd: RawFd, nr: u32, arg: &T) -> io::Result<()> {
    let req = drm_ioctl(1, nr, size_of::<T>());
    if unsafe { libc::ioctl(fd, req, arg) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Kernel structs (linux/drm_mode.h, repr(C), 64-bit layout).
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Default)]
struct ModeCardRes {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

#[repr(C)]
#[derive(Default, Clone)]
struct ModeModeInfo {
    clock: u32,
    hdisplay: u16,
    hsync_start: u16,
    hsync_end: u16,
    htotal: u16,
    hskew: u16,
    vdisplay: u16,
    vsync_start: u16,
    vsync_end: u16,
    vtotal: u16,
    vscan: u16,
    vrefresh: u32,
    flags: u32,
    type_: u32,
    name: [u8; 32],
}

#[repr(C)]
#[derive(Default)]
struct ModeGetConnector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: u32,
    count_props: u32,
    count_encoders: u32,
    encoder_id: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct ModeGetEncoder {
    encoder_id: u32,
    encoder_type: u32,
    crtc_id: u32,
    possible_crtcs: u32,
    possible_clones: u32,
}

#[repr(C)]
#[derive(Default)]
struct ModeCreateDumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

#[repr(C)]
#[derive(Default)]
struct ModeMapDumb {
    handle: u32,
    pad: u32,
    offset: u64,
}

#[repr(C)]
#[derive(Default)]
struct ModeFbCmd {
    fb_id: u32,
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u32,
    depth: u32,
    handle: u32,
}

#[repr(C)]
#[derive(Default)]
struct ModeCrtc {
    set_connectors_ptr: u64,
    count_connectors: u32,
    crtc_id: u32,
    fb_id: u32,
    x: u32,
    y: u32,
    gamma_size: u32,
    mode_valid: u32,
    mode: ModeModeInfo,
}

// ---------------------------------------------------------------------------
// The renderer.
// ---------------------------------------------------------------------------

/// A live modeset on the granted card: fb id, dumb-buffer mapping, and the
/// active mode. Drop tears the display down (`RMFB` + `munmap`; the dumb BO
/// dies with the fd).
pub struct Renderer {
    fd: OwnedFd,
    fb_id: u32,
    map: *mut u8,
    map_len: usize,
    pitch: u32,
    width: u32,
    height: u32,
    frame: u64,
}

impl Renderer {
    /// Modeset the first usable output of the card and show the first frame.
    pub fn from_fd(fd: OwnedFd) -> Result<Self, Box<dyn std::error::Error>> {
        let raw = fd.as_raw_fd();
        let (crtc_id, connector_id, mode) = find_output(raw)?;
        let (fb_id, map, map_len, pitch) = create_framebuffer(raw, &mode)?;

        let mut renderer = Self {
            fd,
            fb_id,
            map,
            map_len,
            pitch,
            width: mode.hdisplay as u32,
            height: mode.vdisplay as u32,
            frame: 0,
        };

        // Paint before committing so the screen comes up already drawn.
        renderer.draw();
        renderer.set_crtc(raw, crtc_id, connector_id, &mode)?;
        println!(
            "[render] modeset {}x{} on connector {connector_id} (crtc {crtc_id})",
            mode.hdisplay, mode.vdisplay
        );
        Ok(renderer)
    }

    /// Paint one frame: eight color bars + a moving white square.
    pub fn draw(&mut self) {
        const BARS: [u32; 8] = [
            0x00FF_0000, // red
            0x0000_FF00, // green
            0x0000_00FF, // blue
            0x00FF_FFFF, // white
            0x0000_0000, // black
            0x00FF_FF00, // yellow
            0x0000_FFFF, // cyan
            0x00FF_00FF, // magenta
        ];

        let (w, h, pitch) = (
            self.width as usize,
            self.height as usize,
            self.pitch as usize,
        );
        let base = self.map;

        for y in 0..h {
            let row = unsafe { base.add(y * pitch) } as *mut u32;
            for x in 0..w {
                let bar = x * BARS.len() / w;
                unsafe { *row.add(x) = BARS[bar] };
            }
        }

        // A white square sweeping left to right.
        let size = 64usize;
        let max_x = w.saturating_sub(size);
        let x0 = (self.frame as usize * 16) % max_x.max(1);
        let y0 = (h.saturating_sub(size)) / 2;
        for y in y0..(y0 + size).min(h) {
            let row = unsafe { base.add(y * pitch) } as *mut u32;
            for x in x0..(x0 + size).min(w) {
                unsafe { *row.add(x) = 0x00FF_FFFF };
            }
        }

        self.frame += 1;
    }

    /// Legacy `SETCRTC`: commit the fb to the crtc with the connector.
    fn set_crtc(
        &self,
        raw: RawFd,
        crtc_id: u32,
        connector_id: u32,
        mode: &ModeModeInfo,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut crtc = ModeCrtc {
            crtc_id,
            fb_id: self.fb_id,
            count_connectors: 1,
            mode: mode.clone(),
            mode_valid: 1,
            ..Default::default()
        };
        crtc.set_connectors_ptr = (&connector_id as *const u32) as u64;
        unsafe { ioctl_wr(raw, 0xA2, &mut crtc) }.map_err(|e| format!("SETCRTC: {e}"))?;
        Ok(())
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        let raw = self.fd.as_raw_fd();
        // Remove the fb first (kernel stops scanning out of it), then unmap.
        let cmd = ModeFbCmd {
            fb_id: self.fb_id,
            ..Default::default()
        };
        let _ = unsafe { ioctl_w(raw, 0xAF, &cmd) }; // RMFB
        unsafe { libc::munmap(self.map as *mut libc::c_void, self.map_len) };
        println!("[render] display torn down (fb {} unmapped)", self.fb_id);
    }
}

// ---------------------------------------------------------------------------
// Discovery: card resources -> connector -> encoder -> mode.
// ---------------------------------------------------------------------------

/// Find a usable output: the crtc, connector, and mode to commit. Prefers a
/// CONNECTED connector with modes; falls back to any connector with modes.
fn find_output(fd: RawFd) -> Result<(u32, u32, ModeModeInfo), Box<dyn std::error::Error>> {
    let mut res = ModeCardRes::default();
    unsafe { ioctl_wr(fd, 0xA0, &mut res) }.map_err(|e| format!("GETRESOURCES: {e}"))?;
    if res.count_crtcs == 0 || res.count_connectors == 0 {
        return Err("card has no CRTCs or connectors".into());
    }

    let mut crtc_ids = vec![0u32; res.count_crtcs as usize];
    let mut connector_ids = vec![0u32; res.count_connectors as usize];
    // The kernel writes fb/encoder ids into their pointers; we only need
    // crtcs and connectors, so zero the other counts to skip those writes
    // (the pointers are null and would EFAULT).
    res.count_fbs = 0;
    res.count_encoders = 0;
    res.crtc_id_ptr = crtc_ids.as_mut_ptr() as u64;
    res.connector_id_ptr = connector_ids.as_mut_ptr() as u64;
    unsafe { ioctl_wr(fd, 0xA0, &mut res) }.map_err(|e| format!("GETRESOURCES (fill): {e}"))?;

    let mut connected: Option<(u32, u32, ModeModeInfo)> = None;
    let mut fallback: Option<(u32, u32, ModeModeInfo)> = None;

    for &connector_id in &connector_ids {
        let mut conn = ModeGetConnector {
            connector_id,
            ..Default::default()
        };
        unsafe { ioctl_wr(fd, 0xA7, &mut conn) }.map_err(|e| format!("GETCONNECTOR: {e}"))?;
        if conn.count_modes == 0 {
            continue; // e.g. writeback connectors — no display modes
        }

        let mut modes = vec![ModeModeInfo::default(); conn.count_modes as usize];
        let mut encoders = vec![0u32; conn.count_encoders as usize];
        // The kernel also writes property ids/values; we don't need them,
        // so zero the count to skip those writes (the pointers are null).
        conn.count_props = 0;
        conn.modes_ptr = modes.as_mut_ptr() as u64;
        conn.encoders_ptr = encoders.as_mut_ptr() as u64;
        unsafe { ioctl_wr(fd, 0xA7, &mut conn) }
            .map_err(|e| format!("GETCONNECTOR (fill): {e}"))?;

        let mode = pick_mode(&modes);
        let candidate = (connector_id, conn.encoder_id, mode);
        if conn.connection == 1 {
            connected = Some(candidate);
            break;
        }
        if fallback.is_none() {
            fallback = Some(candidate);
        }
    }

    let (connector_id, encoder_id, mode) =
        connected.or(fallback).ok_or("no connector with modes")?;

    // The encoder's current crtc (if any), else the card's first crtc.
    let mut enc = ModeGetEncoder {
        encoder_id,
        ..Default::default()
    };
    unsafe { ioctl_wr(fd, 0xA6, &mut enc) }.map_err(|e| format!("GETENCODER: {e}"))?;
    let crtc_id = if enc.crtc_id != 0 {
        enc.crtc_id
    } else {
        crtc_ids[0]
    };

    Ok((crtc_id, connector_id, mode))
}

/// The preferred mode if the connector marks one, else the first.
fn pick_mode(modes: &[ModeModeInfo]) -> ModeModeInfo {
    modes
        .iter()
        .find(|m| m.type_ & 0x80 != 0) // DRM_MODE_TYPE_PREFERRED
        .cloned()
        .unwrap_or_else(|| modes[0].clone())
}

/// Create the dumb buffer (32bpp XRGB8888), mmap it, and register it as an
/// fb. Returns (fb_id, mapping, mapping length, pitch).
fn create_framebuffer(
    fd: RawFd,
    mode: &ModeModeInfo,
) -> Result<(u32, *mut u8, usize, u32), Box<dyn std::error::Error>> {
    let mut dumb = ModeCreateDumb {
        width: mode.hdisplay as u32,
        height: mode.vdisplay as u32,
        bpp: 32,
        ..Default::default()
    };
    unsafe { ioctl_wr(fd, 0xB2, &mut dumb) }.map_err(|e| format!("CREATE_DUMB: {e}"))?;

    let mut map = ModeMapDumb {
        handle: dumb.handle,
        ..Default::default()
    };
    unsafe { ioctl_wr(fd, 0xB3, &mut map) }.map_err(|e| format!("MAP_DUMB: {e}"))?;

    let ptr = unsafe {
        libc::mmap(
            ptr::null_mut(),
            dumb.size as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            map.offset as libc::off_t,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(format!("mmap: {}", io::Error::last_os_error()).into());
    }

    let mut fb = ModeFbCmd {
        width: dumb.width,
        height: dumb.height,
        pitch: dumb.pitch,
        bpp: 32,
        depth: 24, // XRGB8888: depth 24, bpp 32
        handle: dumb.handle,
        ..Default::default()
    };
    unsafe { ioctl_wr(fd, 0xAE, &mut fb) }.map_err(|e| format!("ADDFB: {e}"))?;

    Ok((fb.fb_id, ptr as *mut u8, dumb.size as usize, dumb.pitch))
}

// ---------------------------------------------------------------------------
// The task loop.
// ---------------------------------------------------------------------------

/// Render task: modeset on the first granted fd, then repaint every
/// [`DRAW_INTERVAL`] until Stop/Exit. A re-grant after a revoke just
/// modesets again (the renderer was dropped on Stop, tearing down the
/// previous display).
pub fn run(rx: mpsc::Receiver<RenderCmd>) {
    let mut renderer: Option<Renderer> = None;

    loop {
        match rx.recv_timeout(DRAW_INTERVAL) {
            Ok(RenderCmd::Draw { resource, fd }) => {
                if renderer.is_none() {
                    match Renderer::from_fd(fd) {
                        Ok(r) => {
                            println!("[render] drawing {resource:?}");
                            renderer = Some(r);
                        }
                        Err(e) => eprintln!("[render] modeset failed: {e}"),
                    }
                }
            }
            Ok(RenderCmd::Stop { resource }) => {
                renderer = None;
                println!("[render] stopped {resource:?}");
            }
            Ok(RenderCmd::Exit) => {
                println!("[render] exited");
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(r) = renderer.as_mut() {
                    r.draw();
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}
