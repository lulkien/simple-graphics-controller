use std::os::{
    linux::net::SocketAddrExt,
    unix::net::{SocketAddr, UnixListener as StdUnixListener},
};

use crate::{client_handler::handle_connection, resource_manager::query_resource};
use anyhow::Context;
use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
use tokio::net::UnixListener;
use tracing::{debug, error, info};
use tracing_subscriber::EnvFilter;

mod client_handler;
mod error;
mod resource_manager;
mod types;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // init logger
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!("Starting simple-graphics-controller");
    let (resource_reg, owner_reg) = query_resource();
    debug!(
        "Registered resources: {:?}",
        resource_reg
            .iter()
            .map(|entry| entry.key().clone())
            .collect::<Vec<_>>()
    );

    let addr = SocketAddr::from_abstract_name(b"sgc").context("invalid abstract socket address")?;
    let std_listener =
        StdUnixListener::bind_addr(&addr).context("failed to bind abstract socket @sgc")?;
    std_listener
        .set_nonblocking(true)
        .context("failed to set listener nonblocking")?;
    let listener =
        UnixListener::from_std(std_listener).context("failed to wrap listener in tokio")?;
    info!("Listening on abstract socket @sgc");

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                // Get kernel-verified peer credentials (SO_PEERCRED)
                let creds = getsockopt(&stream, PeerCredentials)
                    .context("failed to read peer credentials (SO_PEERCRED)")?;
                let client_pid = creds.pid();
                debug!(
                    "[pid {client_pid}] Peer credentials: uid={}, gid={}",
                    creds.uid(),
                    creds.gid()
                );

                let owner_reg = owner_reg.clone();
                let resource_reg = resource_reg.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(
                        stream,
                        client_pid,
                        owner_reg.clone(),
                        resource_reg.clone(),
                    )
                    .await
                    {
                        error!("[pid {client_pid}] Handler error: {e:#}");
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
