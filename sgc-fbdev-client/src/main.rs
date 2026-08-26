use std::{
    collections::HashMap,
    io::ErrorKind,
    os::{
        fd::{FromRawFd, OwnedFd, RawFd},
        linux::net::SocketAddrExt,
        unix::net::SocketAddr as StdSocketAddr,
    },
};

use sendfd::RecvWithFd;
use simple_graphics_protocol::{ClientRequest, Resource, ServerMessage, deserialize, serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixStream, unix::SocketAddr},
};

#[tokio::main]
async fn main() {
    // Connect to abstract namespace socket @sgc
    let std_addr = StdSocketAddr::from_abstract_name(b"sgc").expect("invalid socket address");
    let tokio_addr: SocketAddr = SocketAddr::from(std_addr);

    // Connect to the server
    let mut stream = UnixStream::connect_addr(&tokio_addr)
        .await
        .expect("failed to connect to socket");

    // Read Advertise response (no FDs)
    let mut resp_buf = [0u8; 1024];
    let mut fds_buf = [0; 1024];

    let mut resources_map: HashMap<Resource, Option<RawFd>> = Default::default();

    stream.readable().await.expect("readable failed");
    let n = stream.read(&mut resp_buf).await.expect("read failed");
    let msg: ServerMessage = deserialize(&resp_buf[..n]).expect("deserialize failed");
    match msg {
        ServerMessage::Advertise {
            available_resources,
        } => {
            println!("Available resources: {:?}", available_resources);
            for resource in available_resources {
                resources_map.insert(resource, None);
            }
        }
        _ => {
            eprintln!("Expected Advertise, got {:?}", msg);
            return;
        }
    }

    // Request Fbdev
    let req = ClientRequest::Acquire {
        resources: vec![Resource::Fbdev],
    };
    let data = serialize(&req).expect("serialize request failed");
    stream.write_all(&data).await.expect("write failed");
    println!("Sent Acquire for Fbdev");

    // Read Grant with FD
    loop {
        stream.readable().await.expect("readable failed");
        match stream.recv_with_fd(&mut resp_buf, &mut fds_buf) {
            Ok((n, _)) => {
                let msg: ServerMessage = deserialize(&resp_buf[..n]).expect("deserialize failed");
                match msg {
                    ServerMessage::Grant { resources } => {
                        println!("Granted: {:?}", resources);
                        for (idx, resource) in resources.into_iter().enumerate() {
                            resources_map.insert(resource, Some(fds_buf[idx]));
                        }
                        break;
                    }
                    ServerMessage::Deny { reason } => {
                        println!("Denied: {reason}");
                        return;
                    }
                    _ => {
                        eprintln!("Unexpected response: {:?}", msg);
                        return;
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
            Err(e) => panic!("recv failed: {e}"),
        }
    }

    release_resource(&mut stream, &mut resources_map).await;
}

async fn release_resource(
    stream: &mut UnixStream,
    resource_reg: &mut HashMap<Resource, Option<RawFd>>,
) {
    let held_resources: Vec<Resource> = resource_reg
        .iter()
        .filter(|(_, fd)| fd.is_some())
        .map(|(resource, _)| resource.clone())
        .collect();

    for resource in &held_resources {
        if let Some(Some(raw_fd)) = resource_reg.get_mut(resource).map(Option::take) {
            // `OwnedFd` closes the file descriptor when it is dropped.
            //
            // Safety: this RawFd was received through SCM_RIGHTS and ownership
            // is transferred to this process.
            unsafe {
                drop(OwnedFd::from_raw_fd(raw_fd));
            }
        }
    }

    let rel = ClientRequest::Release {
        resources: vec![Resource::Fbdev],
    };
    let data = serialize(&rel).expect("serialize release failed");
    stream.write_all(&data).await.expect("write release failed");
    println!("Sent Release");
}
