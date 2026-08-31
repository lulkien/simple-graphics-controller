//! Typed errors for the client library.
//!
//! `SgcError` is the one error type crossing the library boundary; apps
//! match on it directly (no `anyhow` inside a public API).

use simple_graphics_protocol::{ProtocolError, Resource, ServerMessage};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SgcError {
    /// Could not connect to the controller's abstract socket `@sgc`.
    #[error("failed to connect to @sgc: {0}")]
    ConnectFailed(#[source] std::io::Error),

    /// The server refused an acquire (e.g. first-owner policy, or the
    /// resource is not registered). Carries the server's reason string.
    #[error("acquire denied: {reason}")]
    Denied { reason: String },

    /// Tried to borrow a resource this session does not hold.
    #[error("resource not held: {resource:?}")]
    NotHeld { resource: Resource },

    #[error("resource not available: {resource:?}")]
    NotAvailable { resource: Resource },

    /// Message encoding/decoding failed.
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),

    /// The server sent a message we did not expect at this point in the
    /// conversation (e.g. a Grant instead of the opening Advertise).
    #[error("unexpected server message: {0:?}")]
    UnexpectedMessage(ServerMessage),

    /// Socket I/O failed (EOF means the connection is over).
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}
