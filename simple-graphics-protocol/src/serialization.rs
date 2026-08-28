use rmp_serde::{decode, encode};
use thiserror::Error;

use crate::{ClientRequest, ServerMessage};

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("Encode: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("Decode: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("frame header declares {0} bytes, exceeding max of {MAX_FRAME_PAYLOAD}")]
    FrameTooLarge(usize),
}

/// Serialize a request or response to MessagePack bytes.
pub fn serialize<T: serde::Serialize>(msg: &T) -> Result<Vec<u8>, ProtocolError> {
    let mut buf = Vec::new();
    encode::write_named(&mut buf, msg)?;
    Ok(buf)
}

/// Deserialize a request or response from MessagePack bytes.
pub fn deserialize<T: serde::de::DeserializeOwned>(data: &[u8]) -> Result<T, ProtocolError> {
    Ok(decode::from_read(data)?)
}

// --- Framing ---
//
// Every message on the wire is prefixed with a 4-byte big-endian length
// header (payload = the MessagePack bytes). Stream sockets do not preserve
// message boundaries, so the header lets readers read exactly one message.

pub const FRAME_HEADER_LEN: usize = 4;
pub const MAX_FRAME_PAYLOAD: usize = 1024 * 1024;

/// Serialize a message and prefix it with a 4-byte big-endian length header.
pub fn serialize_framed<T: serde::Serialize>(msg: &T) -> Result<Vec<u8>, ProtocolError> {
    let payload = serialize(msg)?;
    let mut buf = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Parse a 4-byte big-endian frame header into the payload length.
pub fn parse_frame_header(header: &[u8; FRAME_HEADER_LEN]) -> Result<usize, ProtocolError> {
    let len = u32::from_be_bytes(*header) as usize;
    if len > MAX_FRAME_PAYLOAD {
        return Err(ProtocolError::FrameTooLarge(len));
    }
    Ok(len)
}

// Type aliases used by callers to keep signatures readable.
pub type WireRequest = ClientRequest;
pub type WireResponse = ServerMessage;
