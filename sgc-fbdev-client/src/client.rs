//! The client: a plain synchronous wrapper over the `@sgc` stream, with
//! CLIENT-SIDE RESOURCE OWNERSHIP.
//!
//! The client holds the granted fds (one per resource) and LENDS them out:
//! [`SgcClient::fd`] returns a fresh dup, never the canonical fd. The
//! canonical is owned by the client and dropped when the resource is
//! revoked or the session ends — the library cannot leak a resource the
//! app forgot about.
//!
//! Still single-threaded: one app thread drives acquire + the event loop;
//! borrowers (e.g. the render task) talk to the app via channels and never
//! to the client. The app thread must stay in the event loop while
//! resources are held — the server's 5s revoke/ack deadlines are real.

use std::{
    collections::HashMap,
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
///
/// Owns the granted fds: `acquire` stores them in [`SgcClient::held`],
/// [`SgcClient::fd`] lends dups, revoke/disconnect drops them.
pub struct SgcClient {
    stream: UnixStream,
    /// Canonical fds of the resources we hold, owned by the client. Never
    /// handed out by value — [`SgcClient::fd`] dups.
    held: HashMap<Resource, OwnedFd>,
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
                Ok((
                    Self {
                        stream,
                        held: HashMap::new(),
                    },
                    available_resources,
                ))
            }
            other => Err(SgcError::UnexpectedMessage(other)),
        }
    }

    /// Request `resource` and BLOCK until the server answers. On grant the
    /// fd is stored in `held` (client-owned); the app borrows it via
    /// [`SgcClient::fd`]. On `Deny` nothing is held.
    pub fn acquire(&mut self, resource: Resource) -> Result<(), SgcError> {
        self.write_frame(&ClientRequest::Acquire {
            resources: vec![resource.clone()],
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
                self.held.insert(resource, fd);
                Ok(())
            }
            ServerMessage::Deny { reason } => Err(SgcError::Denied { reason }),
            other => Err(SgcError::UnexpectedMessage(other)),
        }
    }

    /// Borrow `resource`: returns a DUP of the held fd (the client keeps
    /// the canonical). The borrower owns the dup and must drop it when the
    /// resource is revoked (it learns that via `RenderCmd::Stop`).
    pub fn fd(&self, resource: &Resource) -> Result<OwnedFd, SgcError> {
        self.held
            .get(resource)
            .ok_or_else(|| SgcError::NotHeld { resource: resource.clone() })
            .and_then(|fd| fd.try_clone().map_err(SgcError::Io))
    }

    /// The resources this session currently holds.
    pub fn held(&self) -> Vec<Resource> {
        self.held.keys().cloned().collect()
    }

    /// The client's event loop: block reading frames until the connection
    /// ends, driving the render task through its command channel.
    ///
    /// - `Revoke { resources }` -> drop the canonical fds (disown), tell
    ///   the render task to stop, reply `Release` (the revoke-ack; the
    ///   server waits up to 5s for it);
    /// - `Grant` (the unsolicited re-grant) -> Ack it, store the canonical
    ///   fd, lend a dup to the render task;
    /// - disconnected -> disown everything, tell the render task to stop,
    ///   return.
    pub fn start_event_loop(&mut self, render_tx: &mpsc::Sender<RenderCmd>) {
        loop {
            let (msg, fds) = match read_framed(&mut self.stream) {
                Ok(x) => x,
                Err(e) => {
                    println!("connection lost: {e}");
                    // Disown everything we hold: drop the canonicals and
                    // tell the borrowers to drop their dups.
                    for resource in self.held.keys().cloned().collect::<Vec<_>>() {
                        self.held.remove(&resource);
                        let _ = render_tx.send(RenderCmd::Stop { resource });
                    }
                    break;
                }
            };
            match msg {
                ServerMessage::Revoke { resources } => {
                    for resource in &resources {
                        println!("revoked {resource:?}; stopping render");
                        // Drop the canonical; the borrower drops its dup
                        // on Stop.
                        self.held.remove(resource);
                        let _ = render_tx.send(RenderCmd::Stop {
                            resource: resource.clone(),
                        });
                    }
                    // Revoke-ack: Release doubles as the acknowledgment.
                    let _ = self.write_frame(&ClientRequest::Release { resources });
                }
                ServerMessage::Grant { resources } => {
                    if fds.len() != 1 {
                        eprintln!("grant carried {} fds; ignoring", fds.len());
                        continue;
                    }
                    let Some(resource) = resources.first() else {
                        eprintln!("grant without resources; ignoring");
                        continue;
                    };
                    let resource = resource.clone();
                    // Safety: SCM_RIGHTS transfers ownership to us.
                    let fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
                    let _ = self.write_frame(&ClientRequest::Ack);
                    // Re-grant: store the canonical, lend a dup to render.
                    self.held.insert(resource.clone(), fd);
                    println!("re-granted {resource:?}; drawing");
                    match self.fd(&resource) {
                        Ok(fd) => {
                            let _ = render_tx.send(RenderCmd::Draw { resource, fd });
                        }
                        Err(e) => eprintln!("lend failed: {e}"),
                    }
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
