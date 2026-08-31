//! The client session: [`SgcClient`] and its [`SgcEvent`]s.
//!
//! CLIENT-SIDE RESOURCE OWNERSHIP: the client holds the granted fds (one
//! per resource) and LENDS them out — [`SgcClient::fd`] returns a fresh
//! dup, never the canonical fd. The canonical is owned by the client and
//! dropped when the resource is revoked or the session ends — the library
//! cannot leak a resource the app forgot about.
//!
//! Single-threaded: one app thread drives acquire + the event loop;
//! borrowers (e.g. a render task) talk to the app via channels and never
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
};

use sendfd::RecvWithFd;
use simple_graphics_protocol::{
    ClientRequest, FRAME_HEADER_LEN, Resource, ServerMessage, deserialize, parse_frame_header,
    serialize_framed,
};

use crate::error::SgcError;

/// Events delivered to the app through the event-loop callback.
///
/// The library maps the wire protocol to these; the app maps them to its
/// own commands (e.g. a render task's `Draw`/`Stop`).
#[derive(Debug)]
pub enum SgcEvent {
    /// The server revoked `resource`: drop the fd and stop drawing. The
    /// library already dropped the canonical; it replies `Release` (the
    /// revoke-ack; the server waits up to 5s for it) once the callback
    /// returns.
    Revoked { resource: Resource },
    /// The server re-granted `resource` after a revoke (we were requeued):
    /// a fresh dup of the device fd, owned by the app, valid until the
    /// next [`SgcEvent::Revoked`]. The protocol `Ack` was already sent by
    /// the library — no second `acquire` needed.
    Granted { resource: Resource, fd: OwnedFd },
}

/// A connected session to the graphics controller, driven by one thread.
///
/// Owns the granted fds: `acquire` stores them in [`SgcClient::held`],
/// [`SgcClient::fd`] lends dups, revoke/disconnect drops them.
#[derive(Debug)]
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
        Self::connect_at(b"sgc")
    }

    /// Like [`SgcClient::connect`], but to an arbitrary abstract socket
    /// name (used by tests; the name must not include the leading NUL).
    pub(crate) fn connect_at(name: &[u8]) -> Result<(Self, Vec<Resource>), SgcError> {
        let addr = StdSocketAddr::from_abstract_name(name).map_err(SgcError::Io)?;
        let mut stream = UnixStream::connect_addr(&addr).map_err(SgcError::ConnectFailed)?;

        let (msg, fds) = read_framed(&mut stream)?;
        if !fds.is_empty() {
            eprintln!("warning: Advertise carried {} unexpected fds", fds.len());
        }
        match msg {
            ServerMessage::Advertise {
                available_resources,
            } => {
                println!(
                    "connected to @{}; available: {available_resources:?}",
                    String::from_utf8_lossy(name)
                );
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
            resource: resource.clone(),
        })?;

        let (msg, fds) = read_framed(&mut self.stream)?;
        match msg {
            ServerMessage::Grant { resource: granted } => {
                if fds.len() != 1 {
                    return Err(SgcError::Io(io::Error::new(
                        ErrorKind::InvalidData,
                        format!("grant carried {} fds, expected 1", fds.len()),
                    )));
                }
                if granted != resource {
                    return Err(SgcError::UnexpectedMessage(ServerMessage::Grant {
                        resource: granted,
                    }));
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
    /// resource is revoked (it learns that via [`SgcEvent::Revoked`]).
    pub fn fd(&self, resource: &Resource) -> Result<OwnedFd, SgcError> {
        self.held
            .get(resource)
            .ok_or_else(|| SgcError::NotHeld {
                resource: resource.clone(),
            })
            .and_then(|fd| fd.try_clone().map_err(SgcError::Io))
    }

    /// The resources this session currently holds.
    pub fn held(&self) -> Vec<Resource> {
        self.held.keys().cloned().collect()
    }

    /// The client's event loop: block reading frames until the connection
    /// ends, calling `on_event` for each.
    ///
    /// - `Revoke { resource }` -> drop the canonical fd, emit
    ///   [`SgcEvent::Revoked`], reply `Release` (the revoke-ack; the
    ///   server waits up to 5s for it);
    /// - `Grant { resource }` (the unsolicited re-grant) -> Ack it, store
    ///   the canonical fd, emit [`SgcEvent::Granted`] with a dup;
    /// - disconnected -> disown everything (drop the canonicals, emit
    ///   `Revoked` for each), return.
    pub fn start_event_loop<F>(&mut self, mut on_event: F)
    where
        F: FnMut(SgcEvent),
    {
        loop {
            let (msg, fds) = match read_framed(&mut self.stream) {
                Ok(x) => x,
                Err(e) => {
                    println!("connection lost: {e}");
                    // Disown everything we hold: drop the canonicals and
                    // tell the borrowers to drop their dups.
                    for resource in self.held.keys().cloned().collect::<Vec<_>>() {
                        self.held.remove(&resource);
                        on_event(SgcEvent::Revoked { resource });
                    }
                    break;
                }
            };
            match msg {
                ServerMessage::Revoke { resource } => {
                    println!("revoked {resource:?}; stopping render");
                    // Drop the canonical; the borrower drops its dup
                    // on Revoked.
                    self.held.remove(&resource);
                    on_event(SgcEvent::Revoked {
                        resource: resource.clone(),
                    });
                    // Revoke-ack: Release doubles as the acknowledgment.
                    let _ = self.write_frame(&ClientRequest::Release { resource });
                }
                ServerMessage::Grant { resource } => {
                    if fds.len() != 1 {
                        eprintln!("grant carried {} fds; ignoring", fds.len());
                        continue;
                    }
                    // Safety: SCM_RIGHTS transfers ownership to us.
                    let fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
                    let _ = self.write_frame(&ClientRequest::Ack);
                    // Re-grant: store the canonical, lend a dup to the app.
                    self.held.insert(resource.clone(), fd);
                    println!("re-granted {resource:?}; drawing");
                    match self.fd(&resource) {
                        Ok(fd) => on_event(SgcEvent::Granted { resource, fd }),
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

#[cfg(test)]
mod tests {
    use super::*;
    use sendfd::SendWithFd;
    use std::{
        io::{Read, Write},
        os::{
            fd::AsRawFd,
            linux::net::SocketAddrExt,
            unix::net::{SocketAddr, UnixListener},
        },
        sync::mpsc,
        thread,
    };

    /// Minimal fake controller: bind an abstract listener, signal `ready`,
    /// accept one client, send `Advertise`, optionally reply with `reply`
    /// (written immediately — the client's request sits in the socket
    /// buffer), then hold the connection open until the client drops.
    fn fake_server(
        name: &'static [u8],
        reply: Option<(ServerMessage, Option<RawFd>)>,
    ) -> (thread::JoinHandle<()>, mpsc::Receiver<()>) {
        let (ready_tx, ready_rx) = mpsc::sync_channel(0);
        let handle = thread::spawn(move || {
            let addr = SocketAddr::from_abstract_name(name).expect("address");
            let listener = UnixListener::bind_addr(&addr).expect("bind");
            ready_tx.send(()).expect("ready signal");
            let (mut stream, _) = listener.accept().expect("accept");
            let adv = serialize_framed(&ServerMessage::Advertise {
                available_resources: vec![Resource::Fbdev],
            })
            .expect("serialize advertise");
            stream.write_all(&adv).expect("write advertise");
            if let Some((msg, fd)) = reply {
                let frame = serialize_framed(&msg).expect("serialize reply");
                match fd {
                    Some(fd) => {
                        stream
                            .send_with_fd(&frame, &[fd])
                            .expect("send reply with fd");
                    }
                    None => stream.write_all(&frame).expect("write reply"),
                }
            }
            // Drain requests/acks until the client drops (EOF), then exit.
            let mut buf = [0u8; 64];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
        });
        (handle, ready_rx)
    }

    #[test]
    fn connect_fails_cleanly_when_no_server() {
        let err = SgcClient::connect_at(b"sgc-test-nowhere").expect_err("must fail");
        assert!(
            matches!(err, SgcError::ConnectFailed(_)),
            "expected ConnectFailed, got {err:?}"
        );
    }

    #[test]
    fn connect_reads_advertise_and_returns_resources() {
        let (server, ready) = fake_server(b"sgc-test-adv", None);
        ready.recv().expect("server ready");
        let (client, available) = SgcClient::connect_at(b"sgc-test-adv").expect("connect");
        assert_eq!(available, vec![Resource::Fbdev]);
        assert!(client.held().is_empty());
        drop(client);
        server.join().expect("server thread finished");
    }

    #[test]
    fn acquire_denied_surfaces_reason() {
        let (server, ready) = fake_server(
            b"sgc-test-deny",
            Some((
                ServerMessage::Deny {
                    reason: "first-owner policy".into(),
                },
                None,
            )),
        );
        ready.recv().expect("server ready");
        let (mut client, _) = SgcClient::connect_at(b"sgc-test-deny").expect("connect");
        let err = client.acquire(Resource::Fbdev).expect_err("must be denied");
        assert!(
            matches!(err, SgcError::Denied { ref reason } if reason == "first-owner policy"),
            "expected Denied, got {err:?}"
        );
        assert!(client.held().is_empty(), "nothing held on deny");
        drop(client);
        server.join().expect("server thread finished");
    }

    #[test]
    fn acquire_grant_holds_fd_and_lends_dup() {
        let devnull = std::fs::File::open("/dev/null").expect("open /dev/null");
        let (server, ready) = fake_server(
            b"sgc-test-grant",
            Some((
                ServerMessage::Grant {
                    resource: Resource::Fbdev,
                },
                Some(devnull.as_raw_fd()),
            )),
        );
        ready.recv().expect("server ready");
        let (mut client, _) = SgcClient::connect_at(b"sgc-test-grant").expect("connect");
        client.acquire(Resource::Fbdev).expect("grant");
        assert_eq!(client.held(), vec![Resource::Fbdev]);
        let fd = client.fd(&Resource::Fbdev).expect("lend");
        assert!(fd.as_raw_fd() >= 0, "lent fd must be valid");
        drop(client);
        server.join().expect("server thread finished");
    }
}
