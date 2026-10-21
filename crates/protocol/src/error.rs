//! Protocol-level errors and their mapping onto wire [`ErrorCode`]s.

use thiserror::Error;

use crate::message::ErrorCode;
use crate::version::ProtocolVersion;

/// An encoding, decoding, validation, or negotiation failure.
///
/// [`ProtocolError::code`] maps every variant onto the [`ErrorCode`] a peer
/// should receive in a wire `Error` message.
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// A frame exceeds [`crate::limits::MAX_FRAME_BYTES`]. Detected before
    /// parsing (and before allocation on the transport receive path).
    #[error("frame of {len} bytes exceeds the {max}-byte limit")]
    Oversized {
        /// Observed or declared frame length in bytes.
        len: usize,
        /// The enforced maximum in bytes.
        max: usize,
    },

    /// The payload is not a valid message: bad JSON, an unknown field
    /// (`deny_unknown_fields`), a missing field, or a bad base64 string.
    #[error("malformed message: {0}")]
    Malformed(#[source] serde_json::Error),

    /// A field violates one of the [`crate::limits`] bounds.
    #[error("{field}: {len} exceeds the limit of {max}")]
    LimitExceeded {
        /// Which field or count violated its bound.
        field: &'static str,
        /// Observed length or count.
        len: u64,
        /// The enforced maximum.
        max: u64,
    },

    /// The peer offered a protocol version with a different major.
    #[error("incompatible protocol version: local {local}, peer {peer}")]
    IncompatibleVersion {
        /// Our version.
        local: ProtocolVersion,
        /// The version the peer offered.
        peer: ProtocolVersion,
    },

    /// A [`crate::RemotePath`] failed a structural safety check (traversal, a
    /// UNC or device namespace, a reserved device name, an alternate-data-stream
    /// colon, or too many components). Rejected on **both** ends in
    /// [`crate::Message::validate`] before any operation runs (ADR-0012).
    #[error("{field}: unsafe path ({reason})")]
    InvalidPath {
        /// Which path field was rejected.
        field: &'static str,
        /// Why the path was rejected.
        reason: &'static str,
    },
}

impl ProtocolError {
    /// The wire [`ErrorCode`] a peer should receive for this failure.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::Oversized { .. } => ErrorCode::OversizedMessage,
            Self::Malformed(_) => ErrorCode::MalformedMessage,
            Self::LimitExceeded { .. } => ErrorCode::LimitExceeded,
            Self::IncompatibleVersion { .. } => ErrorCode::IncompatibleVersion,
            Self::InvalidPath { .. } => ErrorCode::InvalidPath,
        }
    }
}
