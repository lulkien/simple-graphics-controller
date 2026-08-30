//! The client session: [`SgcClient`], its [`SgcEvent`]s, and teardown.
//!
//! One [`SgcClient`] is a cheap, cloneable handle to a shared session
//! (connect + the background reader thread). Dropping the last clone shuts
//! the session down via shutdown(2) + join — no `de_init` to forget.

use std::{
    net::Shutdown,
    os::fd::OwnedFd,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver},
    },
    thread::JoinHandle,
};

use simple_graphics_protocol::Resource;

use crate::{comm, error::SgcError};

/// Events delivered to the app through the event receiver returned by
/// [`SgcClient::connect`]. The receiver is the channel between the
/// library's thread and the app's main/render threads — the re-granted fd
/// travels in it ([`OwnedFd`] is `Send`).
#[derive(Debug)]
pub enum SgcEvent {
    /// The server re-granted the resource after a revoke (we were
    /// requeued). Use the carried fd immediately — the server handed the
    /// resource back on its own, no [`SgcClient::acquire`] needed. The
    /// protocol `Ack` was already sent by the library.
    /// (Grants that answer an `acquire` are returned by that call, not
    /// delivered here.)
    Granted {
        resource: Resource,
        /// A fresh dup of the device fd, owned by the app. Valid until the
        /// next [`SgcEvent::Revoked`].
        fd: OwnedFd,
    },
    /// The server revoked the resource: drop the fd and stop drawing. The
    /// revoke-ack `Release` was already sent by the library — the resource
    /// is disowned; do NOT call [`SgcClient::release`].
    Revoked { resource: Resource },
}

/// A connected session to the graphics controller.
///
/// The session lives until the client is dropped (the last clone shuts the
/// reader thread down via shutdown(2) and joins it).
#[derive(Debug, Clone)]
pub struct SgcClient {
    inner: Arc<ClientInner>,
}
#[derive(Debug)]
struct ClientInner {
    /// Clone of the socket used by `Drop` to unblock the reader thread:
    /// `shutdown(2)` makes its blocking read return EOF.
    shutdown: std::os::unix::net::UnixStream,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl SgcClient {
    /// Connect to the controller's abstract socket `@sgc` and spawn the
    /// background reader thread. Returns the client plus an event receiver
    /// the app drains from its own loop.
    pub fn connect() -> Result<(Self, Receiver<SgcEvent>), SgcError> {
        Self::connect_at(b"sgc")
    }

    /// Like [`SgcClient::connect`], but to an arbitrary abstract socket
    /// name (used by tests; the name must not include the leading NUL).
    pub(crate) fn connect_at(name: &[u8]) -> Result<(Self, Receiver<SgcEvent>), SgcError> {
        let (event_tx, event_rx) = mpsc::channel();
        let stream = comm::connect_and_advertise(name)?;
        let shutdown = stream.try_clone().map_err(SgcError::Io)?;

        let thread = std::thread::Builder::new()
            .name("sgc-reader".into())
            .spawn(move || comm::reader_main(stream, event_tx))
            .map_err(SgcError::Io)?;

        let inner = Arc::new(ClientInner {
            shutdown,
            join: Mutex::new(Some(thread)),
        });

        Ok((Self { inner }, event_rx))
    }

    /// Request `resource` from the server and BLOCK until the outcome is
    /// known:
    ///
    /// - `Ok(OwnedFd)` — granted; the fd is yours (dropping it closes it).
    /// - `Err(Denied { reason })` — refused (first-owner policy, "already
    ///   owned by this client", ...); nothing is held.
    /// - `Err(HandlePoisoned)` — the session died while waiting.
    ///
    /// The wire work happens on the library's own thread; this call waits
    /// on the reply. Call once per acquisition — at startup, or again
    /// after a voluntary [`SgcClient::release`].
    ///
    /// After a [`SgcEvent::Revoked`], drop the fd and stop drawing; the
    /// re-grant arrives on its own as [`SgcEvent::Granted`] with a fresh
    /// fd — no second `acquire`.
    ///
    /// (Lands in the next phase.)
    pub fn acquire(&self, _resource: Resource) -> Result<OwnedFd, SgcError> {
        Err(SgcError::NotConnected)
    }

    /// Voluntarily hand `resource` back to the server; returns once the
    /// `Release` is written. A later [`SgcClient::acquire`] re-requests
    /// it. Do NOT call this in response to [`SgcEvent::Revoked`] — the
    /// library already sent the revoke-ack `Release` and disowned the
    /// resource.
    ///
    /// (Lands in a later phase.)
    pub fn release(&self, _resource: Resource) -> Result<(), SgcError> {
        Err(SgcError::NotConnected)
    }
}

impl Drop for SgcClient {
    fn drop(&mut self) {
        // Only the last clone shuts the session down: shutdown(2) makes the
        // reader's blocking read return EOF, then join it.
        if let Some(handle) = self
            .inner
            .join
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = self.inner.shutdown.shutdown(Shutdown::Both);
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::{
            linux::net::SocketAddrExt,
            unix::net::{SocketAddr, UnixListener},
        },
        thread,
    };
    use simple_graphics_protocol::{ServerMessage, serialize_framed};

    const TEST_NAME: &[u8] = b"sgc-test-comm";

    /// Minimal fake controller: accept one client, send Advertise, then
    /// read until the client goes away. Signals via `ready` once the
    /// listener is bound.
    fn fake_server() -> (thread::JoinHandle<()>, mpsc::Receiver<()>) {
        let (ready_tx, ready_rx) = mpsc::sync_channel(0);
        let handle = thread::spawn(move || {
            let addr = SocketAddr::from_abstract_name(TEST_NAME).expect("address");
            let listener = UnixListener::bind_addr(&addr).expect("bind");
            ready_tx.send(()).expect("ready signal");
            let (mut stream, _) = listener.accept().expect("accept");
            let adv = serialize_framed(&ServerMessage::Advertise {
                available_resources: vec![Resource::Fbdev],
            })
            .expect("serialize");
            stream.write_all(&adv).expect("write advertise");
            let mut buf = [0u8; 64];
            let _ = stream.read(&mut buf); // hold open until client drops
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
    fn connect_reads_advertise_and_drop_shuts_down() {
        let (server, ready) = fake_server();
        ready.recv().expect("server ready");
        let (client, _events) = SgcClient::connect_at(TEST_NAME).expect("connect");
        // Drop sends Shutdown; the comm thread joins and closes the socket,
        // which unblocks the fake server's read.
        drop(client);
        server.join().expect("server thread finished");
    }
}
