//! The background reader thread.
//!
//! One thread per [`SgcClient`](crate::SgcClient): it owns the socket and
//! BLOCKING-ly reads one framed message at a time. No poll, no wakeup
//! pipe, no non-blocking sockets:
//!
//! - inbound (server messages) is this thread's whole job;
//! - outbound (app requests) is written synchronously by the app thread
//!   through a shared `Mutex<UnixStream>` (later phases);
//! - teardown unblocks the reader with `shutdown(2)` from the API side —
//!   the blocking read returns EOF and the thread exits.

use std::{
    io::{self, ErrorKind},
    os::{
        fd::RawFd,
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr as StdSocketAddr, UnixStream},
    },
    sync::mpsc,
};

use sendfd::RecvWithFd;
use simple_graphics_protocol::{
    FRAME_HEADER_LEN, ServerMessage, deserialize, parse_frame_header,
};
use tracing::{debug, info, warn};

use crate::{SgcError, SgcEvent};

/// Connect to the controller's abstract socket `@name` (name without the
/// leading NUL) and read its `Advertise`. The socket stays blocking — the
/// reader thread owns it from here on.
pub(crate) fn connect_and_advertise(name: &[u8]) -> Result<UnixStream, SgcError> {
    let addr = StdSocketAddr::from_abstract_name(name).map_err(SgcError::Io)?;
    let mut stream = UnixStream::connect_addr(&addr).map_err(SgcError::ConnectFailed)?;

    let (msg, fds) = read_framed(&mut stream)?;
    if !fds.is_empty() {
        warn!("Advertise carried {} unexpected fds", fds.len());
    }
    match msg {
        ServerMessage::Advertise { available_resources } => {
            info!(
                "connected to @{}; available: {available_resources:?}",
                String::from_utf8_lossy(name)
            );
        }
        other => return Err(SgcError::UnexpectedMessage(other)),
    }

    Ok(stream)
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

/// Run the reader thread: read one framed message, dispatch it (handling
/// lands in later phases), until the connection ends.
pub(crate) fn reader_main(
    mut stream: UnixStream,
    _event_tx: mpsc::Sender<SgcEvent>, // used from phase 5 (revoke events)
) {
    loop {
        match read_framed(&mut stream) {
            Ok((msg, fds)) => {
                if !fds.is_empty() {
                    warn!("received {} fds; fd handling lands in phase 3", fds.len());
                }
                info!("server message {msg:?} (handling lands in later phases)");
            }
            Err(e) => {
                debug!("reader thread exiting: {e}");
                break;
            }
        }
    }
}
