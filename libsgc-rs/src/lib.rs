//! Rust client library for the simple-graphics-controller daemon (`@sgc`).
//!
//! Apps link this crate and drive a [`SgcClient`](client::SgcClient):
//! connect, acquire a resource (returns an owned fd), release it; dropping
//! the client shuts the session down. All wire work (framing, SCM_RIGHTS
//! fd passing, `Ack`, the `Revoke` handshake) happens on the library's own
//! thread — the app only reacts to the [`SgcEvent`]s it drains.
//!
//! Crate name is `libsgc-rs`; import as `libsgc_rs::...`.

pub mod client;
pub mod comm;
pub mod error;

pub use client::{SgcClient, SgcEvent};
pub use error::SgcError;
pub use simple_graphics_protocol::Resource;
