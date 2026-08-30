//! The render task: owns the renderer (framebuffer + scene) and the
//! granted fd. It is driven by [`RenderCmd`] messages from the main
//! thread's event loop — the message-passing mechanism between threads.

use std::{os::fd::OwnedFd, sync::mpsc};

use linfb::{
    Compositor, Framebuffer,
    shape::{Caption, Color, FontBuilder, Rectangle, Shape},
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
        let mut compositor = framebuffer.compositor((40, 40, 40).into());

        // A 5x5 checkerboard of 100px squares, alternating two colors.
        let tile = 100usize;
        let origin = (150usize, 100usize);
        for row in 0..5 {
            for col in 0..5 {
                let color = if (row + col) % 2 == 0 {
                    Color::hex("#ffaa0099").unwrap()
                } else {
                    Color::hex("#0055ff99").unwrap()
                };
                compositor.add(
                    &format!("tile_{row}_{col}"),
                    Rectangle::builder()
                        .width(tile)
                        .height(tile)
                        .fill_color(color)
                        .build()
                        .unwrap()
                        .at(origin.0 + col * tile, origin.1 + row * tile),
                );
            }
        }

        // A caption under the board.
        compositor.add(
            "caption",
            Caption::builder()
                .text("sample 2: checkerboard".into())
                .size(48)
                .color(Color::hex("#ffffffff").unwrap())
                .font(FontBuilder::default().family("monospace").build().unwrap())
                .build()
                .unwrap()
                .at(origin.0, origin.1 + 5 * tile + 40),
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
