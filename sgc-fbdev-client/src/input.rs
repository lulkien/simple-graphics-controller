//! The input task: reads raw evdev events from the granted mouse fd and
//! logs them. Driven like the render task — `Start` hands it a fresh dup of
//! the mouse fd (client-ownership model), `Stop` drops it.
//!
//! The fd is a dup of the server's open of /dev/input/eventN, so there is
//! no path — events are parsed straight off the fd.
//!
//! No EVIOCGRAB: input events are broadcast to every open fd of the device,
//! and grabbing would steal them from the server and every other client.
//! Shared visibility is the point — like a compositor's pointer, every
//! session sees the same events.
//!
//! Layout: Linux `input_event` is timeval(16) + type u16 + code u16 +
//! value i32 = 24 bytes on 64-bit kernels (time_t is 64-bit; 32-bit
//! kernels have an 8-byte timeval). Values are NATIVE-endian — the kernel
//! and userspace share endianness, so `from_ne_bytes`, not `from_le_bytes`.

use std::{
    fs::File,
    io::{ErrorKind, Read},
    os::fd::{AsRawFd, OwnedFd},
    sync::mpsc,
    time::Duration,
};

/// Commands from main (the client's event loop) to the input task.
pub enum InputCmd {
    /// A grant (initial or re-grant): watch this mouse fd.
    Start { fd: OwnedFd },
    /// Revoked: stop watching and drop the fd.
    Stop,
}

const EVENT_SIZE: usize = 24;

// input_event types.
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;

// Codes we care about.
const SYN_REPORT: u16 = 0;
const REL_X: u16 = 0;
const REL_Y: u16 = 1;
const REL_WHEEL: u16 = 8;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;

/// Accumulated mouse state between SYN_REPORT packets.
struct MouseState {
    dx: i32,
    dy: i32,
    wheel: i32,
}

/// Input task: watches the mouse fd when granted, logs events. Reads are
/// non-blocking so the command channel stays live (recv_timeout loop).
pub fn run(rx: mpsc::Receiver<InputCmd>) {
    let mut fd: Option<File> = None;
    let mut bytes: Vec<u8> = Vec::new();
    let mut state = MouseState {
        dx: 0,
        dy: 0,
        wheel: 0,
    };

    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(cmd) => match cmd {
                InputCmd::Start { fd: new_fd } => {
                    let file = File::from(new_fd);
                    set_nonblocking(&file);
                    fd = Some(file);
                    println!("[input] watching mouse");
                }
                InputCmd::Stop => {
                    fd = None;
                    println!("[input] stopped");
                }
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if let Some(fd) = fd.as_mut() {
            drain(fd, &mut bytes, &mut state);
        }
    }
}

/// Drain whatever the fd has right now and parse complete events.
fn drain(fd: &mut File, bytes: &mut Vec<u8>, state: &mut MouseState) {
    let mut buf = [0u8; 1024];
    loop {
        match fd.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                bytes.extend_from_slice(&buf[..n]);
                parse_events(bytes, state);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) => {
                eprintln!("[input] read error: {e}");
                break;
            }
        }
    }
}

/// Parse and consume every complete 24-byte event in the buffer.
fn parse_events(bytes: &mut Vec<u8>, state: &mut MouseState) {
    let mut consumed = 0;
    while bytes.len() - consumed >= EVENT_SIZE {
        let event = &bytes[consumed..consumed + EVENT_SIZE];
        // type/code/value live in the last 8 bytes (offsets 16/18/20);
        // the 16-byte timeval is irrelevant for logging. Native-endian:
        // the kernel writes the host's endianness.
        let ty = u16::from_ne_bytes([event[16], event[17]]);
        let code = u16::from_ne_bytes([event[18], event[19]]);
        let value = i32::from_ne_bytes([event[20], event[21], event[22], event[23]]);
        handle_event(ty, code, value, state);
        consumed += EVENT_SIZE;
    }
    bytes.drain(..consumed);
}

fn handle_event(ty: u16, code: u16, value: i32, state: &mut MouseState) {
    match ty {
        EV_REL => match code {
            REL_X => state.dx += value,
            REL_Y => state.dy += value,
            REL_WHEEL => state.wheel += value,
            _ => {}
        },
        EV_KEY => {
            // value: 0 = release, 1 = press, 2 = auto-repeat (ignore).
            if value != 2 {
                let name = match code {
                    BTN_LEFT => "left",
                    BTN_RIGHT => "right",
                    BTN_MIDDLE => "middle",
                    _ => return,
                };
                println!("[input] {name} {}", if value != 0 { "down" } else { "up" });
            }
        }
        EV_SYN if code == SYN_REPORT => {
            // Packet boundary: flush the accumulated motion.
            if state.dx != 0 || state.dy != 0 {
                println!("[input] move: dx={} dy={}", state.dx, state.dy);
                state.dx = 0;
                state.dy = 0;
            }
            if state.wheel != 0 {
                println!("[input] scroll: {}", state.wheel);
                state.wheel = 0;
            }
        }
        _ => {}
    }
}

/// Make the fd non-blocking so reads never wedge the command channel.
fn set_nonblocking(file: &File) {
    unsafe {
        let flags = libc::fcntl(file.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
}
