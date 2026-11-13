//! The file explorer of the live session (ST-M010-002, REQ-F-028).
//!
//! Two commands let the Files tab look through the customer computer's files:
//!
//! - [`explorer_drives`] lists the drives;
//! - [`explorer_open`] opens the place a path names and lists it. The folder
//!   arrives on the [`EXPLORER_EVENT`] event while the command runs: first
//!   where the folder really is with its first page, then page after page
//!   until the end ([`ExplorerEvent`]). The command answers when the listing
//!   has ended, with how it ended ([`ListingView`]). A folder of thousands of
//!   entries is therefore on screen with its first page and fills while the
//!   technician already works in it.
//!
//! Both are thin: [`cretlink_console::TechnicianFiles`] does the work on the
//! consented session, attributes it to the technician and records it in the
//! audit trail, from where it reaches the activity feed like every other
//! action.
//!
//! # No authority of its own (REQ-S-023)
//!
//! Neither command works without a session under full control: outside one
//! the service slot is empty and the command refuses with
//! `no_consented_session` before anything is asked. A stop or a revocation
//! ends a listing with the session, between two of its requests, as an error
//! — never as the whole folder (REQ-N-008).
//!
//! # A path from the screen
//!
//! `explorer_open` takes a path as text from the window. It is a path on the
//! customer computer, never one on this computer, and nothing on this
//! computer is opened with it. It must be a rooted path on a drive
//! ([`cretlink_protocol::validate_remote_absolute_path`]); any other text is
//! refused, and recorded, before anything is asked of the customer computer.
//! What is then shown and sent is the place as the customer computer names
//! it, not the text that was typed (see `cretlink_console::explorer`).
//!
//! # One listing at a time
//!
//! The window looks at one folder. A new `explorer_open`, or the drive list,
//! leaves the folder that is still being listed: its listing stops between
//! two requests and answers `cancelled` ([`Listings`]). Every event carries
//! the number the window gave its listing, so a page of a folder that was
//! left is never taken for a page of the one that followed.

use std::sync::{Arc, Mutex, PoisonError};

use cretlink_console::{FolderEvent, FolderReport, ListingEnd, TechnicianFiles};
use cretlink_mcp::TargetError;
use cretlink_protocol::{DriveEntry, DriveKind, ErrorCode, ErrorStage, FileEntry, FileKind};
use cretlink_session::{CancelToken, RemotePath, activity::error_text};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use super::{AppState, CommandError, no_session};

/// The Tauri event a listing is delivered on: one [`ExplorerEvent`] each.
pub(crate) const EXPLORER_EVENT: &str = "console-explorer";

/// One drive of the customer computer, as the explorer shows it
/// (`ExplorerDrive` in the frontend contract). A value the customer computer
/// could not read is absent, never zero (REQ-N-008).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DriveView {
    /// The drive's root folder, such as `C:\`.
    root: String,
    /// What the drive is.
    kind: DriveKind,
    /// The volume label, when it has one that can be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    /// The size of the drive in bytes, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    total_bytes: Option<u64>,
    /// The bytes free on it, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    free_bytes: Option<u64>,
}

impl From<DriveEntry> for DriveView {
    fn from(drive: DriveEntry) -> Self {
        Self {
            root: drive.root,
            kind: drive.kind,
            label: drive.label,
            total_bytes: drive.total_bytes,
            free_bytes: drive.free_bytes,
        }
    }
}

/// One entry of a folder (`ExplorerEntry` in the frontend contract). Every
/// text is the customer computer's own and is written as text by the window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EntryView {
    /// The name of the entry in its folder.
    name: String,
    /// A file, a folder, a link, or something else. A link is described and
    /// never followed.
    kind: FileKind,
    /// Its size in bytes.
    len: u64,
    /// When it was last modified, in Unix milliseconds, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    modified_ms: Option<u64>,
    /// Whether it is read-only.
    readonly: bool,
}

impl From<FileEntry> for EntryView {
    fn from(entry: FileEntry) -> Self {
        Self {
            name: entry.name,
            kind: entry.kind,
            len: entry.len,
            modified_ms: entry.modified_ms,
            readonly: entry.readonly,
        }
    }
}

fn entries(page: Vec<FileEntry>) -> Vec<EntryView> {
    page.into_iter().map(EntryView::from).collect()
}

/// One step of a listing, as the window receives it on [`EXPLORER_EVENT`]
/// (`ExplorerEvent` in the frontend contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub(crate) enum ExplorerEvent {
    /// The folder is open: where it really is, and its first page. Sent once,
    /// first, and for an empty folder too.
    Opened {
        /// The number the window gave this listing.
        listing: u64,
        /// The folder as the customer computer names it.
        path: String,
        /// The file that the asked path named, in that folder.
        #[serde(skip_serializing_if = "Option::is_none")]
        select: Option<String>,
        /// The first page, ordered by name.
        entries: Vec<EntryView>,
        /// Whether this page is the whole folder.
        end: bool,
    },
    /// A further page of the folder.
    Page {
        /// The number the window gave this listing.
        listing: u64,
        /// The entries after those delivered before.
        entries: Vec<EntryView>,
        /// Whether the folder ends with this page.
        end: bool,
    },
}

impl ExplorerEvent {
    fn of(listing: u64, event: FolderEvent) -> Self {
        match event {
            FolderEvent::Opened {
                folder,
                select,
                entries: page,
                end,
            } => Self::Opened {
                listing,
                path: shown(&folder),
                select,
                entries: entries(page),
                end,
            },
            FolderEvent::Page { entries: page, end } => Self::Page {
                listing,
                entries: entries(page),
                end,
            },
        }
    }
}

/// How a listing ended (`ExplorerListing` in the frontend contract). Only
/// `complete` is the whole folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ListingView {
    /// The number the window gave this listing.
    listing: u64,
    /// The folder that was listed, as the customer computer names it. Absent
    /// when the listing was left before the folder was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    /// How many entries were delivered.
    entries: u64,
    /// `complete`, `cancelled` (left for another place) or `limit` (the
    /// folder holds more entries than the explorer lists).
    state: &'static str,
}

impl ListingView {
    fn of(listing: u64, report: &FolderReport) -> Self {
        Self {
            listing,
            path: report.folder.as_ref().map(shown),
            entries: report.entries,
            state: match report.end {
                ListingEnd::Complete => "complete",
                ListingEnd::Cancelled => "cancelled",
                ListingEnd::Limit => "limit",
            },
        }
    }
}

/// A folder as the window names it. The explorer opens folders by their
/// path; a staged folder, which it does not open, would be named as the
/// activity names it.
fn shown(folder: &RemotePath) -> String {
    match folder {
        RemotePath::Absolute { path } => path.clone(),
        RemotePath::Staging { name } => format!("{name} (CretLink staging folder)"),
    }
}

/// The listing the window looks at, so that the next one can leave it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Listings(Arc<Mutex<Option<(u64, CancelToken)>>>);

impl Listings {
    /// Starts the listing `listing`: the one before it is left, and the
    /// token that leaves this one is returned.
    fn begin(&self, listing: u64) -> CancelToken {
        let token = CancelToken::new();
        let previous = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace((listing, token.clone()));
        if let Some((_, previous)) = previous {
            previous.cancel();
        }
        token
    }

    /// Leaves the listing that runs, if any, for a place that is no folder.
    fn leave(&self) {
        let running = self.0.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some((_, running)) = running {
            running.cancel();
        }
    }

    /// The listing `listing` has ended. A listing that started after it is
    /// left as it is.
    fn end(&self, listing: u64) {
        let mut current = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(&*current, Some((running, _)) if *running == listing) {
            *current = None;
        }
    }
}

/// The name of an error category as the wire and the window spell it:
/// `PathNotFound` is `path_not_found`.
fn code_token(code: ErrorCode) -> String {
    let name = format!("{code:?}");
    let mut token = String::with_capacity(name.len() + 4);
    for (at, letter) in name.chars().enumerate() {
        if letter.is_ascii_uppercase() && at > 0 {
            token.push('_');
        }
        token.push(letter.to_ascii_lowercase());
    }
    token
}

/// A failure of an explorer command, for the window: the stage is the error
/// category, so the window can tell the end of the session from a refusal,
/// and the message says it in the words the application uses for a failed
/// action elsewhere (REQ-N-015).
///
/// A refusal the explorer made itself — a text that is no remote path, a
/// name the folder above does not list, a link on the way — keeps its own
/// message, which names what stood in the way. A refusal of the customer
/// computer is put in the words the activity feed has for its category
/// ([`error_text`]).
fn explorer_error(error: TargetError) -> CommandError {
    match error {
        TargetError::Remote {
            code,
            message,
            stage: ErrorStage::Validation,
        } => CommandError::new(&code_token(code), message),
        TargetError::Remote { code, .. } => CommandError::new(&code_token(code), error_text(code)),
        TargetError::NoConsentedSession => no_session(),
        TargetError::Disconnected => CommandError::new(
            "disconnected",
            "the connection to the customer computer was lost before it answered",
        ),
        TargetError::Unexpected => CommandError::new(
            "unexpected",
            "the customer computer gave an answer that CretLink did not expect",
        ),
    }
}

/// Lists the drives of the customer computer. See [`explorer_drives`].
pub(crate) async fn drives(state: &AppState) -> Result<Vec<DriveView>, CommandError> {
    let (service, audit) = state.files().ok_or_else(no_session)?;
    // The drives are another place than the folder that may still be listed.
    state.listings.leave();
    TechnicianFiles::new(&service, &audit)
        .list_drives()
        .await
        .map(|drives| drives.into_iter().map(DriveView::from).collect())
        .map_err(explorer_error)
}

/// Opens the place `path` names and lists it, delivering each step to `emit`.
/// See [`explorer_open`].
pub(crate) async fn open(
    state: &AppState,
    path: &str,
    listing: u64,
    emit: &mut (dyn FnMut(ExplorerEvent) + Send),
) -> Result<ListingView, CommandError> {
    let (service, audit) = state.files().ok_or_else(no_session)?;
    let cancel = state.listings.begin(listing);
    let result = {
        let mut sink = |event: FolderEvent| emit(ExplorerEvent::of(listing, event));
        TechnicianFiles::new(&service, &audit)
            .open_folder(path, cancel, &mut sink)
            .await
    };
    state.listings.end(listing);
    result
        .map(|report| ListingView::of(listing, &report))
        .map_err(explorer_error)
}

/// The drives of the customer computer, each with its root, kind, label and
/// total and free space (ST-M010-002). Leaves a folder that is still being
/// listed. Fails closed with `no_consented_session` outside a session under
/// full control.
#[tauri::command]
pub(super) async fn explorer_drives(
    state: State<'_, AppState>,
) -> Result<Vec<DriveView>, CommandError> {
    drives(&state).await
}

/// Opens the place the path `path` names on the customer computer and lists
/// it (ST-M010-002).
///
/// `listing` is a number the window chooses for this listing; every
/// [`ExplorerEvent`] of it carries the number. The folder is delivered on
/// [`EXPLORER_EVENT`] and the command answers once the listing has ended.
/// Nothing is delivered for a place that is refused: the answer is then the
/// error, and the window stays where it was. A listing that is already
/// running is left for this one.
///
/// Fails closed with `no_consented_session` outside a session under full
/// control.
#[tauri::command]
pub(super) async fn explorer_open(
    app: AppHandle,
    state: State<'_, AppState>,
    path: String,
    listing: u64,
) -> Result<ListingView, CommandError> {
    let mut emit = |event: ExplorerEvent| {
        // A window that is gone is not an error of the listing.
        let _ = app.emit(EXPLORER_EVENT, &event);
    };
    open(&state, &path, listing, &mut emit).await
}

#[cfg(test)]
mod tests;
