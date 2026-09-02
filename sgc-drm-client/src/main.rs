//! Demo client for simple-graphics-controller on a DRM card.
//!
//! Acquires the FIRST `Display(Drm)` the server advertises (the best card,
//! per the server's priority ordering), then hands a dup of the granted fd
//! to the render task, which does a full KMS modeset + paint loop on it
//! (raw ioctls — see [`render`]).
//!
//! Same ownership model as the fbdev demo: the client HOLDS the canonical
//! fd; [`SgcClient::fd`] lends a dup; revoke drops the canonical and stops
//! the render task; re-grant hands a fresh dup.

mod render;

use std::{os::fd::OwnedFd, sync::mpsc, thread};

use libsgc_rs::{Resource, SgcClient, SgcError, SgcEvent};
use render::RenderCmd;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (mut client, available) = SgcClient::connect()?;

    // The first advertised DRM card is the best one (server priority order).
    let drm = available.iter().find_map(|resource| match resource {
        Resource::Drm { card } => Some(Resource::Drm { card: *card }),
        _ => None,
    });

    let Some(drm) = drm else {
        eprintln!("server does not offer a DRM card; available: {available:?}");
        return Err(SgcError::NotAvailable {
            resource: Resource::Drm { card: 0 },
        }
        .into());
    };

    let fd = acquire_display_fd(&mut client, drm.clone())?;

    // The render task owns the dup and runs the modeset + paint loop; the
    // client keeps the canonical.
    let (render_tx, render_rx) = mpsc::channel();
    let render_handle = thread::spawn(move || render::run(render_rx));

    render_tx.send(RenderCmd::Draw {
        resource: drm.clone(),
        fd,
    })?;

    // The client's event loop: revoke -> stop the render task (it drops the
    // dup and tears the display down), re-grant -> fresh dup + redraw.
    client.start_event_loop(|event| match event {
        SgcEvent::Revoked { resource } => {
            let _ = render_tx.send(RenderCmd::Stop { resource });
        }
        SgcEvent::Granted { resource, fd } => {
            let _ = render_tx.send(RenderCmd::Draw { resource, fd });
        }
    });

    let _ = render_tx.send(RenderCmd::Exit);
    drop(render_tx);
    let _ = render_handle.join();

    Ok(())
}

fn acquire_display_fd(client: &mut SgcClient, resource: Resource) -> Result<OwnedFd, SgcError> {
    match client.acquire(resource.clone()) {
        Ok(()) => println!("granted {resource:?} (client holds fd)"),
        Err(e) => {
            eprintln!("acquire failed: {e}");
            return Err(e);
        }
    }

    // Borrow a dup for the render task; the client keeps the canonical.
    match client.fd(&resource) {
        Ok(fd) => Ok(fd),
        Err(e) => {
            eprintln!("lend failed: {e}");
            Err(e)
        }
    }
}
