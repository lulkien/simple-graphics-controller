//! Client library for `simple-graphics-controller`.
//!
//! Apps link this crate and drive a [`SgcClient`]:
//!
//! - [`SgcClient::connect`] — connect to the controller at `@sgc` and spawn
//!   the background communication thread;
//! - [`SgcClient::acquire`] — block until a resource is granted, returning
//!   an owned fd ([`OwnedFd`], RAII: dropping it closes it);
//! - [`SgcClient::release`] — hand the resource back to the server;
//! - dropping the client shuts the session down (no `de_init` to forget).
//!
//! All protocol work (framing, SCM_RIGHTS fd passing, `Ack`, the `Revoke`
//! handshake, re-grant after being requeued) happens on the library's own
//! thread. The app only reacts to [`SgcEvent`]s drained from the receiver
//! returned by [`SgcClient::connect`] — it never sees the wire.
//!
//! Crate name is `libsgc-rs`; import as `libsgc_rs::...`.

use std::{os::fd::OwnedFd, sync::mpsc};

pub mod error;

pub use error::SgcError;
pub use simple_graphics_protocol::Resource;

/// Ownership-change events delivered to the app through the event receiver
/// returned by [`SgcClient::connect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SgcEvent {
    /// The server revoked the resource: drop the `OwnedFd` you got from
    /// [`SgcClient::acquire`] and stop drawing.
    Revoked { resource: Resource },
    /// The resource was re-granted after a revoke (the server requeued us).
    /// Call [`SgcClient::acquire`] again to receive the new fd.
    Granted { resource: Resource },
}

/// A connected session to the graphics controller.
///
/// Inert until the communication thread lands (next phase). All operations
/// take `&SgcClient`; the session lives until the client is dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SgcClient {
    _private: (),
}

impl SgcClient {
    /// Connect to `@sgc` and spawn the background communication thread.
    /// Returns the client plus an event receiver the app drains from its
    /// own loop (never from a callback — there is none).
    pub fn connect() -> Result<(Self, mpsc::Receiver<SgcEvent>), SgcError> {
        // Landed in the next phase (comm thread).
        Err(SgcError::NotConnected)
    }

    /// Block until `resource` is granted. The returned [`OwnedFd`] is owned
    /// by the caller — dropping it closes it. On [`SgcEvent::Revoked`],
    /// drop the fd and call `acquire` again for the next grant.
    pub fn acquire(&self, _resource: Resource) -> Result<OwnedFd, SgcError> {
        Err(SgcError::NotConnected)
    }

    /// Hand `resource` back to the server.
    pub fn release(&self, _resource: Resource) -> Result<(), SgcError> {
        Err(SgcError::NotConnected)
    }
}

impl Drop for SgcClient {
    fn drop(&mut self) {
        // The comm thread is joined in the phase that adds it.
    }
}
