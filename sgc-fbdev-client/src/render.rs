//! The render task: owns the renderer (framebuffer + motion state) and the
//! borrowed fd, and runs the LVGL-style timer system:
//!
//! - logic timer (16ms): advances the animation state;
//! - draw timer (33ms): repaints the framebuffer.
//!
//! Both timers live in [`TimerList`] and fire on this thread; the channel
//! from main (`Draw`/`Stop`/`Exit`) and the timer deadlines wake the same
//! `recv_timeout` loop — one loop, one wakeup source, like `lv_timer_handler`.
//!
//! The fd arrives as a DUP lent by the client (client-ownership model):
//! the render task owns its dup until `Stop`, when it drops it.

use std::{
    os::fd::{IntoRawFd, OwnedFd},
    sync::mpsc,
    time::{Duration, Instant},
};

use libsgc_rs::Resource;
use linfb::{
    Framebuffer,
    shape::{Rectangle, Shape},
};

use crate::logic::{LOGIC_INTERVAL, Motion};
use crate::timer::TimerList;

pub const RECT_WIDTH: i32 = 100;
pub const RECT_HEIGHT: i32 = 100;

pub const DRAW_INTERVAL: Duration = Duration::from_millis(33);

/// Commands from main (the client's event loop) to the render task.
pub enum RenderCmd {
    /// A grant (initial or re-grant): draw with this fd.
    Draw { resource: Resource, fd: OwnedFd },
    /// Revoked: stop rendering and drop the borrowed fd.
    Stop { resource: Resource },
    /// Main is done (connection lost): drop the fd and exit the loop.
    Exit,
}

/// The renderer: framebuffer + motion state. Opening the framebuffer
/// (ioctls + mmap) is the expensive part, so it happens exactly once; the
/// mmap stays valid across re-grants (same device), so a
/// revoked-then-regranted cycle just redraws.
pub struct Renderer {
    framebuffer: Framebuffer,
    motion: Motion,
}

impl Renderer {
    /// Open the framebuffer from the granted fd. linfb takes ownership of
    /// the fd it is given (File::from_raw_fd), so it gets a dup — the
    /// caller keeps the granted fd.
    pub fn from_fd(fd: &OwnedFd) -> Result<Self, Box<dyn std::error::Error>> {
        let dup = fd.try_clone()?;
        let framebuffer =
            Framebuffer::open_with_fd(dup.into_raw_fd()).expect("framebuffer open failed");

        let screen_info = framebuffer.screen_info.clone();

        Ok(Self {
            framebuffer,
            motion: Motion::new(
                screen_info.xres as i32,
                screen_info.yres as i32,
                RECT_WIDTH,
                RECT_HEIGHT,
            ),
        })
    }

    /// One logic tick (16ms): advance the animation state.
    pub fn tick(&mut self) {
        let _ = self.motion.tick();
    }

    /// One draw (33ms): paint the rect at the current position.
    pub fn draw(&mut self) {
        let (x, y) = self.motion.position();
        self.draw_at(x, y);
    }

    fn draw_at(&mut self, x: i32, y: i32) {
        let mut compositor = self.framebuffer.compositor((255, 255, 255).into());

        compositor.add(
            "rect1",
            Rectangle::builder()
                .width(RECT_WIDTH as usize)
                .height(RECT_HEIGHT as usize)
                .fill_color(self.motion.color())
                .build()
                .unwrap()
                .at(x as usize, y as usize),
        );

        self.framebuffer.draw(0, 0, &compositor);
        self.framebuffer.flush();
    }
}

/// Everything the timers and commands touch: the renderer (created lazily
/// from the first granted fd) and the granted fd (None while revoked).
struct RenderState {
    renderer: Option<Renderer>,
    current_fd: Option<OwnedFd>,
}

/// Render task: runs the timer system and the command loop until the
/// channel closes. The logic timer keeps advancing the animation even
/// while revoked (background work); only drawing stops without a grant.
pub fn run(rx: mpsc::Receiver<RenderCmd>) {
    let mut state = RenderState {
        renderer: None,
        current_fd: None,
    };

    let mut timers: TimerList<RenderState> = TimerList::new();

    timers.register(LOGIC_INTERVAL, |s| {
        if let Some(renderer) = s.renderer.as_mut() {
            renderer.tick();
        }
    });

    timers.register(DRAW_INTERVAL, |s| {
        if s.current_fd.is_some()
            && let Some(renderer) = s.renderer.as_mut()
        {
            renderer.draw();
        }
    });

    loop {
        // Wait for a command, but no longer than the earliest timer
        // deadline — timer fires and commands wake the same loop.
        let timeout = timers
            .next_deadline()
            .map(|d| d.saturating_duration_since(Instant::now()));

        let result = match timeout {
            Some(t) => rx.recv_timeout(t),
            None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };

        match result {
            Ok(cmd) => match cmd {
                RenderCmd::Draw { resource, fd } => {
                    if state.renderer.is_none() {
                        match Renderer::from_fd(&fd) {
                            Ok(value) => state.renderer = Some(value),
                            Err(error) => {
                                eprintln!("[render] framebuffer open failed: {error}");
                                continue;
                            }
                        }
                    }

                    state.current_fd = Some(fd);

                    if let Some(renderer) = state.renderer.as_mut() {
                        renderer.draw();
                    }

                    println!("[render] drawing {resource:?}");
                }
                RenderCmd::Stop { resource } => {
                    state.current_fd.take();
                    println!("[render] stopped {resource:?}");
                }
                RenderCmd::Exit => {
                    state.current_fd.take();
                    println!("[render] exited");
                    break;
                }
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {
                timers.fire_due(Instant::now(), &mut state);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}
