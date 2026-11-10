//! The technician's display name (ST-M009-011, protocol minor 10).
//!
//! The console takes its display name from the technician's operating-system
//! account (the full name, else the user name) and sends it in the discovery
//! advertisement and, as an optional field, in the session handshake
//! ([`crate::PairingMessage::Nonce`]). The customer side shows it as **what the
//! technician's computer says**: it is a label the technician's machine chose,
//! never verified, never an identity, and it grants no authority. Trust in the
//! peer still comes only from the Session Approval Code (REQ-S-006).
//!
//! A [`DisplayName`] can only be built through [`DisplayName::new`], which trims
//! it and enforces the single-line, bounded shape, so a malformed name is
//! rejected when decoded from the wire like any other out-of-contract field.

use core::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::limits::MAX_DISPLAY_NAME_CHARS;
use crate::purpose::is_disallowed;

/// A validated, trimmed, single-line display name: 1 to
/// [`MAX_DISPLAY_NAME_CHARS`] characters, with no control characters, line or
/// paragraph separators, or bidirectional formatting characters, so it renders
/// as one honest line on the customer surface.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DisplayName(String);

/// Why a display name was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DisplayNameError {
    /// The name was empty or whitespace only.
    #[error("the display name is empty")]
    Empty,
    /// The trimmed name is longer than [`MAX_DISPLAY_NAME_CHARS`] characters.
    #[error("the display name is {chars} characters long; at most {max} are allowed")]
    TooLong {
        /// The trimmed length in characters.
        chars: usize,
        /// The enforced maximum in characters.
        max: usize,
    },
    /// The name contains a newline, a control character, a line or paragraph
    /// separator, or a bidirectional formatting character.
    #[error(
        "the display name must be a single line of plain text without control, \
         line-break, or text-direction characters"
    )]
    NotSingleLine,
}

impl DisplayName {
    /// Trims `raw` and validates it as a display name.
    ///
    /// # Errors
    ///
    /// [`DisplayNameError::Empty`] for an empty or whitespace-only string,
    /// [`DisplayNameError::TooLong`] when the trimmed text exceeds
    /// [`MAX_DISPLAY_NAME_CHARS`] characters, and
    /// [`DisplayNameError::NotSingleLine`] when it contains a character that
    /// would break or disguise the displayed line.
    pub fn new(raw: &str) -> Result<Self, DisplayNameError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(DisplayNameError::Empty);
        }
        if trimmed.chars().any(is_disallowed) {
            return Err(DisplayNameError::NotSingleLine);
        }
        let chars = trimmed.chars().count();
        if chars > MAX_DISPLAY_NAME_CHARS {
            return Err(DisplayNameError::TooLong {
                chars,
                max: MAX_DISPLAY_NAME_CHARS,
            });
        }
        Ok(Self(trimmed.to_owned()))
    }

    /// Builds a display name from untrusted local text, keeping what can be
    /// shown honestly: disallowed characters are removed and an over-long name
    /// is cut on a character boundary. Returns `None` when nothing printable is
    /// left. Used where the source is the local operating system (an account's
    /// full name) rather than the wire, which is always validated strictly.
    #[must_use]
    pub fn sanitized(raw: &str) -> Option<Self> {
        let cleaned: String = raw.chars().filter(|c| !is_disallowed(*c)).collect();
        let cut: String = cleaned
            .trim()
            .chars()
            .take(MAX_DISPLAY_NAME_CHARS)
            .collect();
        Self::new(&cut).ok()
    }

    /// The display name text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for DisplayName {
    type Error = DisplayNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<DisplayName> for String {
    fn from(value: DisplayName) -> Self {
        value.0
    }
}

impl AsRef<str> for DisplayName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DisplayName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
