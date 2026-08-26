//! Shared protocol types for FD passing over a Unix stream.

pub mod message;
pub mod serialization;

pub use message::{ClientRequest, Resource, ServerMessage};
pub use serialization::{ProtocolError, deserialize, serialize};
