//! A page of a folder listing that always fits the channel (ST-M010-005).
//!
//! The agent answers [`Operation::FsList`](crate::Operation::FsList) with one
//! page of the folder ([`OpResult::DirEntries`]). The secure channel carries a
//! message only when its encoding fits one record
//! ([`limits::MAX_RECORD_BYTES`]); nothing splits a longer one. A page that is
//! bounded by its number of entries alone does not fit when the names are
//! long: [`limits::MAX_DIR_PAGE`] names of 250 characters of three bytes each
//! are about 200 KiB.
//!
//! [`DirPage`] is a page that is filled entry by entry and takes an entry only
//! while the answer that carries the page still fits one record. The page then
//! ends before that entry, and the answer says that more follows. The listing
//! continues after a name, so a page may be as short as it has to be: nothing
//! changes on the wire.
//!
//! # Measured, not estimated
//!
//! The size of the answer is the size of its real encoding. An entry is
//! measured by encoding it, so a name that the encoding writes longer — a
//! quotation mark or a backslash as two bytes, a control character as six — is
//! counted as it is written. What surrounds the entries is measured by
//! encoding the answer of an empty page with the longest request identifier
//! there is and with `end` at the longer of its two values. A JSON array of
//! one or more values is its values with one comma between each two, so the
//! entries add their own lengths and those commas, and nothing else. The
//! answer that is really sent is therefore never longer than the size counted
//! here, whatever its request identifier and whether or not it is the last
//! page.
//!
//! # A single entry always fits
//!
//! A name has at most [`limits::MAX_NAME_BYTES`] bytes, each written as at
//! most six, so the largest entry is a small part of a record and an empty
//! page takes every entry the protocol can carry. A page that says more
//! follows therefore holds at least one entry, and a listing always gets
//! further.

use serde::Serialize;
use uuid::Uuid;

use crate::ids::{RequestId, SessionId};
use crate::limits;
use crate::message::{FileEntry, Message, OpResult};

/// One page of a folder listing, filled entry by entry: at most
/// [`limits::MAX_DIR_PAGE`] entries, and never more than fit, with the answer
/// that carries them, in one record of the secure channel
/// ([`limits::MAX_RECORD_BYTES`]). See the module documentation.
#[derive(Debug, Clone)]
pub struct DirPage {
    /// The entries of the page, in the order they were added.
    entries: Vec<FileEntry>,
    /// The size of the encoded answer with these entries, at its largest.
    encoded: usize,
}

impl DirPage {
    /// An empty page.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            encoded: empty_answer_len(),
        }
    }

    /// Adds `entry` to the page when the page has room for it: it holds fewer
    /// than [`limits::MAX_DIR_PAGE`] entries, and the answer still fits one
    /// record with this entry in it.
    ///
    /// # Errors
    ///
    /// The entry itself, not added, when the page ends before it.
    pub fn push(&mut self, entry: FileEntry) -> Result<(), FileEntry> {
        if self.entries.len() >= limits::MAX_DIR_PAGE {
            return Err(entry);
        }
        // One comma between this entry and the one before it.
        let separator = usize::from(!self.entries.is_empty());
        let with_entry = self
            .encoded
            .saturating_add(separator)
            .saturating_add(encoded_len(&entry));
        if with_entry > limits::MAX_RECORD_BYTES {
            return Err(entry);
        }
        self.encoded = with_entry;
        self.entries.push(entry);
        Ok(())
    }

    /// Whether the page holds no entry.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many entries the page holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The size in bytes of the encoded answer that carries this page, at its
    /// largest: with the longest request identifier, and saying that more
    /// follows. Never more than [`limits::MAX_RECORD_BYTES`].
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        self.encoded
    }

    /// The result that carries this page. `end` says whether it is the last
    /// page of the listing.
    #[must_use]
    pub fn into_result(self, end: bool) -> OpResult {
        OpResult::DirEntries {
            entries: self.entries,
            end,
        }
    }
}

impl Default for DirPage {
    fn default() -> Self {
        Self::new()
    }
}

/// The size of the encoded answer of a page without entries, at its largest:
/// see the module documentation.
fn empty_answer_len() -> usize {
    encoded_len(&Message::Response {
        // No identifier is written longer than the largest one.
        id: RequestId(u64::MAX),
        // Every session identifier is written with the same number of
        // characters.
        session: SessionId(Uuid::nil()),
        result: OpResult::DirEntries {
            entries: Vec::new(),
            // `false` is the longer of the two.
            end: false,
        },
    })
}

/// The number of bytes `value` takes in a message, counted by encoding it the
/// way [`Message::encode`] does. A value that cannot be encoded fits nowhere.
fn encoded_len<T: Serialize>(value: &T) -> usize {
    let mut counted = ByteCount(0);
    match serde_json::to_writer(&mut counted, value) {
        Ok(()) => counted.0,
        Err(_) => usize::MAX,
    }
}

/// Counts the bytes written to it and keeps none.
struct ByteCount(usize);

impl std::io::Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
