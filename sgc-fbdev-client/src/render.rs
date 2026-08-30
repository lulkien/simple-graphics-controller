//! The render task: owns the renderer (framebuffer + scene) and the
//! granted fd. It is driven by [`RenderCmd`] messages from the main
//! thread's event loop — the message-passing mechanism between threads.

use std::{os::fd::OwnedFd, sync::mpsc};

use linfb::{
    Compositor, Framebuffer,
    shape::{Alignment, Caption, Color, FontBuilder, Rectangle, Shape},
};

/// Commands from the client's event loop to the render task.
pub enum RenderCmd {
    /// A grant (initial or re-grant): draw with this fd.
    Draw(OwnedFd),
    /// Revoked: stop rendering and drop the granted fd.
    Stop,
}

/// The renderer: framebuffer + scene, created ONCE per app run from the
/// first granted fd and owned by the render task. Opening the framebuffer
/// (ioctls + mmap) and building the scene (font loading) are the expensive
/// parts, so they happen exactly once. The mmap stays valid across
/// re-grants (same device), so a revoked-then-regranted cycle just
/// redraws.
pub struct Renderer {
    framebuffer: Framebuffer,
    compositor: Compositor,
}

impl Renderer {
    /// Open the framebuffer and build the scene from the granted fd.
    /// linfb takes ownership of the fd it is given (File::from_raw_fd), so
    /// it gets a dup — the caller keeps the granted fd.
    pub fn from_fd(fd: &OwnedFd) -> Result<Self, Box<dyn std::error::Error>> {
        use std::os::fd::IntoRawFd;

        let dup = fd.try_clone()?;
        let framebuffer =
            Framebuffer::open_with_fd(dup.into_raw_fd()).expect("framebuffer open failed");
        let mut compositor = framebuffer.compositor((255, 255, 255).into());
        compositor
            .add(
                "rect1",
                Rectangle::builder()
                    .width(100)
                    .height(100)
                    .fill_color(Color::hex("#ff000099").unwrap())
                    .build()
                    .unwrap()
                    .at(100, 100),
            )
            .add(
                "rect2",
                Rectangle::builder()
                    .width(100)
                    .height(100)
                    .fill_color(Color::hex("#00ff0099").unwrap())
                    .build()
                    .unwrap()
                    .at(150, 150),
            )
            .add(
                "wrapped_text",
                Caption::builder()
                    .text("Some centered text\nwith newlines".into())
                    .size(56)
                    .color(Color::hex("#4066b877").unwrap())
                    .font(FontBuilder::default().family("monospace").build().unwrap())
                    .alignment(Alignment::Center)
                    .max_width(650)
                    .build()
                    .unwrap()
                    .at(1000, 300),
            );
        Ok(Self {
            framebuffer,
            compositor,
        })
    }

    /// Redraw the cached scene.
    pub fn draw(&mut self) {
        self.framebuffer.draw(0, 0, &self.compositor);
        self.framebuffer.flush();
    }
}

/// Render task: owns the renderer and the granted fd; redraws on `Draw`,
/// drops the fd on `Stop`. Runs until the channel closes (main is gone),
/// then drops the renderer (closes the framebuffer).
///
/// The renderer is created HERE, inside the task, from the first granted
/// fd — linfb's scene graph is `!Send` (`Box<dyn Shape>`), so it cannot be
/// moved across threads. Created once per app run; kept across revokes
/// (the mmap stays valid — same device — so a re-grant just redraws).
pub fn run(rx: mpsc::Receiver<RenderCmd>) {
    let mut renderer: Option<Renderer> = None;
    let mut current: Option<OwnedFd> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            RenderCmd::Draw(fd) => {
                if renderer.is_none() {
                    match Renderer::from_fd(&fd) {
                        Ok(renderer_) => renderer = Some(renderer_),
                        Err(e) => {
                            eprintln!("[render] open failed: {e}");
                            continue;
                        }
                    }
                }
                current = Some(fd);
                if let Some(renderer) = &mut renderer {
                    renderer.draw();
                }
            }
            RenderCmd::Stop => {
                current.take(); // disown the granted fd
                println!("[render] stopped");
            }
        }
    }
}
