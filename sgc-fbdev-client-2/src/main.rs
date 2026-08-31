//! Demo client for simple-graphics-controller — built on `libsgc-rs`.
//!
//! The client HOLDS the granted fd (client-ownership) and LENDS a dup to
//! the render task:
//!
//! - `SgcClient::acquire` stores the canonical fd (client-owned);
//! - `SgcClient::fd` returns a fresh dup for the borrower;
//! - revoke drops the canonical and tells the render task to stop;
//! - re-grant stores the new canonical and lends a fresh dup.
//!
//! Multi-resource ready: one held fd per resource, one owner of the
//! truth — the client. Renders a different scene than `sgc-fbdev-client`
//! (checkerboard instead of rects + wrapped text).

mod render;

use std::{sync::mpsc, thread};

use libsgc_rs::{Resource, SgcClient, SgcError, SgcEvent};
use render::RenderCmd;

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
    client.start_event_loop(|event| match event {
        SgcEvent::Revoked { resource } => {
            let _ = render_tx.send(RenderCmd::Stop { resource });
        }
        SgcEvent::Granted { resource, fd } => {
            let _ = render_tx.send(RenderCmd::Draw { resource, fd });
        }
    });

    let _ = render_tx.send(RenderCmd::Stop {
        resource: Resource::Fbdev,
    });
    drop(render_tx);
    let _ = render.join();
    Ok(())
}
