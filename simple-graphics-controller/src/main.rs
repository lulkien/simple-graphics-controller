use std::os::{
    linux::net::SocketAddrExt,
    unix::net::{SocketAddr, UnixListener as StdUnixListener},
};

use crate::{client_handler::handle_connection, resource_manager::query_resource};
use anyhow::Context;
use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
use tokio::net::UnixListener;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

mod client_handler;
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

    let addr = SocketAddr::from_abstract_name(b"sgc").context("invalid socket address")?;
    let std_listener = StdUnixListener::bind_addr(&addr).context("bind failed")?;
    std_listener
        .set_nonblocking(true)
        .context("set nonblocking failed")?;
    let listener = UnixListener::from_std(std_listener).context("into listener failed")?;

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                // Get kernel-verified peer credentials (SO_PEERCRED)
                let creds = getsockopt(&stream, PeerCredentials)?;
                let client_pid = creds.pid();

                tokio::spawn(handle_connection(
                    stream,
                    client_pid,
                    owner_reg.clone(),
                    resource_reg.clone(),
                ));
            }
            Err(e) => {
                error!("Accept error: {e}");
                break;
            }
        }
    }

    Ok(())
}
