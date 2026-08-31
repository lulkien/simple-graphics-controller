//! Engine-pushed control messages: write `Revoke` / `Grant`-from-queue on
//! the wire. A `Grant` carries the resource's fd and arms the ack timer.

use crate::error::{ServerError, ServerResult};
use crate::types::{ClientId, ResourceRegistry};
use crate::windowing::ControlMessage;
use nix::libc::pid_t;
use simple_graphics_protocol::{ServerMessage, serialize_framed};
use tokio::{io::AsyncWriteExt, net::UnixStream, time::Instant};
use tracing::info;

use super::requests::send_grant;
use super::GRANT_ACK_TIMEOUT;

pub(super) async fn process_control_message(
    stream: &mut UnixStream,
    client_id: ClientId,
    client_pid: pid_t,
    msg: ControlMessage,
    resource_reg: &ResourceRegistry,
    ack_deadline: &mut Option<Instant>,
) -> ServerResult<()> {
    match msg {
        ControlMessage::Revoke { resource } => {
            info!("[client {client_id} (pid {client_pid})] Revoking {resource:?}");
            let resp = serialize_framed(&ServerMessage::Revoke { resource })?;
            stream.write_all(&resp).await.map_err(ServerError::Write)?;
        }
        ControlMessage::Grant { resource } => {
            send_grant(stream, client_id, client_pid, resource, resource_reg).await?;
            *ack_deadline = Some(Instant::now() + GRANT_ACK_TIMEOUT);
        }
    }
    Ok(())
}
