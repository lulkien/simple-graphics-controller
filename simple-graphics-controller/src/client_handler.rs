use std::{
    io::ErrorKind,
    os::fd::{AsRawFd, RawFd},
    time::Duration,
};

use crate::error::{ServerError, ServerResult};
use crate::types::{ClientId, ResourceRegistry};
use crate::windowing::{AcquireOutcome, ControlMessage, PolicyEngine};
use nix::libc::pid_t;
use sendfd::SendWithFd;
use simple_graphics_protocol::{
    ClientRequest, FRAME_HEADER_LEN, Resource, ServerMessage, deserialize, parse_frame_header,
    serialize_framed,
};
use tokio::{
    io::AsyncWriteExt,
    net::UnixStream,
    sync::mpsc,
    time::Instant,
};
use tracing::{debug, info, trace, warn};

/// How long the server waits for a client to acknowledge a Grant before
/// declaring the grant unconfirmed. Delivery signal only — it never gates
/// ownership or the queue (the fd was already duplicated to the client).
const GRANT_ACK_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn handle_connection(
    mut stream: UnixStream,
    client_id: ClientId,
    client_pid: pid_t,
    engine: PolicyEngine,
    resource_reg: ResourceRegistry,
) -> ServerResult<()> {
    info!("[client {client_id} (pid {client_pid})] New client connected");

    // Control channel: the engine pushes Revoke/Grant here; this task writes
    // them on the wire (with fds for Grant). Unregistered on disconnect.
    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    engine.register(client_id, control_tx).await;

    // Send available resources immediately (no Hello handshake needed).
    let available_resources: Vec<Resource> = resource_reg
        .iter()
        .map(|entry| entry.key().clone())
        .collect();
    let res = serialize_framed(&ServerMessage::Advertise {
        available_resources: available_resources.clone(),
    })?;
    stream.write_all(&res).await.map_err(ServerError::Write)?;
    debug!(
        "[client {client_id} (pid {client_pid})] Sent Advertise: {available_resources:?}"
    );

    let mut ack_deadline: Option<Instant> = None;
    // Bytes read from the wire but not yet assembled into a complete frame.
    // Persists across select! iterations: a frame split across reads must
    // survive even when the control channel wins the next select.
    let mut frame_buf: Vec<u8> = Vec::new();

    // Wrap the loop so engine.disconnected() ALWAYS runs, even when an arm
    // propagates an error (e.g. EPIPE writing to a client that just died) —
    // otherwise the engine would keep a dead client as owner forever.
    let result: ServerResult<()> = async {
        loop {
            tokio::select! {
                // Wire: a client request (never blocks on a partial frame —
                // see process_wire_bytes; the control channel stays live).
                _ = stream.readable() => {
                    if !process_wire_bytes(
                        &mut stream,
                        &mut frame_buf,
                        client_id,
                        client_pid,
                        &engine,
                        &resource_reg,
                        &mut ack_deadline,
                    ).await? {
                        break;
                    }
                }
                // Engine: server-pushed Revoke / Grant-from-queue.
                Some(msg) = control_rx.recv() => {
                    process_control_message(
                        &mut stream,
                        client_id,
                        client_pid,
                        msg,
                        &resource_reg,
                        &mut ack_deadline,
                    ).await?;
                }
                // Grant delivery ack timeout (log-only; never gates ownership).
                // NOTE: select! evaluates this future EXPRESSION eagerly,
                // before any if-guard would be checked — so it must not
                // panic on None. Pending forever while no grant awaits an ack.
                _ = async {
                    if let Some(deadline) = ack_deadline {
                        tokio::time::sleep_until(deadline).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    warn!("[client {client_id} (pid {client_pid})] Grant not acknowledged within {GRANT_ACK_TIMEOUT:?}");
                    ack_deadline = None;
                }
            }
        }
        Ok(())
    }
    .await;

    info!("[client {client_id} (pid {client_pid})] Client disconnected");
    engine.disconnected(client_id).await;
    result?;

    Ok(())
}

/// Drain the socket (non-blocking), assemble complete frames in `frame_buf`,
/// and dispatch each request. Returns `Ok(true)` to keep the connection
/// loop going, `Ok(false)` on EOF.
///
/// Never blocks: a partial frame just stays in `frame_buf` until the next
/// readable event. This is what keeps the control channel (Revoke/Grant)
/// responsive while a client dribbles bytes in.
async fn process_wire_bytes(
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
        dispatch_request(
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

/// Handle one parsed client request.
async fn dispatch_request(
    req: ClientRequest,
    stream: &mut UnixStream,
    client_id: ClientId,
    client_pid: pid_t,
    engine: &PolicyEngine,
    resource_reg: &ResourceRegistry,
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
                resource_reg,
                ack_deadline,
            )
            .await?;
        }
        ClientRequest::Release { resource } => {
            handle_release(client_id, resource, engine).await;
        }
        ClientRequest::Ack => {
            info!("[client {client_id} (pid {client_pid})] Grant acknowledged");
            *ack_deadline = None;
        }
    }
    Ok(())
}

/// Act on a message pushed by the engine through the control channel.
async fn process_control_message(
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

/// Handle an Acquire: ask the engine, then act on its decision.
///
/// A queued Acquire gets no reply here — the Grant arrives later through the
/// control channel (the requeued/queued grant path).
async fn handle_acquire(
    stream: &mut UnixStream,
    client_id: ClientId,
    client_pid: pid_t,
    resource: Resource,
    engine: &PolicyEngine,
    resource_reg: &ResourceRegistry,
    ack_deadline: &mut Option<Instant>,
) -> ServerResult<()> {
    info!("[client {client_id} (pid {client_pid})] Acquire: {resource:?}");

    match engine.acquire(client_id, resource.clone()).await {
        AcquireOutcome::Granted => {
            send_grant(stream, client_id, client_pid, resource, resource_reg).await?;
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
async fn send_grant(
    stream: &mut UnixStream,
    client_id: ClientId,
    client_pid: pid_t,
    resource: Resource,
    resource_reg: &ResourceRegistry,
) -> ServerResult<()> {
    let fd = resource_reg
        .get(&resource)
        .ok_or_else(|| anyhow::anyhow!("resource {resource:?} is not registered"))?
        .as_raw_fd();

    info!("[client {client_id} (pid {client_pid})] Granted {resource:?} (fd {fd})");
    let response = serialize_framed(&ServerMessage::Grant { resource })?;
    stream
        .send_with_fd(&response, &[fd])
        .map_err(ServerError::Write)?;
    Ok(())
}

/// Forward a Release to the engine. The engine validates ownership (a
/// Release from a non-owner, or one that completes a revoke, is its call —
/// it warns and requeues accordingly).
async fn handle_release(client_id: ClientId, resource: Resource, engine: &PolicyEngine) {
    engine.release(client_id, resource).await;
}
