//! Demo client for simple-graphics-controller.
//!
//! Architecture:
//!
//! - main thread: drives the [`SgcClient`] — connect, acquire (blocking),
//!   then the client's own event loop until the connection ends;
//! - render task ([`render`]): owns the renderer (framebuffer + scene),
//!   built from the granted fd; driven by [`render::RenderCmd`] messages
//!   sent by the client's event loop.
//!
//! Flow: connect -> check the advertised resources -> acquire (blocking)
//! -> spawn render task, send it the fd -> client event loop (Revoke ->
//! Stop + revoke-ack, re-Grant -> Draw with the new fd, disconnect ->
//! stop) -> wait for the render task. The app keeps rendering until
//! killed: the server revokes on preemption, re-grants later, and frees
//! the resource on our disconnect when we die.

mod client;
mod error;
mod render;

use std::{sync::mpsc, thread};

use client::SgcClient;
use error::SgcError;
use render::RenderCmd;
use simple_graphics_protocol::Resource;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (mut client, available) = SgcClient::connect()?;

    // Fail fast if the server does not offer Fbdev (e.g. no /dev/fb0 on
    // the host): an acquire would only be denied with a round trip.
    if !available.contains(&Resource::Fbdev) {
        eprintln!("server does not offer Fbdev; available: {available:?}");
        return Ok(());
    }

    // Got the fd from the server (blocking; queued requests block here).
    let fd = match client.acquire(Resource::Fbdev) {
        Ok(fd) => fd,
        Err(SgcError::Denied { reason }) => {
            eprintln!("denied: {reason}");
            return Ok(());
        }
        Err(e) => {
            eprintln!("acquire failed: {e}");
            return Ok(());
        }
    };
    println!("granted Fbdev");

    // Send the fd to the render task; it builds the renderer from it
    // (linfb's scene graph is !Send, so it must be created there).
    let (render_tx, render_rx) = mpsc::channel();
    let render = thread::spawn(move || render::run(render_rx));
    render_tx.send(RenderCmd::Draw(fd))?;

    // The client's event loop: revoke -> stop render, re-grant -> draw,
    // disconnect -> stop. Blocks until the connection ends.
    client.start_event_loop(&render_tx);

    let _ = render_tx.send(RenderCmd::Stop);
    drop(render_tx);
    let _ = render.join();
    Ok(())
}
