//! Engine-pushed control messages: write `Revoke` / `Grant`-from-queue on
//! the wire. A `Grant` carries the resource's fd and arms the ack timer.

use crate::{
    error::{ServerError, ServerResult},
    resource_manager::ResourceRegistries,
    types::ClientId,
    windowing::ControlMessage,
};
use nix::libc::pid_t;
#[cfg(feature = "drm")]
use simple_graphics_protocol::Resource;
use simple_graphics_protocol::{ServerMessage, serialize_framed};
use tokio::{io::AsyncWriteExt, net::UnixStream, time::Instant};
use tracing::info;

use super::GRANT_ACK_TIMEOUT;
use super::requests::send_grant;

pub(super) async fn process_control_message(
    stream: &mut UnixStream,
    client_id: ClientId,
    client_pid: pid_t,
    msg: ControlMessage,
    registries: &ResourceRegistries,
    ack_deadline: &mut Option<Instant>,
) -> ServerResult<()> {
    match msg {
        ControlMessage::Revoke { resource } => {
            // Kernel-enforced revoke: invalidate a granted DRM lease NOW,
            // before the client is even told — a misbehaving client that
            // keeps its fd open must not be able to keep the card.
            // (Fbdev/Input have no such mechanism; they are cooperative.)
            #[cfg(feature = "drm")]
            if matches!(resource, Resource::Drm { .. })
                && let Some(device) = registries.drm.get(&resource)
            {
                device.revoke_lease();
            }
            info!("[client {client_id} (pid {client_pid})] Revoking {resource:?}");
            let resp = serialize_framed(&ServerMessage::Revoke { resource })?;
            stream.write_all(&resp).await.map_err(ServerError::Write)?;
        }
        ControlMessage::Grant { resource } => {
            send_grant(stream, client_id, client_pid, resource, registries).await?;
            *ack_deadline = Some(Instant::now() + GRANT_ACK_TIMEOUT);
        }
    }
    Ok(())
}
