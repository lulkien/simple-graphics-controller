//! The accept loop: bind the abstract `@sgc` socket, accept connections, and
//! spawn one task per client (fresh [`ClientId`], kernel peer credentials).
//! The engine and resource registry are shared across connections.

use std::{
    os::{
        linux::net::SocketAddrExt, unix::net::SocketAddr,
        unix::net::UnixListener as StdUnixListener,
    },
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{
    client_handler::handle_connection,
    types::{ClientId, ResourceRegistry},
    windowing::PolicyEngine,
};
use anyhow::Context;
use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
use simple_graphics_protocol::Resource;
use tokio::net::UnixListener;
use tracing::{debug, error, info};

/// The abstract socket the server listens on.
const SOCKET_NAME: &[u8] = b"sgc";

/// Monotonic connection counter. Starts at 1 so `ClientId(0)` stays free as
/// a "no client" sentinel; ids are never reused while the server runs
/// (unlike pid_t, which the kernel recycles).
static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

/// Bind `@sgc` and serve connections forever. Returns only when the accept
/// loop ends (the listener broke).
pub async fn run(
    engine: PolicyEngine,
    resource_reg: ResourceRegistry,
    advertised: Arc<Vec<Resource>>,
) -> anyhow::Result<()> {
    // Listen on the abstract namespace socket @sgc. Abstract sockets have
    // no filesystem path and vanish automatically when the server dies.
    let addr =
        SocketAddr::from_abstract_name(SOCKET_NAME).context("invalid abstract socket address")?;
    let std_listener =
        StdUnixListener::bind_addr(&addr).context("failed to bind abstract socket @sgc")?;
    std_listener
        .set_nonblocking(true)
        .context("failed to set listener nonblocking")?;
    let listener =
        UnixListener::from_std(std_listener).context("failed to wrap listener in tokio")?;
    info!("Listening on abstract socket @sgc");

    // Accept loop: every connection gets a fresh ClientId (the engine's key
    // for ownership and control-channel lookup) and its own task. Peer
    // credentials come from the kernel (SO_PEERCRED) and stay in the logs.
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                // Get kernel-verified peer credentials (SO_PEERCRED)
                let creds = getsockopt(&stream, PeerCredentials)
                    .context("failed to read peer credentials (SO_PEERCRED)")?;
                let client_pid = creds.pid();
                let client_id = ClientId::new(NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed));
                debug!(
                    "[client {client_id} (pid {client_pid})] Peer credentials: uid={}, gid={}",
                    creds.uid(),
                    creds.gid()
                );

                let engine = engine.clone();
                let resource_reg = resource_reg.clone();
                let advertised = advertised.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(
                        stream,
                        client_id,
                        client_pid,
                        engine.clone(),
                        resource_reg.clone(),
                        advertised,
                    )
                    .await
                    {
                        error!("[client {client_id} (pid {client_pid})] Handler error: {e:#}");
                    }
                });
            }
            Err(e) => {
                error!("Accept error: {e}");
                break;
            }
        }
    }

    Ok(())
}
