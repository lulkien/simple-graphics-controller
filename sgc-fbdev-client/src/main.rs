use std::{
    io::ErrorKind,
    os::{
        fd::{FromRawFd, OwnedFd, RawFd},
        linux::net::SocketAddrExt,
        unix::net::SocketAddr as StdSocketAddr,
    },
};

use linfb::{
    Framebuffer,
    shape::{Alignment, Caption, Color, FontBuilder, Rectangle, Shape},
};
use sendfd::RecvWithFd;
use simple_graphics_protocol::{
    ClientRequest, FRAME_HEADER_LEN, Resource, ServerMessage, deserialize, parse_frame_header,
    serialize_framed,
};
use tokio::{
    io::AsyncWriteExt,
    net::{UnixStream, unix::SocketAddr},
};

type ClientResult<T> = Result<T, Box<dyn std::error::Error>>;

/// Read one framed server message, collecting any SCM_RIGHTS fds.
///
/// All reads go through `recvmsg`: fds are attached to the first bytes of a
/// Grant frame, and a plain read would silently discard them.
async fn read_message(stream: &mut UnixStream) -> ClientResult<(ServerMessage, Vec<RawFd>)> {
    let mut fds: Vec<RawFd> = Vec::new();
    let mut fd_buf = [0i32; 16];

    let mut header = [0u8; FRAME_HEADER_LEN];
    let mut got = 0;
    while got < FRAME_HEADER_LEN {
        stream.readable().await?;
        match stream.recv_with_fd(&mut header[got..], &mut fd_buf) {
            Ok((0, _)) => return Err("server closed connection".into()),
            Ok((n, nfds)) => {
                got += n;
                fds.extend_from_slice(&fd_buf[..nfds]);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e.into()),
        }
    }

    let len = parse_frame_header(&header)?;
    let mut payload = vec![0u8; len];
    let mut got = 0;
    while got < len {
        stream.readable().await?;
        match stream.recv_with_fd(&mut payload[got..], &mut fd_buf) {
            Ok((0, _)) => return Err("server closed connection mid-frame".into()),
            Ok((n, nfds)) => {
                got += n;
                fds.extend_from_slice(&fd_buf[..nfds]);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e.into()),
        }
    }

    let msg = deserialize(&payload)?;
    Ok((msg, fds))
}

/// Draw the demo scene on the given framebuffer fd.
fn draw(fd: RawFd) -> ClientResult<()> {
    let mut framebuffer = Framebuffer::open_with_fd(fd).expect("framebuffer open failed");

    let mut compositor = framebuffer.compositor((255, 255, 255).into());
    compositor
        .add(
            "rect1",
            Rectangle::builder()
                .width(100)
                .height(100)
                .fill_color(Color::hex("#ff000099").unwrap())
                .build()
                .unwrap()
                .at(100, 100),
        )
        .add(
            "rect2",
            Rectangle::builder()
                .width(100)
                .height(100)
                .fill_color(Color::hex("#00ff0099").unwrap())
                .build()
                .unwrap()
                .at(150, 150),
        )
        // .add("image", Image::from_path("image.png").unwrap().at(500, 500))
        .add(
            "wrapped_text",
            Caption::builder()
                .text("Some centered text\nwith newlines".into())
                .size(56)
                .color(Color::hex("#4066b877").unwrap())
                .font(FontBuilder::default().family("monospace").build().unwrap())
                .alignment(Alignment::Center)
                .max_width(650)
                .build()
                .unwrap()
                .at(1000, 300),
        );
    // Compositor is shape, so we can just draw it at the top left angle
    framebuffer.draw(0, 0, &compositor);
    // Really changing screen contents
    framebuffer.flush();

    Ok(())
}

/// Close our dup'd copies of the granted fds (the server keeps its own).
fn close_fds(fds: &[RawFd]) {
    for &raw_fd in fds {
        // Safety: these fds were received through SCM_RIGHTS and ownership
        // is transferred to this process.
        unsafe {
            drop(OwnedFd::from_raw_fd(raw_fd));
        }
    }
}

/// Tell the server we are done with Fbdev (also the revoke acknowledgment).
async fn send_release(stream: &mut UnixStream) -> ClientResult<()> {
    let rel = ClientRequest::Release {
        resources: vec![Resource::Fbdev],
    };
    let data = serialize_framed(&rel)?;
    stream.write_all(&data).await?;
    Ok(())
}

#[tokio::main]
async fn main() -> ClientResult<()> {
    // --hold: keep running after drawing so a second instance can preempt
    // us (Revoke -> Release -> re-Grant -> draw again). Default: draw,
    // release, exit (old linear behavior).
    let hold = std::env::args().any(|arg| arg == "--hold");

    // Connect to abstract namespace socket @sgc
    let std_addr = StdSocketAddr::from_abstract_name(b"sgc").expect("invalid socket address");
    let tokio_addr: SocketAddr = SocketAddr::from(std_addr);
    let mut stream = UnixStream::connect_addr(&tokio_addr)
        .await
        .expect("failed to connect to socket");

    // Read Advertise response (no FDs)
    let (msg, fds) = read_message(&mut stream)
        .await
        .expect("read advertise failed");
    assert!(fds.is_empty(), "Advertise should not carry fds");
    let ServerMessage::Advertise { available_resources } = msg else {
        eprintln!("Expected Advertise, got {msg:?}");
        return Ok(());
    };
    println!("Available resources: {available_resources:?}");

    // Request Fbdev. The answer may come immediately (Grant/Deny) or later
    // (Queued -> Grant via the engine's handoff) — either way the loop
    // below is the single place that handles it.
    let req = ClientRequest::Acquire {
        resources: vec![Resource::Fbdev],
    };
    let data = serialize_framed(&req).expect("serialize request failed");
    stream.write_all(&data).await.expect("write failed");
    println!("Sent Acquire for Fbdev");

    // Fds we currently hold (dup'd copies of the server's grant).
    let mut held_fds: Vec<RawFd> = Vec::new();

    loop {
        tokio::select! {
            _ = stream.readable() => {
                let (msg, fds) = match read_message(&mut stream).await {
                    Ok(x) => x,
                    Err(e) => {
                        eprintln!("Server connection lost: {e}");
                        break;
                    }
                };
                match msg {
                    ServerMessage::Grant { resources } => {
                        println!("Granted: {resources:?}");
                        assert_eq!(fds.len(), resources.len(), "one fd per granted resource");
                        held_fds.extend_from_slice(&fds);
                        for &fd in &fds {
                            if let Err(e) = draw(fd) {
                                eprintln!("draw failed: {e}");
                            }
                        }
                        println!("Drawing (hold={hold})");

                        // Acknowledge the grant so the server knows it landed.
                        let ack = serialize_framed(&ClientRequest::Ack).expect("serialize ack failed");
                        stream.write_all(&ack).await.expect("write ack failed");
                        println!("Sent Ack");

                        if !hold {
                            // Old behavior: draw once, hand back, exit.
                            close_fds(&held_fds);
                            held_fds.clear();
                            send_release(&mut stream).await?;
                            println!("Sent Release");
                            break;
                        }
                    }
                    ServerMessage::Revoke { resources } => {
                        println!("Revoked: {resources:?} — releasing");
                        close_fds(&held_fds);
                        held_fds.clear();
                        send_release(&mut stream).await?;
                        println!("Sent Release (revoke-ack); waiting for re-grant");
                        // If the engine requeues us, a Grant arrives without
                        // a preceding Acquire — handled above.
                    }
                    ServerMessage::Deny { reason } => {
                        eprintln!("Denied: {reason}");
                        break;
                    }
                    ServerMessage::Advertise { .. } => {
                        eprintln!("Unexpected duplicate Advertise");
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                println!("Ctrl+C, exiting");
                break;
            }
        }
    }

    Ok(())
}
