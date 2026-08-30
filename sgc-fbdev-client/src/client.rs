//! The client: a plain synchronous wrapper over the `@sgc` stream.
//!
//! No background threads, no shared state: the client is driven by ONE app
//! thread, which blocks in [`SgcClient::acquire`] / 
//! [`SgcClient::start_event_loop`]. The app must keep that thread on the
//! socket while it holds the resource — the server's revoke/ack timeouts
//! are real deadlines. Other threads (e.g. the render task) talk to the
//! app via channels, never to the client.

use std::{
    io::{self, ErrorKind, Write},
    os::{
        fd::{FromRawFd, OwnedFd, RawFd},
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr as StdSocketAddr, UnixStream},
    },
    sync::mpsc,
};

use sendfd::RecvWithFd;
use simple_graphics_protocol::{
    ClientRequest, FRAME_HEADER_LEN, Resource, ServerMessage, deserialize, parse_frame_header,
    serialize_framed,
};

use crate::error::SgcError;
use crate::render::RenderCmd;

/// A connected session to the graphics controller, driven by one thread.
pub struct SgcClient {
    stream: UnixStream,
}

impl SgcClient {
    /// Connect to the controller's abstract socket `@sgc` and read its
    /// `Advertise`. Returns the client plus the advertised resources.
    pub fn connect() -> Result<(Self, Vec<Resource>), SgcError> {
        let addr = StdSocketAddr::from_abstract_name(b"sgc").map_err(SgcError::Io)?;
        let mut stream = UnixStream::connect_addr(&addr).map_err(SgcError::ConnectFailed)?;

        let (msg, fds) = read_framed(&mut stream)?;
        if !fds.is_empty() {
            eprintln!("warning: Advertise carried {} unexpected fds", fds.len());
        }
        match msg {
            ServerMessage::Advertise { available_resources } => {
                println!("connected to @sgc; available: {available_resources:?}");
                Ok((Self { stream }, available_resources))
            }
            other => Err(SgcError::UnexpectedMessage(other)),
        }
    }

    /// Request `resource` and BLOCK until the server answers: write
    /// Acquire, read the reply frame, Ack the grant, return the fd.
    pub fn acquire(&mut self, resource: Resource) -> Result<OwnedFd, SgcError> {
        self.write_frame(&ClientRequest::Acquire {
            resources: vec![resource],
        })?;

        let (msg, fds) = read_framed(&mut self.stream)?;
        match msg {
            ServerMessage::Grant { .. } => {
                if fds.len() != 1 {
                    return Err(SgcError::Io(io::Error::new(
                        ErrorKind::InvalidData,
                        format!("grant carried {} fds, expected 1", fds.len()),
                    )));
                }
                // Safety: the fd arrived via SCM_RIGHTS, which transfers
                // ownership to this process.
                let fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
                // Ack the grant: the server waits up to 5s for it.
                self.write_frame(&ClientRequest::Ack)?;
                Ok(fd)
            }
            ServerMessage::Deny { reason } => Err(SgcError::Denied { reason }),
            other => Err(SgcError::UnexpectedMessage(other)),
        }
    }

    /// The client's event loop: block reading frames until the connection
    /// ends, driving the render task through its command channel.
    ///
    /// - `Revoke` -> tell the render task to stop, reply `Release` (the
    ///   revoke-ack; the server waits up to 5s for it);
    /// - `Grant` (the unsolicited re-grant) -> Ack it, hand the fd to the
    ///   render task;
    /// - disconnected -> send `Stop` and return.
    pub fn start_event_loop(&mut self, render_tx: &mpsc::Sender<RenderCmd>) {
        loop {
            let (msg, fds) = match read_framed(&mut self.stream) {
                Ok(x) => x,
                Err(e) => {
                    println!("connection lost: {e}");
                    break;
                }
            };
            match msg {
                ServerMessage::Revoke { resources } => {
                    println!("revoked {resources:?}; stopping render");
                    let _ = render_tx.send(RenderCmd::Stop);
                    // Revoke-ack: Release doubles as the acknowledgment.
                    let _ = self.write_frame(&ClientRequest::Release { resources });
                }
                ServerMessage::Grant { .. } => {
                    if fds.len() != 1 {
                        eprintln!("grant carried {} fds; ignoring", fds.len());
                        continue;
                    }
                    // Safety: SCM_RIGHTS transfers ownership to us.
                    let fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
                    let _ = self.write_frame(&ClientRequest::Ack);
                    println!("re-granted; drawing");
                    let _ = render_tx.send(RenderCmd::Draw(fd));
                }
                other => {
                    eprintln!("unexpected server message: {other:?}; ignoring");
                }
            }
        }
    }

    /// Serialize + write one request frame.
    fn write_frame(&mut self, req: &ClientRequest) -> Result<(), SgcError> {
        let frame = serialize_framed(req)?;
        self.stream.write_all(&frame).map_err(SgcError::Io)
    }
}

/// Blocking framed read; collects any SCM_RIGHTS fds (only `Grant` carries
/// them). `Err` on EOF — the caller treats that as "connection over".
fn read_framed(stream: &mut UnixStream) -> Result<(ServerMessage, Vec<RawFd>), SgcError> {
    let mut fds: Vec<RawFd> = Vec::new();
    let mut fd_buf = [0i32; 16];

    let mut header = [0u8; FRAME_HEADER_LEN];
    let mut got = 0;
    while got < FRAME_HEADER_LEN {
        let (n, nfds) = stream
            .recv_with_fd(&mut header[got..], &mut fd_buf)
            .map_err(SgcError::Io)?;
        if n == 0 {
            return Err(SgcError::Io(io::Error::from(ErrorKind::UnexpectedEof)));
        }
        got += n;
        fds.extend_from_slice(&fd_buf[..nfds]);
    }

    let len = parse_frame_header(&header)?;
    let mut payload = vec![0u8; len];
    let mut got = 0;
    while got < len {
        let (n, nfds) = stream
            .recv_with_fd(&mut payload[got..], &mut fd_buf)
            .map_err(SgcError::Io)?;
        if n == 0 {
            return Err(SgcError::Io(io::Error::from(ErrorKind::UnexpectedEof)));
        }
        got += n;
        fds.extend_from_slice(&fd_buf[..nfds]);
    }

    let msg = deserialize(&payload)?;
    Ok((msg, fds))
}
