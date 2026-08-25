//! Shared protocol types for FD passing over a Unix stream.

pub mod message;
pub mod serialization;

pub use message::{Request, Resource, Response};
pub use serialization::{ProtocolError, deserialize, serialize};

