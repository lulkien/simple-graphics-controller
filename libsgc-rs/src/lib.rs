//! Rust client library for the simple-graphics-controller daemon (`@sgc`).
//!
//! Apps link this crate and drive a [`SgcClient`]: connect (returns the
//! advertised resources), acquire a resource (the client HOLDS the granted
//! fd), borrow it via [`SgcClient::fd`] (a dup, never the canonical), and
//! run the event loop with a callback for revokes / re-grants. All wire
//! work (framing, SCM_RIGHTS fd passing, `Ack`, the `Revoke` handshake)
//! happens synchronously on the app's own thread — no background threads,
//! no shared state.
//!
//! Client-side resource ownership: the client owns the granted fds (one
//! per resource) and lends them out; the canonical is dropped on revoke or
//! disconnect, so the library cannot leak a resource the app forgot about.
//!
//! Crate name is `libsgc-rs`; import as `libsgc_rs::...`.

pub mod client;
pub mod error;

pub use client::{SgcClient, SgcEvent};
pub use error::SgcError;
pub use simple_graphics_protocol::{InputResource, Resource};
