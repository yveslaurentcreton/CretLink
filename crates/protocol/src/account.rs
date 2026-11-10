//! The customer process's effective account (ST-M009-008, protocol minor 11).
//!
//! The agent reports the account its process runs as, `DOMAIN\user` (or the
//! bare user name when the domain is unknown), as an optional field of
//! [`crate::TargetIdentity`], so the technician's live identity line can show
//! which account the work runs under (REQ-F-016, REQ-S-011). It is the
//! customer's own account name: it is shown to the technician, it is not a
//! credential, and it grants no authority.
//!
//! An [`AccountName`] can only be built through [`AccountName::new`], which
//! trims it and enforces the single-line, bounded shape, so a malformed name
//! is rejected when decoded from the wire like any other out-of-contract field.

use core::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::limits::MAX_ACCOUNT_NAME_CHARS;
use crate::purpose::is_disallowed;

/// A validated, trimmed, single-line account name: 1 to
/// [`MAX_ACCOUNT_NAME_CHARS`] characters, with no control characters, line or
/// paragraph separators, or bidirectional formatting characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AccountName(String);

/// Why an account name was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AccountNameError {
    /// The name was empty or whitespace only.
    #[error("the account name is empty")]
    Empty,
    /// The trimmed name is longer than [`MAX_ACCOUNT_NAME_CHARS`] characters.
    #[error("the account name is {chars} characters long; at most {max} are allowed")]
    TooLong {
        /// The trimmed length in characters.
        chars: usize,
        /// The enforced maximum in characters.
        max: usize,
    },
    /// The name contains a newline, a control character, a line or paragraph
    /// separator, or a bidirectional formatting character.
    #[error(
        "the account name must be a single line of plain text without control, \
         line-break, or text-direction characters"
    )]
    NotSingleLine,
}

impl AccountName {
    /// Trims `raw` and validates it as an account name.
    ///
    /// # Errors
    ///
    /// [`AccountNameError::Empty`] for an empty or whitespace-only string,
    /// [`AccountNameError::TooLong`] when the trimmed text exceeds
    /// [`MAX_ACCOUNT_NAME_CHARS`] characters, and
    /// [`AccountNameError::NotSingleLine`] when it contains a character that
    /// would break or disguise the displayed line.
    pub fn new(raw: &str) -> Result<Self, AccountNameError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(AccountNameError::Empty);
        }
        if trimmed.chars().any(is_disallowed) {
            return Err(AccountNameError::NotSingleLine);
        }
        let chars = trimmed.chars().count();
        if chars > MAX_ACCOUNT_NAME_CHARS {
            return Err(AccountNameError::TooLong {
                chars,
                max: MAX_ACCOUNT_NAME_CHARS,
            });
        }
        Ok(Self(trimmed.to_owned()))
    }

    /// The account as the agent reports it: `DOMAIN\user`, or `user` alone
    /// when the domain is unknown. `None` when the user name is empty or
    /// unknown, or the result is not a valid account name: the field is then
    /// omitted rather than guessed (REQ-S-011).
    #[must_use]
    pub fn qualified(domain: Option<&str>, user: &str) -> Option<Self> {
        let user = user.trim();
        if user.is_empty() {
            return None;
        }
        let raw = match domain.map(str::trim).filter(|domain| !domain.is_empty()) {
            Some(domain) => format!("{domain}\\{user}"),
            None => user.to_owned(),
        };
        Self::new(&raw).ok()
    }

    /// The account name text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for AccountName {
    type Error = AccountNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<AccountName> for String {
    fn from(value: AccountName) -> Self {
        value.0
    }
}

impl AsRef<str> for AccountName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AccountName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
