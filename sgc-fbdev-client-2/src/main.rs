//! Demo client for simple-graphics-controller — CLIENT-OWNERSHIP variant.
//!
//! Same app skeleton as `sgc-fbdev-client` (connect, check the advertised
//! resources, acquire, spawn a render task, event loop), but the client
//! HOLDS the granted fd and LENDS a dup to the render task:
//!
//! - `SgcClient::acquire` stores the canonical fd (client-owned);
//! - `SgcClient::fd` returns a fresh dup for the borrower;
//! - revoke drops the canonical and tells the render task to stop;
//! - re-grant stores the new canonical and lends a fresh dup.
//!
//! This is the prototype for the multi-resource future (one held fd per
//! resource, one owner of the truth: the client). `sgc-fbdev-client` keeps
//! the simpler move-based design.

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

    // Request the resource (blocking; queued requests block here). The
    // client now owns the canonical fd.
    match client.acquire(Resource::Fbdev) {
        Ok(()) => println!("granted Fbdev (client holds fd)"),
        Err(SgcError::Denied { reason }) => {
            eprintln!("denied: {reason}");
            return Ok(());
        }
        Err(e) => {
            eprintln!("acquire failed: {e}");
            return Ok(());
        }
    };
    println!("held: {:?}", client.held());

    // Borrow a dup for the render task; the client keeps the canonical.
    let fd = match client.fd(&Resource::Fbdev) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("lend failed: {e}");
            return Ok(());
        }
    };

    // Send the fd to the render task; it builds the renderer from it
    // (linfb's scene graph is !Send, so it must be created there).
    let (render_tx, render_rx) = mpsc::channel();
    let render = thread::spawn(move || render::run(render_rx));
    render_tx.send(RenderCmd::Draw {
        resource: Resource::Fbdev,
        fd,
    })?;

    // The client's event loop: revoke -> drop canonical + stop render,
    // re-grant -> store canonical + draw with a fresh dup, disconnect ->
    // disown everything. Blocks until the connection ends.
    client.start_event_loop(&render_tx);

    let _ = render_tx.send(RenderCmd::Stop {
        resource: Resource::Fbdev,
    });
    drop(render_tx);
    let _ = render.join();
    Ok(())
}
