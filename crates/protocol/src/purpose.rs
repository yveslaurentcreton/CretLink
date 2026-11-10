//! The caller's stated purpose of one AI action (REQ-F-026, ST-M009-007).
//!
//! Every MCP tool call that acts on the customer computer carries a short,
//! plain-language statement of *why* the AI client is doing it, so the attended
//! customer and the technician read a reason next to the real operation. The
//! purpose is a **claim by the caller**: it is never verified, never replaces or
//! hides the real command, path, or action, and grants no authority. It is
//! attribution text only, recorded and shown as such.
//!
//! A [`Purpose`] can only be built through [`Purpose::new`], which trims it and
//! enforces the single-line, bounded shape, so an invalid purpose cannot be
//! constructed in Rust and is rejected when decoded from the wire (serde
//! deserializes through the same constructor).

use core::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::limits::MAX_PURPOSE_CHARS;

/// A validated, trimmed, single-line statement of why an AI action is taken
/// (REQ-F-026). Between 1 and [`MAX_PURPOSE_CHARS`] characters, with no control
/// characters, line or paragraph separators, or bidirectional formatting
/// characters, so it renders as one honest line on both surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Purpose(String);

/// Why a purpose was rejected. Every message is actionable for an AI client:
/// it says what to send instead.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PurposeError {
    /// The purpose was empty or whitespace only.
    #[error(
        "purpose is empty: state in one short sentence why you are doing this, \
         for example \"Checking free disk space\""
    )]
    Empty,
    /// The trimmed purpose is longer than [`MAX_PURPOSE_CHARS`] characters.
    #[error(
        "purpose is {chars} characters long; keep it to at most {max} characters \
         (one short sentence the customer will read)"
    )]
    TooLong {
        /// The trimmed length in characters.
        chars: usize,
        /// The enforced maximum in characters.
        max: usize,
    },
    /// The purpose contains a newline, a control character, a line or
    /// paragraph separator, or a bidirectional formatting character.
    #[error(
        "purpose must be a single line of plain text without control, line-break, \
         or text-direction characters"
    )]
    NotSingleLine,
}

impl Purpose {
    /// Trims `raw` and validates it as a purpose.
    ///
    /// # Errors
    ///
    /// [`PurposeError::Empty`] for an empty or whitespace-only string,
    /// [`PurposeError::TooLong`] when the trimmed text exceeds
    /// [`MAX_PURPOSE_CHARS`] characters, and [`PurposeError::NotSingleLine`]
    /// when it contains a character that would break or disguise the single
    /// displayed line.
    pub fn new(raw: &str) -> Result<Self, PurposeError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(PurposeError::Empty);
        }
        if trimmed.chars().any(is_disallowed) {
            return Err(PurposeError::NotSingleLine);
        }
        let chars = trimmed.chars().count();
        if chars > MAX_PURPOSE_CHARS {
            return Err(PurposeError::TooLong {
                chars,
                max: MAX_PURPOSE_CHARS,
            });
        }
        Ok(Self(trimmed.to_owned()))
    }

    /// The purpose text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Characters a purpose may not contain: every Unicode control character
/// (C0, DEL, C1, which includes CR and LF), the line and paragraph separators,
/// and the bidirectional formatting characters that could visually reorder the
/// line a customer reads (a spoofing vector on the attended surface).
pub(crate) fn is_disallowed(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{2028}' // LINE SEPARATOR
                | '\u{2029}' // PARAGRAPH SEPARATOR
                | '\u{061C}' // ARABIC LETTER MARK
                | '\u{200E}' // LEFT-TO-RIGHT MARK
                | '\u{200F}' // RIGHT-TO-LEFT MARK
                | '\u{202A}'..='\u{202E}' // embeddings and overrides
                | '\u{2066}'..='\u{2069}' // isolates
        )
}

impl TryFrom<String> for Purpose {
    type Error = PurposeError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<Purpose> for String {
    fn from(value: Purpose) -> Self {
        value.0
    }
}

impl AsRef<str> for Purpose {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Purpose {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
