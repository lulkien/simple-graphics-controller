use std::{
    collections::HashMap,
    os::{linux::net::SocketAddrExt, unix::net::SocketAddr, unix::net::UnixListener as StdUnixListener},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    client_handler::handle_connection,
    resource_manager::query_resource,
    types::ClientId,
    windowing::{Policy, PolicyEngine},
};
use anyhow::Context;
use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
use simple_graphics_protocol::Resource;
use tokio::net::UnixListener;
use tracing::{debug, error, info};
use tracing_subscriber::EnvFilter;

mod client_handler;
mod error;
mod resource_manager;
mod types;
mod windowing;

#[cfg(test)]
mod integration_tests;

/// Monotonic connection counter. Starts at 1 so `ClientId(0)` stays free as
/// a "no client" sentinel; ids are never reused while the server runs
/// (unlike pid_t, which the kernel recycles).
static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // init logger
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!("Starting simple-graphics-controller");
    let resource_reg = query_resource();
    debug!(
        "Registered resources: {:?}",
        resource_reg
            .iter()
            .map(|entry| entry.key().clone())
            .collect::<Vec<_>>()
    );

    // Window policy: SGC_POLICY env, default fair-queue. One policy for all
    // registered resources (per-resource override map is a future knob).
    let policy: Policy = std::env::var("SGC_POLICY")
        .ok()
        .map(|value| value.parse())
        .transpose()
        .context("invalid SGC_POLICY (expected first-owner | latest-owner | fair-queue)")?
        .unwrap_or(Policy::FairQueue);
    info!("Windowing policy: {policy:?}");

    let policies: HashMap<Resource, Policy> = resource_reg
        .iter()
        .map(|entry| (entry.key().clone(), policy))
        .collect();
    let engine = PolicyEngine::spawn(policies);

    // Listen on the abstract namespace socket @sgc. Abstract sockets have
    // no filesystem path and vanish automatically when the server dies.
    let addr = SocketAddr::from_abstract_name(b"sgc").context("invalid abstract socket address")?;
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
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(
                        stream,
                        client_id,
                        client_pid,
                        engine.clone(),
                        resource_reg.clone(),
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
