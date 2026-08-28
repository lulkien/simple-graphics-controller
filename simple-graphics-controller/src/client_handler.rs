use std::{
    io::ErrorKind,
    os::fd::{AsRawFd, RawFd},
};

use crate::error::{ServerError, ServerResult};
use crate::types::lock_owners;
use anyhow::Context;
use nix::libc::pid_t;
use sendfd::SendWithFd;
use simple_graphics_protocol::{
    ClientRequest, FRAME_HEADER_LEN, Resource, ServerMessage, deserialize, parse_frame_header,
    serialize_framed,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};
use tracing::{debug, error, info, trace, warn};

use crate::types::{OwnerRegistry, ResourceRegistry};

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

    loop {
        stream.readable().await.context("failed to wait for socket readability")?;

        // Read one framed message: 4-byte big-endian length, then payload.
        let mut header = [0u8; FRAME_HEADER_LEN];
        match stream.read_exact(&mut header).await {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
                debug!("[pid {client_pid}] Peer closed connection");
                break;
            }
            Err(e) => {
                error!("[pid {client_pid}] failed to read frame header: {e}");
                break;
            }
        }

        let len = match parse_frame_header(&header) {
            Ok(len) => len,
            Err(e) => {
                error!("[pid {client_pid}] {e}");
                break;
            }
        };
        trace!("[pid {client_pid}] Reading {len} byte payload");

        let mut payload = vec![0u8; len];
        match stream.read_exact(&mut payload).await {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
                error!("[pid {client_pid}] Peer closed mid-frame ({len} bytes)");
                break;
            }
            Err(e) => {
                error!("[pid {client_pid}] failed to read frame payload: {e}");
                break;
            }
        }

        let req: ClientRequest = match deserialize(&payload) {
            Ok(req) => req,
            Err(e) => {
                error!("[pid {client_pid}] {e}");
                break;
            }
        };
        debug!("[pid {client_pid}] Request: {:?}", req);
        match req {
            ClientRequest::Acquire { resources } => {
                info!("[pid {client_pid}] Acquire: {:?}", resources);
                handle_client_request(
                    &mut stream,
                    client_pid,
                    resources,
                    owner_reg.clone(),
                    resource_reg.clone(),
                )
                .await?;
            }
            ClientRequest::Release { resources } => {
                handle_client_release(client_pid, resources, owner_reg.clone()).await;
            }
        }
    }

    info!("[pid {client_pid}] Client disconnected");
    cleanup_client_data(client_pid, owner_reg).await;

    Ok(())
}

#[allow(unused)]
async fn handle_client_request(
    stream: &mut UnixStream,
    pid: pid_t,
    resources: Vec<Resource>,
    owner_reg: OwnerRegistry,
    resource_reg: ResourceRegistry,
) -> ServerResult<()> {
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

    match reason {
        Some(reason) => {
            info!("[pid {pid}] Denied acquire of {:?}: {reason}", resources);
            let resp = serialize_framed(&ServerMessage::Deny { reason })?;
            stream.write_all(&resp).await.map_err(ServerError::Write)?;
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
        }
    }

    Ok(())
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
