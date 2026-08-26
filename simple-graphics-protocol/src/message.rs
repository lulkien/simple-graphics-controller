//! Messages and resource types used by the client-server protocol.
//!
//! Clients use [`ClientRequest`] to announce themselves, acquire resources,
//! release resources, and acknowledge server messages. Servers use
//! [`ServerMessage`] to advertise available resources, grant or deny requests,
//! and revoke previously granted resources.

use serde::{Deserialize, Serialize};

/// A resource that can be managed by the server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Resource {
    /// The framebuffer device resource.
    Fbdev,
}

/// A request sent from a client to the server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClientRequest {
    /// Requests ownership of one or more resources.
    Acquire {
        /// The resources the client wants to acquire.
        resources: Vec<Resource>,
    },
    /// Releases one or more resources previously acquired by the client.
    Release {
        /// The resources the client wants to release.
        resources: Vec<Resource>,
    },
}

/// A message sent from the server to a client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ServerMessage {
    /// Sent on connect to advertise available resources.
    Advertise {
        /// Resources that are currently available for acquisition.
        available_resources: Vec<Resource>,
    },
    /// Grants the client's resource request.
    Grant {
        /// Resources has been granted for the client.
        resources: Vec<Resource>,
    },
    /// Denies the client's request.
    Deny {
        /// Human-readable explanation for why the request was denied.
        reason: String,
    },
    /// Revokes resources previously granted to the client.
    Revoke {
        /// Resources the client must release.
        resources: Vec<Resource>,
    },
}
