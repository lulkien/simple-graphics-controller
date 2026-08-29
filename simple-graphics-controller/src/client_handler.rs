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
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::mpsc,
    time::Instant,
};
use tracing::{debug, error, info, trace, warn};

/// How long the server waits for a client to acknowledge a Grant before
/// declaring the grant unconfirmed. Delivery signal only — it never gates
/// ownership or the queue (the fd was already duplicated to the client).
const GRANT_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Outcome of reading one framed message from a client.
enum MessageRead {
    /// A complete, deserialized request.
    Message(ClientRequest),
    /// The peer closed the connection cleanly.
    Closed,
}

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
    debug!("[client {client_id} (pid {client_pid})] Sent Advertise: {available_resources:?}");

    let mut ack_deadline: Option<Instant> = None;
    // Wrap the loop so engine.disconnected() ALWAYS runs, even when an arm
    // propagates an error (e.g. EPIPE writing to a client that just died) —
    // otherwise the engine would keep a dead client as owner forever.
    let result: ServerResult<()> = async {
        loop {
            tokio::select! {
                // Wire: a client request.
                _ = stream.readable() => {
                    if !process_wire_read(
                        &mut stream,
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
                _ = tokio::time::sleep_until(ack_deadline.expect("guarded by if")), if ack_deadline.is_some() => {
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

/// Read one framed request and act on it. Returns `Ok(true)` to keep the
/// connection loop going, `Ok(false)` when the peer closed or the read
/// failed (the error is logged here; nothing to propagate).
async fn process_wire_read(
    stream: &mut UnixStream,
    client_id: ClientId,
    client_pid: pid_t,
    engine: &PolicyEngine,
    resource_reg: &ResourceRegistry,
    ack_deadline: &mut Option<Instant>,
) -> ServerResult<bool> {
    match read_message(stream, client_id, client_pid).await {
        Ok(MessageRead::Message(req)) => {
            debug!("[client {client_id} (pid {client_pid})] Request: {req:?}");
            match req {
                ClientRequest::Acquire { resources } => {
                    handle_acquire(
                        stream,
                        client_id,
                        client_pid,
                        resources,
                        engine,
                        resource_reg,
                        ack_deadline,
                    )
                    .await?;
                }
                ClientRequest::Release { resources } => {
                    handle_release(client_id, resources, engine).await;
                }
                ClientRequest::Ack => {
                    info!("[client {client_id} (pid {client_pid})] Grant acknowledged");
                    *ack_deadline = None;
                }
            }
            Ok(true)
        }
        Ok(MessageRead::Closed) => {
            debug!("[client {client_id} (pid {client_pid})] Peer closed connection");
            Ok(false)
        }
        Err(e) => {
            error!("[client {client_id} (pid {client_pid})] {e}");
            Ok(false)
        }
    }
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
        ControlMessage::Revoke { resources } => {
            info!("[client {client_id} (pid {client_pid})] Revoking {resources:?}");
            let resp = serialize_framed(&ServerMessage::Revoke { resources })?;
            stream.write_all(&resp).await.map_err(ServerError::Write)?;
        }
        ControlMessage::Grant { resources } => {
            send_grant(stream, client_id, client_pid, resources, resource_reg).await?;
            *ack_deadline = Some(Instant::now() + GRANT_ACK_TIMEOUT);
        }
    }
    Ok(())
}

/// Wait for and read one framed request from the client.
///
/// Stream sockets do not preserve message boundaries, so the 4-byte length
/// header tells us exactly how much to read. Readiness is driven by the
/// caller's `select!`; if the peer stalls mid-frame the control channel
/// messages queue up and are delivered once the frame completes.
async fn read_message(
    stream: &mut UnixStream,
    client_id: ClientId,
    client_pid: pid_t,
) -> ServerResult<MessageRead> {
    // Frame header: 4-byte big-endian payload length.
    let mut header = [0u8; FRAME_HEADER_LEN];
    match stream.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(MessageRead::Closed),
        Err(e) => {
            return Err(anyhow::anyhow!("failed to read frame header: {e}").into());
        }
    }

    let len = parse_frame_header(&header).map_err(|e| anyhow::anyhow!("{e}"))?;
    trace!("[client {client_id} (pid {client_pid})] Reading {len} byte payload");

    // Payload.
    let mut payload = vec![0u8; len];
    match stream.read_exact(&mut payload).await {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
            return Err(anyhow::anyhow!("peer closed mid-frame ({len} bytes)").into());
        }
        Err(e) => {
            return Err(anyhow::anyhow!("failed to read frame payload: {e}").into());
        }
    }

    let req: ClientRequest = deserialize(&payload).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(MessageRead::Message(req))
}

/// Handle an Acquire: ask the engine, then act on its decision.
///
/// A queued Acquire gets no reply here — the Grant arrives later through the
/// control channel (the requeued/queued grant path).
async fn handle_acquire(
    stream: &mut UnixStream,
    client_id: ClientId,
    client_pid: pid_t,
    resources: Vec<Resource>,
    engine: &PolicyEngine,
    resource_reg: &ResourceRegistry,
    ack_deadline: &mut Option<Instant>,
) -> ServerResult<()> {
    info!("[client {client_id} (pid {client_pid})] Acquire: {resources:?}");

    // v1: single resource per Acquire. All-or-nothing multi-resource grants
    // with queues can deadlock (A holds fb0 waits fb1, B holds fb1 waits
    // fb0); revisit when multi-resource is actually needed.
    let [resource] = resources.as_slice() else {
        let reason = "multi-resource acquire is not supported (one resource per Acquire)";
        let resp = serialize_framed(&ServerMessage::Deny {
            reason: reason.into(),
        })?;
        stream.write_all(&resp).await.map_err(ServerError::Write)?;
        return Ok(());
    };

    match engine.acquire(client_id, resource.clone()).await {
        AcquireOutcome::Granted => {
            send_grant(
                stream,
                client_id,
                client_pid,
                vec![resource.clone()],
                resource_reg,
            )
            .await?;
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

/// Send `ServerMessage::Grant` with one fd per resource, in order.
async fn send_grant(
    stream: &mut UnixStream,
    client_id: ClientId,
    client_pid: pid_t,
    resources: Vec<Resource>,
    resource_reg: &ResourceRegistry,
) -> ServerResult<()> {
    let mut grant_fds: Vec<RawFd> = Vec::new();
    for resource in &resources {
        let fd = resource_reg
            .get(resource)
            .ok_or_else(|| anyhow::anyhow!("resource {resource:?} is not registered"))?
            .as_raw_fd();
        grant_fds.push(fd);
    }

    info!(
        "[client {client_id} (pid {client_pid})] Granted {resources:?} ({} fd(s))",
        grant_fds.len()
    );
    let response = serialize_framed(&ServerMessage::Grant { resources })?;
    stream
        .send_with_fd(&response, &grant_fds)
        .map_err(ServerError::Write)?;
    Ok(())
}

/// Forward a Release to the engine. The engine validates ownership (a
/// Release from a non-owner, or one that completes a revoke, is its call —
/// it warns and requeues accordingly).
async fn handle_release(client_id: ClientId, resources: Vec<Resource>, engine: &PolicyEngine) {
    for resource in resources {
        engine.release(client_id, resource).await;
    }
}
