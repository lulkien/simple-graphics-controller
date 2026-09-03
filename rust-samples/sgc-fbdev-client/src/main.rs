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
//! truth — the client.

mod input;
mod logic;
mod render;
mod timer;

use std::{os::fd::OwnedFd, sync::mpsc, thread};

use input::InputCmd;
use libsgc_rs::{InputResource, Resource, SgcClient, SgcError, SgcEvent};
use render::RenderCmd;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (mut client, available) = SgcClient::connect()?;

    // Request the resource (blocking; queued requests block here). The
    // client now owns the canonical fd.
    let fbdev_fd = acquire_display_fd(&mut client, &available)?;

    // Send the fd to the render task; it builds the renderer from it
    // (linfb's scene graph is !Send, so it must be created there) and runs
    // the timer system: logic timer (16ms) + draw timer (33ms).
    let (render_tx, render_rx) = mpsc::channel();
    let render_handle = thread::spawn(move || render::run(render_rx));

    // Initial grant.
    render_tx.send(RenderCmd::Draw {
        resource: Resource::Fbdev,
        fd: fbdev_fd,
    })?;

    // The first mouse the server offers, if any. Acquire it and hand the
    // fd to the input task, which logs mouse events.
    let mouse = available.iter().find_map(|resource| match resource {
        Resource::Input(InputResource::Mouse(index)) => {
            Some(Resource::Input(InputResource::Mouse(*index)))
        }
        _ => None,
    });

    let (input_tx, input_rx) = mpsc::channel();
    let input_handle = match mouse.clone() {
        Some(mouse) => match client.acquire(mouse.clone()) {
            Ok(()) => {
                println!("granted {mouse:?}");
                match client.fd(&mouse) {
                    Ok(fd) => {
                        let input = thread::spawn(move || input::run(input_rx));
                        input_tx.send(InputCmd::Start { fd })?;
                        Some(input)
                    }
                    Err(e) => {
                        eprintln!("input lend failed: {e}");
                        None
                    }
                }
            }
            Err(SgcError::Denied { reason }) => {
                eprintln!("mouse denied: {reason}");
                None
            }
            Err(e) => {
                eprintln!("mouse acquire failed: {e}");
                None
            }
        },
        None => {
            println!("no mouse advertised; skipping input");
            None
        }
    };

    // The client's event loop: revoke -> drop canonical + stop the owner
    // task, re-grant -> store canonical + hand a fresh dup to the owner
    // task, disconnect -> disown everything. Blocks until the connection
    // ends. Events are routed by resource (display -> render, mouse ->
    // input).
    client.start_event_loop(|event| match event {
        SgcEvent::Revoked { resource } => {
            if Some(&resource) == mouse.as_ref() {
                println!("revoked {resource:?}; stopping input");
                let _ = input_tx.send(InputCmd::Stop);
            } else {
                println!("revoked {resource:?}; stopping render");
                let _ = render_tx.send(RenderCmd::Stop { resource });
            }
        }
        SgcEvent::Granted { resource, fd } => {
            if Some(&resource) == mouse.as_ref() {
                println!("re-granted {resource:?}; reading input");
                let _ = input_tx.send(InputCmd::Start { fd });
            } else {
                println!("re-granted {resource:?}; drawing");
                let _ = render_tx.send(RenderCmd::Draw { resource, fd });
            }
        }
    });

    let _ = render_tx.send(RenderCmd::Exit);
    let _ = input_tx.send(InputCmd::Stop);

    drop(render_tx);
    drop(input_tx);

    let _ = render_handle.join();
    if let Some(input_handle) = input_handle {
        let _ = input_handle.join();
    }

    Ok(())
}

fn acquire_display_fd(client: &mut SgcClient, available: &[Resource]) -> Result<OwnedFd, SgcError> {
    // Fail fast if the server does not offer Fbdev (e.g. no /dev/fb0 on
    // the host): an acquire would only be denied with a round trip.
    if !available.contains(&Resource::Fbdev) {
        eprintln!("server does not offer Fbdev; available: {available:?}");
        return Err(SgcError::NotAvailable {
            resource: Resource::Fbdev,
        });
    }

    match client.acquire(Resource::Fbdev) {
        Ok(()) => println!("granted Fbdev (client holds fd)"),
        Err(SgcError::Denied { reason }) => {
            eprintln!("denied: {reason}");
            return Err(SgcError::Denied { reason });
        }
        Err(e) => {
            eprintln!("acquire failed: {e}");
            return Err(e);
        }
    };
    println!("held: {:?}", client.held());

    // Borrow a dup for the render task; the client keeps the canonical.
    match client.fd(&Resource::Fbdev) {
        Ok(fd) => Ok(fd),
        Err(e) => {
            eprintln!("lend failed: {e}");
            Err(e)
        }
    }
}
