use rmp_serde::{decode, encode};
use thiserror::Error;

use crate::{ClientRequest, ServerMessage};

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("Encode: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("Decode: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
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

// Type aliases used by callers to keep signatures readable.
pub type WireRequest = ClientRequest;
pub type WireResponse = ServerMessage;
