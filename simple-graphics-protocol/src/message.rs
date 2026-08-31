//! Messages and resource types used by the client-server protocol.
//!
//! Clients use [`ClientRequest`] to announce themselves, acquire resources,
//! release resources, and acknowledge server messages. Servers use
//! [`ServerMessage`] to advertise available resources, grant or deny requests,
//! and revoke previously granted resources.

use serde::{Deserialize, Serialize};

/// A resource that can be managed by the server, grouped by kind.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Resource {
    /// A display/output device.
    Display(DisplayResource),
    /// An input device.
    Input(InputResource),
}

/// Display resources.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum DisplayResource {
    /// The framebuffer device (/dev/fb0).
    Fbdev,
    /// A DRM render/display card (/dev/dri/cardN).
    Drm { card: u8 },
}

/// Input resources. The index counts devices of the same class, so the
/// registry can hold several mice/keyboards/touchscreens at once.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum InputResource {
    /// A mouse-like device (relative axes + buttons).
    Mouse(u8),
    /// A keyboard-like device (key codes).
    Keyboard(u8),
    /// A touch device (absolute axes, single- or multi-touch).
    Touch(u8),
}

/// A request sent from a client to the server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClientRequest {
    /// Requests ownership of one resource.
    Acquire {
        /// The resource the client wants to acquire.
        resource: Resource,
    },
    /// Releases one resource previously acquired by the client.
    Release {
        /// The resource the client wants to release.
        resource: Resource,
    },
    /// Acknowledges receipt of a [`ServerMessage::Grant`] and its file
    /// descriptors. The server waits for this within a timeout after
    /// granting; without it the server cannot tell a delivered grant from a
    /// lost one.
    Ack,
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
        /// The resource granted for the client, with one fd.
        resource: Resource,
    },
    /// Denies the client's request.
    Deny {
        /// Human-readable explanation for why the request was denied.
        reason: String,
    },
    /// Revokes a resource previously granted to the client.
    Revoke {
        /// The resource the client must release.
        resource: Resource,
    },
}
