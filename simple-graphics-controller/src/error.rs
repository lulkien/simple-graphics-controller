//! Common typed errors for the graphics controller server.
//!
//! Only errors that occur at multiple call sites are typed here (protocol
//! encoding/decoding, socket writes). One-off errors — listener setup, peer
//! credentials, registry lookups — use `anyhow` context at the call site.

use simple_graphics_protocol::ProtocolError;
use std::io;
use thiserror::Error;

pub type ServerResult<T> = Result<T, ServerError>;

#[derive(Debug, Error)]
pub enum ServerError {
    /// Message encoding/decoding: serialize/deserialize failures.
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),

    /// Any socket write: advertise, deny, or grant (with fds).
    #[error("failed to write to client: {0}")]
    Write(#[source] io::Error),

    /// One-off errors (registry lookups, context strings) carried as anyhow.
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}
