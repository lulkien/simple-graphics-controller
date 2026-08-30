//! Typed errors for the client library.
//!
//! `SgcError` is the one error type crossing the library boundary; apps
//! match on it directly (no `anyhow` inside a public API).

use simple_graphics_protocol::{ProtocolError, ServerMessage};
use thiserror::Error;

use crate::Resource;

#[derive(Debug, Error)]
pub enum SgcError {
    /// Could not connect to the controller's abstract socket `@sgc`.
    #[error("failed to connect to @sgc: {0}")]
    ConnectFailed(#[source] std::io::Error),

    /// The handle is not connected (an operation was attempted before
    /// `connect` succeeded, or after `de_init`).
    #[error("not connected; call sgc_rs::connect first")]
    NotConnected,

    /// The server refused an acquire (e.g. first-owner policy, or the
    /// resource is already owned by this client). Carries the server's
    /// reason string.
    #[error("acquire denied: {reason}")]
    Denied { reason: String },

    /// Message encoding/decoding failed.
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),

    /// The server sent a message we did not expect at this point in the
    /// conversation (e.g. a Grant instead of the opening Advertise).
    #[error("unexpected server message: {0:?}")]
    UnexpectedMessage(ServerMessage),

    /// Socket I/O failed.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// The background communication thread terminated unexpectedly (e.g.
    /// the server died). The handle is unusable; call `de_init`.
    #[error("the communication thread terminated")]
    HandlePoisoned,

    /// A release/revoke referenced a resource this handle does not hold.
    #[error("{resource:?} is not held by this handle")]
    ResourceNotHeld { resource: Resource },
}
