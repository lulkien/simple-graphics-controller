use nix::libc::pid_t;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Resource {
    Fbdev,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClientRequest {
    Hello { pid: pid_t },
    Request { resources: Vec<Resource> },
    Release { resources: Vec<Resource> },
    Ack,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ServerMessage {
    Hello,
    Grant,
    Deny { reason: String },
    Revoke { resources: Vec<Resource> },
}
