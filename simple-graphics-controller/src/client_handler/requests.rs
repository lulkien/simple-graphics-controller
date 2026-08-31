//! Client request semantics: Acquire (ask the engine, reply Grant/Deny or
//! stay silent for a queue), Release (forward to the engine), Ack (clear
//! the grant-ack timer).

use std::os::fd::{AsRawFd, OwnedFd};

use crate::{
    error::{ServerError, ServerResult},
    resource_manager::ResourceRegistries,
    types::ClientId,
    windowing::{AcquireOutcome, PolicyEngine},
};
use nix::libc::pid_t;
use sendfd::SendWithFd;
use simple_graphics_protocol::{ClientRequest, Resource, ServerMessage, serialize_framed};
use tokio::{io::AsyncWriteExt, net::UnixStream, time::Instant};
use tracing::{debug, info};

use super::GRANT_ACK_TIMEOUT;

/// Handle one parsed client request.
pub(super) async fn dispatch_request(
    req: ClientRequest,
    stream: &mut UnixStream,
    client_id: ClientId,
    client_pid: pid_t,
    engine: &PolicyEngine,
    registries: &ResourceRegistries,
    ack_deadline: &mut Option<Instant>,
) -> ServerResult<()> {
    match req {
        ClientRequest::Acquire { resource } => {
            handle_acquire(
                stream,
                client_id,
                client_pid,
                resource,
                engine,
                registries,
                ack_deadline,
            )
            .await?;
        }
        ClientRequest::Release { resource } => {
            handle_release(client_id, resource, engine, registries).await;
        }
        ClientRequest::Ack => {
            info!("[client {client_id} (pid {client_pid})] Grant acknowledged");
            *ack_deadline = None;
        }
    }
    Ok(())
}

/// Handle an Acquire: ask the engine, then act on its decision.
///
/// A queued Acquire gets no reply here — the Grant arrives later through the
/// control channel (the requeued/queued grant path).
pub(super) async fn handle_acquire(
    stream: &mut UnixStream,
    client_id: ClientId,
    client_pid: pid_t,
    resource: Resource,
    engine: &PolicyEngine,
    registries: &ResourceRegistries,
    ack_deadline: &mut Option<Instant>,
) -> ServerResult<()> {
    info!("[client {client_id} (pid {client_pid})] Acquire: {resource:?}");

    match engine.acquire(client_id, resource.clone()).await {
        AcquireOutcome::Granted => {
            send_grant(stream, client_id, client_pid, resource, registries).await?;
            *ack_deadline = Some(Instant::now() + GRANT_ACK_TIMEOUT);
        }
        AcquireOutcome::Queued => {
            debug!(
                "[client {client_id} (pid {client_pid})] Queued for {resource:?}; \
                 grant will arrive via control channel"
            );
        }
        AcquireOutcome::Denied { reason } => {
            info!("[client {client_id} (pid {client_pid})] Denied {resource:?}: {reason}");
            let resp = serialize_framed(&ServerMessage::Deny { reason })?;
            stream.write_all(&resp).await.map_err(ServerError::Write)?;
        }
    }
    Ok(())
}

/// Send `ServerMessage::Grant` for one resource, with its fd.
///
/// For `Drm` the fd is a FRESH lease, created for this grant: the client
/// becomes the lessee (never DRM master), and the server keeps the ability
/// to revoke it at any time. Other resources grant a dup of the server's
/// registered fd.
pub(super) async fn send_grant(
    stream: &mut UnixStream,
    client_id: ClientId,
    client_pid: pid_t,
    resource: Resource,
    registries: &ResourceRegistries,
) -> ServerResult<()> {
    // The granted fd must stay open until AFTER send_with_fd: SCM_RIGHTS
    // dups the fd at send time, and sending a number whose fd was already
    // closed fails with EBADF. Binding it here (not inside the match arm)
    // keeps it alive across the send.
    let granted = grant_fd(&resource, registries)?;
    let fd = granted.as_raw_fd();

    info!("[client {client_id} (pid {client_pid})] Granted {resource:?} (fd {fd})");
    let response = serialize_framed(&ServerMessage::Grant { resource })?;
    stream
        .send_with_fd(&response, &[fd])
        .map_err(ServerError::Write)?;
    Ok(())
}

/// The fd to grant for one resource: a fresh lease for `Drm`, otherwise a
/// dup of the server's registered fd.
fn grant_fd(resource: &Resource, registries: &ResourceRegistries) -> anyhow::Result<OwnedFd> {
    match resource {
        Resource::Drm { .. } => {
            let device = registries
                .drm
                .get(resource)
                .ok_or_else(|| anyhow::anyhow!("resource {resource:?} is not registered"))?;
            device
                .grant_lease()
                .map_err(|e| anyhow::anyhow!("failed to lease {resource:?}: {e}"))
        }
        _ => registries
            .fds
            .get(resource)
            .ok_or_else(|| anyhow::anyhow!("resource {resource:?} is not registered"))?
            .try_clone()
            .map_err(|e| anyhow::anyhow!("failed to dup fd for {resource:?}: {e}")),
    }
}

/// Forward a Release to the engine. The engine validates ownership (a
/// Release from a non-owner, or one that completes a revoke, is its call —
/// it warns and requeues accordingly). A `Drm` lease is revoked
/// immediately: the resource must be truly free before the next grant,
/// kernel-enforced, regardless of what the client does with its fd.
pub(super) async fn handle_release(
    client_id: ClientId,
    resource: Resource,
    engine: &PolicyEngine,
    registries: &ResourceRegistries,
) {
    if matches!(resource, Resource::Drm { .. })
        && let Some(device) = registries.drm.get(&resource)
    {
        device.revoke_lease();
    }
    engine.release(client_id, resource).await;
}
