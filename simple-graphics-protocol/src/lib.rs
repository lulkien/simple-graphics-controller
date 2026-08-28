//! Shared protocol types for FD passing over a Unix stream.

pub mod message;
pub mod serialization;

pub use message::{ClientRequest, Resource, ServerMessage};
pub use serialization::{
    FRAME_HEADER_LEN, MAX_FRAME_PAYLOAD, ProtocolError, deserialize, parse_frame_header,
    serialize, serialize_framed,
};
