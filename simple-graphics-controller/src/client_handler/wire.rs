//! Wire plumbing: drain the socket without blocking and assemble complete
//! frames. Never blocks — a partial frame stays buffered until the next
//! readable event, which is what keeps the control channel (Revoke/Grant)
//! responsive while a client dribbles bytes in.

use std::io::ErrorKind;

use crate::error::ServerResult;
use crate::types::{ClientId, ResourceRegistry};
use crate::windowing::PolicyEngine;
use nix::libc::pid_t;
use simple_graphics_protocol::{
    ClientRequest, FRAME_HEADER_LEN, deserialize, parse_frame_header,
};
use tokio::{net::UnixStream, time::Instant};
use tracing::{debug, trace};

use super::requests;

/// Drain the socket (non-blocking), assemble complete frames in `frame_buf`,
/// and dispatch each request. Returns `Ok(true)` to keep the connection
/// loop going, `Ok(false)` on EOF.
pub(super) async fn process_wire_bytes(
    stream: &mut UnixStream,
    frame_buf: &mut Vec<u8>,
    client_id: ClientId,
    client_pid: pid_t,
    engine: &PolicyEngine,
    resource_reg: &ResourceRegistry,
    ack_deadline: &mut Option<Instant>,
) -> ServerResult<bool> {
    // Drain whatever is available right now (WouldBlock = back to select!).
    let mut tmp = [0u8; 4096];
    loop {
        match stream.try_read(&mut tmp) {
            Ok(0) => return Ok(false), // peer closed
            Ok(n) => {
                trace!("[client {client_id} (pid {client_pid})] read {n} bytes");
                frame_buf.extend_from_slice(&tmp[..n]);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) => {
                return Err(anyhow::anyhow!("failed to read from client: {e}").into());
            }
        }
    }

    // Dispatch every complete frame currently buffered.
    while frame_buf.len() >= FRAME_HEADER_LEN {
        let header: [u8; FRAME_HEADER_LEN] = frame_buf[..FRAME_HEADER_LEN]
            .try_into()
            .expect("length checked above");
        let len = parse_frame_header(&header).map_err(|e| anyhow::anyhow!("{e}"))?;
        if frame_buf.len() < FRAME_HEADER_LEN + len {
            break; // incomplete frame; wait for more bytes
        }

        let payload: Vec<u8> = frame_buf[FRAME_HEADER_LEN..FRAME_HEADER_LEN + len].to_vec();
        frame_buf.drain(..FRAME_HEADER_LEN + len);

        let req: ClientRequest = deserialize(&payload).map_err(|e| anyhow::anyhow!("{e}"))?;
        debug!("[client {client_id} (pid {client_pid})] Request: {req:?}");
        requests::dispatch_request(
            req,
            stream,
            client_id,
            client_pid,
            engine,
            resource_reg,
            ack_deadline,
        )
        .await?;
    }

    Ok(true)
}
