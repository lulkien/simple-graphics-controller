use std::{collections::HashSet, io::ErrorKind, os::fd::RawFd};

use nix::libc::pid_t;
use simple_graphics_protocol::{ClientRequest, Resource, ServerMessage, deserialize, serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

use crate::types::{OwnerRegistry, ResourceRegistry};

pub async fn handle_connection(
    mut stream: UnixStream,
    owner_reg: OwnerRegistry,
    resource_reg: ResourceRegistry,
) {
    println!("New client connected");

    // wait for hello message
    let client_pid: pid_t;
    if let Some(pid) = wait_hello_message(&mut stream).await {
        client_pid = pid;
        println!("Registered client: {pid}");
    } else {
        eprintln!("Client not say hello, impolite");
        return;
    }

    let mut buf = [0u8; 1024];
    loop {
        stream.readable().await.expect("readable failed");

        match stream.read(&mut buf).await {
            Ok(0) => {
                eprintln!("EOF");
                break;
            }

            Ok(n) => {
                let req: ClientRequest = deserialize(&buf[..n]).expect("deserialize failed");
                match req {
                    ClientRequest::Hello { .. } => {
                        eprintln!("Invalid hello request. ignored");
                        continue;
                    }
                    ClientRequest::Request { resources } => {
                        handle_client_request(
                            client_pid,
                            resources,
                            owner_reg.clone(),
                            resource_reg.clone(),
                        )
                        .await;
                    }
                    ClientRequest::Release { resources } => {
                        handle_client_release(
                            client_pid,
                            resources,
                            owner_reg.clone(),
                            resource_reg.clone(),
                        )
                        .await;
                    }
                    ClientRequest::Ack => {
                        eprintln!("unsupported");
                        continue;
                    }
                }
            }

            Err(e) => {
                if e.kind() == ErrorKind::WouldBlock {
                    eprintln!("WouldBlock");
                    continue;
                } else {
                    eprintln!("{e}");
                    break;
                }
            }
        }
    }

    cleanup_client_data(client_pid, owner_reg, resource_reg).await;
}

async fn wait_hello_message(stream: &mut UnixStream) -> Option<pid_t> {
    let mut buf = [0u8; 1024];
    stream.readable().await.expect("readable failed");

    let n = stream.read(&mut buf).await.expect("read failed");
    if n == 0 {
        eprintln!("Invalid message len");
        return None;
    }

    let req: ClientRequest = deserialize(&buf[..n]).expect("deserialize failed");
    match req {
        ClientRequest::Hello { pid } => {
            let res = serialize(&ServerMessage::Hello).expect("serialize failed");
            stream.write_all(&res).await.expect("write failed");
            Some(pid)
        }
        _ => {
            eprintln!("Invalid request, need hello message");
            None
        }
    }
}

#[allow(unused)]
async fn handle_client_request(
    pid: pid_t,
    resources: Vec<Resource>,
    owner_reg: OwnerRegistry,
    resource_reg: ResourceRegistry,
) {
    let mut grant_resources: Vec<Resource> = Vec::new();
    let mut grant_rawfds: Vec<RawFd> = Vec::new();

    for resource in resources {
        match owner_reg.get(&resource) {
            Some(pid) => {
                todo!()
            }
            None => {
                todo!()
            }
        }
    }
}

#[allow(unused)]
async fn handle_client_release(
    pid: pid_t,
    resources: Vec<Resource>,
    owner_reg: OwnerRegistry,
    resource_reg: ResourceRegistry,
) {
    todo!()
}

#[allow(unused)]
async fn cleanup_client_data(pid: pid_t, owner_reg: OwnerRegistry, resource_reg: ResourceRegistry) {
    todo!()
}
