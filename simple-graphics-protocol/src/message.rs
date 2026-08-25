use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Resource {
    Fbdev,
    Drm { path: PathBuf },
    Input { path: PathBuf },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClientRequest {
    RequestResource { resource: Resource },
    Release { resource: Resource },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ServerMessage {
    Grant,
    Deny { reason: String },
    Ack,
    Revoke { resource: Resource },
}
