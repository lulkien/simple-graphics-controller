use std::{
    io::ErrorKind,
    os::fd::{AsRawFd, RawFd},
    time::Duration,
};

use crate::error::{ServerError, ServerResult};
use crate::types::lock_owners;
use nix::libc::pid_t;
use sendfd::SendWithFd;
use simple_graphics_protocol::{
    ClientRequest, FRAME_HEADER_LEN, Resource, ServerMessage, deserialize, parse_frame_header,
    serialize_framed,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    time::Instant,
};
use tracing::{debug, error, info, trace, warn};

use crate::types::{OwnerRegistry, ResourceRegistry};

/// How long the server waits for a client to acknowledge a Grant before
/// declaring the grant unconfirmed.
const GRANT_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Outcome of reading one framed message from a client.
enum MessageRead {
    /// A complete, deserialized request.
    Message(ClientRequest),
    /// The peer closed the connection cleanly.
    Closed,
    /// A Grant is awaiting acknowledgment but the client stayed silent for
    /// the whole deadline.
    AckTimeout,
}

pub async fn handle_connection(
    mut stream: UnixStream,
    client_pid: pid_t,
    owner_reg: OwnerRegistry,
    resource_reg: ResourceRegistry,
) -> ServerResult<()> {
    info!("[pid {client_pid}] New client connected");

    // Send available resources immediately (no Hello handshake needed)
    let available_resources: Vec<Resource> = resource_reg
        .iter()
        .map(|entry| entry.key().clone())
        .collect();
    let res = serialize_framed(&ServerMessage::Advertise {
        available_resources: available_resources.clone(),
    })?;

    stream.write_all(&res).await.map_err(ServerError::Write)?;
    debug!(
        "[pid {client_pid}] Sent Advertise: {:?}",
        available_resources
    );

    let mut ack_deadline: Option<Instant> = None;
    loop {
        let outcome = match read_message(&mut stream, ack_deadline, client_pid).await {
            Ok(outcome) => outcome,
            Err(e) => {
                error!("[pid {client_pid}] {e}");
                break;
            }
        };

        match outcome {
            MessageRead::Closed => {
                debug!("[pid {client_pid}] Peer closed connection");
                break;
            }
            MessageRead::AckTimeout => {
                warn!("[pid {client_pid}] Grant not acknowledged within {GRANT_ACK_TIMEOUT:?}");
                ack_deadline = None;
                continue;
            }
            MessageRead::Message(req) => {
                debug!("[pid {client_pid}] Request: {:?}", req);
                if ack_deadline.is_some() && !matches!(&req, ClientRequest::Ack) {
                    warn!("[pid {client_pid}] Received {req:?} before Grant acknowledgment");
                    ack_deadline = None;
                }
                match req {
                    ClientRequest::Acquire { resources } => {
                        info!("[pid {client_pid}] Acquire: {:?}", resources);
                        let granted = handle_client_request(
                            &mut stream,
                            client_pid,
                            resources,
                            owner_reg.clone(),
                            resource_reg.clone(),
                        )
                        .await?;
                        if granted {
                            ack_deadline = Some(Instant::now() + GRANT_ACK_TIMEOUT);
                        }
                    }
                    ClientRequest::Release { resources } => {
                        handle_client_release(client_pid, resources, owner_reg.clone()).await;
                    }
                    ClientRequest::Ack => {
                        info!("[pid {client_pid}] Grant acknowledged");
                        ack_deadline = None;
                    }
                }
            }
        }
    }

    info!("[pid {client_pid}] Client disconnected");
    cleanup_client_data(client_pid, owner_reg).await;

    Ok(())
}

/// Wait for and read one framed request from the client.
///
/// The readability wait is bounded when `ack_deadline` is set (a Grant is
/// awaiting acknowledgment) so a silent client cannot stall the connection.
/// Stream sockets do not preserve message boundaries, so the 4-byte length
/// header tells us exactly how much to read.
async fn read_message(
    stream: &mut UnixStream,
    ack_deadline: Option<Instant>,
    client_pid: pid_t,
) -> ServerResult<MessageRead> {
    if let Some(deadline) = ack_deadline {
        match tokio::time::timeout_at(deadline, stream.readable()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                return Err(
                    anyhow::anyhow!("failed to wait for socket readability: {e}").into(),
                );
            }
            Err(_) => return Ok(MessageRead::AckTimeout),
        }
    } else {
        stream
            .readable()
            .await
            .map_err(|e| anyhow::anyhow!("failed to wait for socket readability: {e}"))?;
    }

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
    trace!("[pid {client_pid}] Reading {len} byte payload");

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

#[allow(unused)]
async fn handle_client_request(
    stream: &mut UnixStream,
    pid: pid_t,
    resources: Vec<Resource>,
    owner_reg: OwnerRegistry,
    resource_reg: ResourceRegistry,
) -> ServerResult<bool> {
    // Atomic acquire: validate and claim under a single lock so concurrent
    // requests cannot both pass the "free" check (TOCTOU). The lock is held
    // only for the check+claim (no awaits inside).
    let mut grant_fds: Vec<RawFd> = Vec::new();
    let reason = {
        let mut owners = lock_owners(&owner_reg);

        // Validate: all requested resources must be registered and free.
        let mut reason = None;
        for resource in &resources {
            resource_reg
                .get(resource)
                .ok_or_else(|| anyhow::anyhow!("resource {resource:?} is not registered"))?;
            match owners.get(resource) {
                Some(Some(o)) if *o == pid => {
                    reason = Some(format!("{resource:?} is already owned by this client"));
                    break;
                }
                Some(Some(o)) => {
                    reason = Some(format!("{resource:?} is owned by pid {o}"));
                    break;
                }
                Some(None) | None => {}
            }
        }

        // Claim all requested resources (registry membership validated above).
        if reason.is_none() {
            for resource in &resources {
                let fd = resource_reg
                    .get(resource)
                    .expect("registry membership validated above")
                    .as_raw_fd();
                debug!("[pid {pid}] Granting fd {fd} for {resource:?}");
                grant_fds.push(fd);
                owners.insert(resource.clone(), Some(pid));
            }
        }
        reason
    };

    let granted = match reason {
        Some(reason) => {
            info!("[pid {pid}] Denied acquire of {:?}: {reason}", resources);
            let resp = serialize_framed(&ServerMessage::Deny { reason })?;
            stream.write_all(&resp).await.map_err(ServerError::Write)?;
            false
        }
        None => {
            info!(
                "[pid {pid}] Granted {:?} ({} fd(s))",
                resources,
                grant_fds.len()
            );
            let response = serialize_framed(&ServerMessage::Grant { resources })?;
            stream
                .send_with_fd(&response, &grant_fds)
                .map_err(ServerError::Write)?;
            true
        }
    };

    Ok(granted)
}

async fn handle_client_release(pid: pid_t, resources: Vec<Resource>, owner_reg: OwnerRegistry) {
    let mut released: Vec<Resource> = Vec::new();
    let mut not_owned: Vec<Resource> = Vec::new();

    {
        let mut owners = lock_owners(&owner_reg);
        for resource in resources {
            match owners.get(&resource) {
                Some(Some(o)) if *o == pid => {
                    owners.insert(resource.clone(), None);
                    released.push(resource);
                }
                _ => not_owned.push(resource),
            }
        }
    }

    for resource in not_owned {
        warn!("[pid {pid}] Cannot release {resource:?}: not owned by this client");
    }
    if !released.is_empty() {
        info!("[pid {pid}] Released {:?}", released);
    }
}

async fn cleanup_client_data(pid: pid_t, owner_reg: OwnerRegistry) {
    let mut released: Vec<Resource> = Vec::new();

    {
        let mut owners = lock_owners(&owner_reg);
        for (resource, owner) in owners.iter_mut() {
            if *owner == Some(pid) {
                *owner = None;
                released.push(resource.clone());
            }
        }
    }

    if released.is_empty() {
        debug!("[pid {pid}] Cleaned up, no resources held");
    } else {
        info!("[pid {pid}] Cleaned up, released {:?}", released);
    }
}
