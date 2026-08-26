use std::{
    io::ErrorKind,
    os::fd::{AsRawFd, RawFd},
};

use anyhow::Context;
use nix::libc::pid_t;
use sendfd::SendWithFd;
use simple_graphics_protocol::{ClientRequest, Resource, ServerMessage, deserialize, serialize};
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
) -> anyhow::Result<()> {
    info!("New client connected: {client_pid}");

    // Send available resources immediately (no Hello handshake needed)
    let available_resources: Vec<Resource> = resource_reg
        .iter()
        .map(|entry| entry.key().clone())
        .collect();
    let res = serialize(&ServerMessage::Advertise {
        available_resources,
    })
    .context("serialize failed")?;

    stream.write_all(&res).await.context("write failed")?;
    info!("Sent Advertise");

    let mut buf = [0u8; 1024];
    loop {
        stream.readable().await.context("readable failed")?;

        match stream.read(&mut buf).await {
            Ok(0) => {
                debug!("EOF");
                break;
            }

            Ok(n) => {
                trace!("Read {n} bytes");
                let req: ClientRequest = deserialize(&buf[..n]).context("deserialize failed")?;
                debug!("Request: {:?}", req);
                match req {
                    ClientRequest::Acquire { resources } => {
                        info!("Handling Acquire: {:?}", resources);
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

            Err(e) => {
                if e.kind() == ErrorKind::WouldBlock {
                    trace!("WouldBlock");
                    continue;
                } else {
                    error!("{e}");
                    break;
                }
            }
        }
    }

    info!("Client disconnected");
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
) -> anyhow::Result<()> {
    let mut grant_resources: Vec<Resource> = Vec::new();
    let mut grant_rawfds: Vec<RawFd> = Vec::new();

    for resource in resources {
        let owner = *owner_reg.get(&resource).expect("get owner failed");
        match owner {
            Some(o) => {
                if o == pid {
                    warn!("Already owned");
                } else {
                    warn!("Owned by other process: {}", o);
                }
                continue;
            }
            None => {
                grant_rawfds.push(
                    resource_reg
                        .get(&resource)
                        .context("get fd failed")?
                        .as_raw_fd(),
                );

                info!("Grant resource: {:?}", resource);
                grant_resources.push(resource.clone());
                owner_reg.insert(resource, Some(pid));
            }
        }
    }

    let response = serialize(&ServerMessage::Grant {
        resources: grant_resources,
    })
    .context("serialize failed")?;

    stream
        .send_with_fd(&response, &grant_rawfds)
        .context("write failed")?;

    Ok(())
}

async fn handle_client_release(pid: pid_t, resources: Vec<Resource>, owner_reg: OwnerRegistry) {
    info!("Release resources: {:?}", resources);

    for mut entry in owner_reg.iter_mut() {
        if resources.contains(entry.key()) {
            match *entry.value() {
                Some(owner) if owner == pid => {
                    *entry.value_mut() = None;
                }
                Some(_) | None => {
                    warn!("It's not belong to you: {pid}");
                }
            }
        }
    }
}

async fn cleanup_client_data(pid: pid_t, owner_reg: OwnerRegistry) {
    info!("Clean up client");

    for mut entry in owner_reg.iter_mut() {
        if *entry.value() == Some(pid) {
            *entry.value_mut() = None;
        }
    }
}
