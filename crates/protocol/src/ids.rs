//! Session, request, and job identifiers.
//!
//! All identifiers serialize transparently (a UUID string or a plain
//! integer) so wire frames stay small and unambiguous.

use core::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A session identifier: a version 4 UUID minted by the responder during
/// negotiation. Every request, response, and event carries it so results
/// stay attributable to one target session (REQ-F-016).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub Uuid);

impl SessionId {
    /// Mints a new random (version 4) session identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl core::str::FromStr for SessionId {
    type Err = uuid::Error;

    /// Parses a session identifier from its canonical UUID text. Used to
    /// recognise a staging directory left by a prior run (its name is the
    /// session id) and to route an operation to a named target session.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// A request identifier, unique per session, chosen by the requester and
/// echoed on the matching response or error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(pub u64);

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// A job identifier assigned by the agent when an `Exec` starts; output,
/// exit, stdin, and cancellation messages reference it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobId(pub u64);

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}
