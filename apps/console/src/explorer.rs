//! Opening a folder for the technician's file explorer (ST-M010-002,
//! REQ-F-028, REQ-N-006, REQ-N-008).
//!
//! The explorer shows where it stands, and every later action names that
//! place. This module finds the place and lists it; [`crate::TechnicianFiles`]
//! is its audited front.
//!
//! # The real location
//!
//! The place the explorer names must be the one the customer computer names
//! itself, not a spelling somebody typed. The target's answers hold two things
//! to name a place by: the roots of its drives, and in a page of a folder the
//! names of its entries. No answer holds a path. `locate` therefore builds a
//! location from those two alone:
//!
//! - the root is the root of a drive as the drive list gives it;
//! - every further name is the name of an entry as a listing of the folder
//!   above it gives it, and that entry is a folder.
//!
//! A path that was typed is taken apart and looked for name by name. A name is
//! found when the folder above lists it — spelled as it was typed, or in
//! another letter case, in which case the listed spelling is the one that
//! counts. Three things end the search, and nothing is opened then. Each is a
//! refusal in the stage `validation` with a message in plain words, like the
//! refusal of a text that is no remote path at all; a refusal of the target
//! itself keeps the stage `execution`:
//!
//! - the folder above lists no entry of that name ([`ErrorCode::PathNotFound`]).
//!   That covers a name that does not exist and a second spelling the listing
//!   does not show, such as a short name (`PROGRA~1`);
//! - the entry is a link ([`ErrorCode::InvalidPath`]). A listing describes a
//!   link and never follows it, and a place that lies behind one has another
//!   real location than its path says;
//! - the entry is a file with further names behind it
//!   ([`ErrorCode::WrongPathKind`]).
//!
//! A path whose last name is a file leads to the folder that holds it, with
//! the file named so a screen can select it.
//!
//! # What is remembered (`Browsed`)
//!
//! Looking a name up costs a request, so the session remembers what the target
//! has confirmed: the drive roots, the folders that were found or listed, and
//! the subfolders of the folder listed last. A step into a subfolder, a step
//! up and a step back therefore cost one request, the listing itself. All
//! three are bounded, and what is forgotten is simply looked up again. The
//! memory is a help, not a check: a folder that changed since is listed by its
//! name as before, and the target decides what it answers.
//!
//! # Finding one name without reading the whole folder
//!
//! A listing is ordered by name and continues after a name
//! ([`cretlink_protocol::Operation::FsList`]). A name spelled as the folder
//! lists it is therefore found in the first page asked for from just before
//! it, however large the folder is. Only a name in another letter case needs
//! the folder read from its start, until the name is met.
//!
//! # A listing in pages, with an honest end
//!
//! `list` asks for one page after another, continuing after the last name
//! of each, until the target says it is the end. A page is as long as it is
//! (ST-M010-005). The caller gets the pages as they arrive
//! ([`FolderEvent`]), the first one with the folder's real location, so a
//! screen can show the folder at once and fill it while the rest arrives.
//!
//! A listing ends in one of three ways ([`ListingEnd`]), and only one of them
//! is the whole folder: it is complete; the caller cancelled it; or it reached
//! [`MAX_FOLDER_ENTRIES`], the most entries of one folder the explorer holds,
//! and says that more exist. A request the target refuses, or a session that
//! is lost, is an error of the call, with what had been listed until then
//! already delivered.

use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;

use async_trait::async_trait;
use cretlink_mcp::{DirPage, TargetError};
use cretlink_protocol::{
    DriveEntry, ErrorCode, ErrorStage, FileEntry, FileKind, ProtocolError,
    validate_remote_absolute_path,
};
use cretlink_session::{CancelToken, RemotePath};

/// The most entries of one folder the explorer lists (REQ-N-006). A listing
/// stops asking once it holds this many and says that more exist; a page that
/// is already on its way is still delivered, so a listing can hold up to one
/// page more.
pub const MAX_FOLDER_ENTRIES: u64 = 100_000;

/// The most folders a session remembers as confirmed by the target.
const MAX_KNOWN_FOLDERS: usize = 256;

/// The most subfolder names remembered of the folder listed last.
const MAX_LISTED_SUBFOLDERS: usize = 20_000;

/// How often in a row a page may come back empty and say that more follows
/// before the listing gives up. Such a page appears when every entry of it
/// vanished between the listing and the look at it; asking again gets
/// further, and a folder that keeps answering so is not listed.
const MAX_EMPTY_PAGES: u32 = 3;

/// What a listing delivers while it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderEvent {
    /// The folder is open: where it really is, and its first page. Always the
    /// first event, and sent for an empty folder too.
    Opened {
        /// The folder as the target names it.
        folder: RemotePath,
        /// The file that the asked path named, in that folder.
        select: Option<String>,
        /// The first page, ordered by name.
        entries: Vec<FileEntry>,
        /// Whether this page is the whole folder.
        end: bool,
    },
    /// A further page of the folder.
    Page {
        /// The entries, ordered by name, after those delivered before.
        entries: Vec<FileEntry>,
        /// Whether the folder ends with this page.
        end: bool,
    },
}

/// How a listing ended. Only [`ListingEnd::Complete`] is the whole folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingEnd {
    /// Every entry of the folder was delivered.
    Complete,
    /// The caller cancelled before the end: the folder holds more than was
    /// delivered, or the search for it was given up.
    Cancelled,
    /// The listing reached [`MAX_FOLDER_ENTRIES`]: more entries exist and
    /// were not asked for.
    Limit,
}

/// The honest report of one listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderReport {
    /// The folder that was listed, as the target names it. `None` when the
    /// listing was cancelled before the folder was found.
    pub folder: Option<RemotePath>,
    /// The file that the asked path named, in that folder.
    pub select: Option<String>,
    /// How many entries were delivered.
    pub entries: u64,
    /// How the listing ended.
    pub end: ListingEnd,
}

/// The two reads the explorer makes on the target.
#[async_trait]
pub(crate) trait FolderSource: Send + Sync {
    /// The drives of the target.
    async fn drives(&self) -> Result<Vec<DriveEntry>, TargetError>;

    /// One page of `folder`, continued after the name `after`.
    async fn page(&self, folder: &RemotePath, after: Option<&str>) -> Result<DirPage, TargetError>;
}

/// What a session remembers of the places the target has confirmed. See the
/// module documentation.
#[derive(Debug, Default)]
pub(crate) struct Browsed {
    /// The drive roots of the drive list read last.
    roots: Vec<String>,
    /// Folders the target has confirmed, the most recently used last.
    folders: VecDeque<String>,
    /// The folder listed last and the names of its subfolders.
    listed: Option<(String, HashSet<String>)>,
}

impl Browsed {
    /// Takes the roots of a drive list the target just answered.
    pub(crate) fn set_roots(&mut self, drives: &[DriveEntry]) {
        self.roots = drives.iter().map(|drive| drive.root.clone()).collect();
    }

    /// The root the drive list gives for the drive of `wanted` (`C:\`),
    /// whatever the letter case of `wanted`.
    fn root(&self, wanted: &str) -> Option<String> {
        self.roots
            .iter()
            .find(|root| root.eq_ignore_ascii_case(wanted))
            .cloned()
    }

    /// Whether the target confirmed `folder` under exactly this spelling.
    fn knows(&mut self, folder: &str) -> bool {
        let Some(at) = self.folders.iter().position(|known| known == folder) else {
            return false;
        };
        // Used again: it is forgotten last.
        if let Some(known) = self.folders.remove(at) {
            self.folders.push_back(known);
        }
        true
    }

    /// Remembers that the target confirmed `folder`.
    fn remember(&mut self, folder: &str) {
        if self.knows(folder) {
            return;
        }
        if self.folders.len() >= MAX_KNOWN_FOLDERS {
            self.folders.pop_front();
        }
        self.folders.push_back(folder.to_owned());
    }

    /// Whether the listing of `folder` that ran last named the subfolder
    /// `name`, under exactly this spelling.
    fn lists(&self, folder: &str, name: &str) -> bool {
        self.listed
            .as_ref()
            .is_some_and(|(listed, subfolders)| listed == folder && subfolders.contains(name))
    }

    /// Starts over for a listing of `folder`.
    fn listing(&mut self, folder: &str) {
        self.listed = Some((folder.to_owned(), HashSet::new()));
    }

    /// Takes the subfolders of one more page of the listing of `folder`.
    fn listed(&mut self, folder: &str, entries: &[FileEntry]) {
        let Some((listed, subfolders)) = &mut self.listed else {
            return;
        };
        if listed != folder {
            return;
        }
        for entry in entries {
            if subfolders.len() >= MAX_LISTED_SUBFOLDERS {
                return;
            }
            if entry.kind == FileKind::Dir {
                subfolders.insert(entry.name.clone());
            }
        }
    }
}

/// Locks the memory, recovering it if an earlier holder panicked.
fn lock(memory: &Mutex<Browsed>) -> std::sync::MutexGuard<'_, Browsed> {
    memory
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A place the explorer was led to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Located {
    /// The folder, as the target names it.
    pub(crate) folder: String,
    /// The file that the asked path named, in that folder.
    pub(crate) select: Option<String>,
}

/// A refusal the explorer makes itself, from a spelling or from what the
/// target listed: the stage is `validation`, since the folder that was asked
/// for is never asked of the target, and the message says in plain words what
/// stood in the way. A refusal of the target keeps the target's own stage.
fn refused(code: ErrorCode, message: String) -> TargetError {
    TargetError::Remote {
        code,
        message,
        stage: ErrorStage::Validation,
    }
}

/// A path as a screen spells it, taken apart: the root of its drive
/// (`C:\`, the letter as it was typed) and the names below it.
///
/// Surrounding spaces and one pair of quotation marks are dropped, as a path
/// that was copied carries them; a forward slash is read as a backslash, and
/// a drive alone (`C:`) as its root.
///
/// # Errors
///
/// An `invalid_path` or `limit_exceeded` [`TargetError`] in the stage
/// `validation` when the text breaks the rules for a remote path: nothing was
/// asked of the target.
pub(crate) fn spell(asked: &str) -> Result<(String, Vec<String>), TargetError> {
    let trimmed = asked.trim();
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(trimmed)
        .trim();
    let mut text: String = unquoted
        .chars()
        .map(|c| if c == '/' { '\\' } else { c })
        .collect();
    // A drive alone means its root here: the explorer has no folder that such
    // a path could be relative to.
    let mut letters = text.chars();
    if let (Some(letter), Some(':'), None) = (letters.next(), letters.next(), letters.next())
        && letter.is_ascii_alphabetic()
    {
        text.push('\\');
    }
    validate_remote_absolute_path("path", &text).map_err(|error| {
        let message = match &error {
            ProtocolError::InvalidPath { reason, .. } => {
                format!("this is not a path on a drive of the customer computer: {reason}")
            }
            ProtocolError::LimitExceeded { .. } => {
                "the path is longer or deeper than a path may be".to_owned()
            }
            other => other.to_string(),
        };
        refused(error.code(), message)
    })?;
    let mut parts = text.split('\\').filter(|part| !part.is_empty());
    // The validation above saw the drive.
    let drive = parts.next().unwrap_or_default();
    Ok((format!("{drive}\\"), parts.map(str::to_owned).collect()))
}

/// The path of the entry `name` in `folder`.
pub(crate) fn join(folder: &str, name: &str) -> String {
    if folder.ends_with('\\') {
        format!("{folder}{name}")
    } else {
        format!("{folder}\\{name}")
    }
}

/// Finds the place the path `asked` names, in the target's own names. See the
/// module documentation. `Ok(None)` when `cancel` was set on the way.
///
/// # Errors
///
/// A [`TargetError`]: the refusal of a spelling, of a name the folder above
/// does not list, of a link, or of a file with names behind it; or whatever
/// the target answered a request with.
pub(crate) async fn locate<S: FolderSource + ?Sized>(
    source: &S,
    memory: &Mutex<Browsed>,
    asked: &str,
    cancel: &CancelToken,
) -> Result<Option<Located>, TargetError> {
    let (wanted_root, names) = spell(asked)?;

    // The whole path as it was spelled may be a folder the target confirmed.
    let spelled = names
        .iter()
        .fold(wanted_root.clone(), |path, name| join(&path, name));
    if lock(memory).knows(&spelled) {
        return Ok(Some(Located {
            folder: spelled,
            select: None,
        }));
    }

    let known_root = lock(memory).root(&wanted_root);
    let root = match known_root {
        Some(root) => root,
        None => {
            if cancel.is_cancelled() {
                return Ok(None);
            }
            // The drive may have appeared since the list was read.
            let drives = source.drives().await?;
            let mut memory = lock(memory);
            memory.set_roots(&drives);
            memory.root(&wanted_root).ok_or_else(|| {
                refused(
                    ErrorCode::PathNotFound,
                    format!(
                        "the customer computer lists no drive {}",
                        wanted_root.trim_end_matches('\\').to_uppercase()
                    ),
                )
            })?
        }
    };

    let mut folder = root;
    let last = names.len().saturating_sub(1);
    for (at, name) in names.iter().enumerate() {
        let below = join(&folder, name);
        let confirmed = {
            let mut memory = lock(memory);
            memory.knows(&below) || memory.lists(&folder, name)
        };
        if confirmed {
            lock(memory).remember(&below);
            folder = below;
            continue;
        }
        let entry = match find(source, &folder, name, cancel).await? {
            Looked::Found(entry) => entry,
            Looked::Cancelled => return Ok(None),
            Looked::Missing => {
                return Err(refused(
                    ErrorCode::PathNotFound,
                    format!("{folder} lists no entry named {name}"),
                ));
            }
        };
        let real = join(&folder, &entry.name);
        match entry.kind {
            FileKind::Dir => {
                lock(memory).remember(&real);
                folder = real;
            }
            FileKind::Symlink => {
                return Err(refused(
                    ErrorCode::InvalidPath,
                    format!("{real} is a link, and a link is shown, not followed"),
                ));
            }
            FileKind::File | FileKind::Other if at == last => {
                return Ok(Some(Located {
                    folder,
                    select: Some(entry.name),
                }));
            }
            FileKind::File | FileKind::Other => {
                return Err(refused(
                    ErrorCode::WrongPathKind,
                    format!("{real} is not a folder"),
                ));
            }
        }
    }
    Ok(Some(Located {
        folder,
        select: None,
    }))
}

/// What the search for one name in a folder came to.
enum Looked {
    /// The folder lists this entry under the name, or under another letter
    /// case of it.
    Found(FileEntry),
    /// The folder lists no such entry.
    Missing,
    /// The search was cancelled.
    Cancelled,
}

/// Looks for the entry `name` in `folder`: first spelled as given, then in
/// any letter case. See "Finding one name" in the module documentation.
async fn find<S: FolderSource + ?Sized>(
    source: &S,
    folder: &str,
    name: &str,
    cancel: &CancelToken,
) -> Result<Looked, TargetError> {
    let path = RemotePath::Absolute {
        path: folder.to_owned(),
    };

    // Spelled as given: the listing is ordered, so the name is in the page
    // that starts just before it, or the folder does not list it. What is
    // just before a name is the name without its last character.
    let mut before: String = name.to_owned();
    before.pop();
    let mut after = (!before.is_empty()).then_some(before);
    let mut read = 0u64;
    'spelled: loop {
        if cancel.is_cancelled() {
            return Ok(Looked::Cancelled);
        }
        let page = source.page(&path, after.as_deref()).await?;
        read = read.saturating_add(page.entries.len() as u64);
        for entry in &page.entries {
            if entry.name == name {
                return Ok(Looked::Found(entry.clone()));
            }
            if entry.name.as_str() > name {
                break 'spelled;
            }
        }
        let Some(last) = page.entries.last() else {
            break;
        };
        if page.end || read >= MAX_FOLDER_ENTRIES {
            break;
        }
        after = Some(last.name.clone());
    }

    // In another letter case: the folder from its start. A name without a
    // letter has no other case.
    if !name.chars().any(char::is_alphabetic) {
        return Ok(Looked::Missing);
    }
    let wanted = name.to_lowercase();
    let mut after: Option<String> = None;
    let mut read = 0u64;
    let mut empty = 0u32;
    loop {
        if cancel.is_cancelled() {
            return Ok(Looked::Cancelled);
        }
        let page = source.page(&path, after.as_deref()).await?;
        read = read.saturating_add(page.entries.len() as u64);
        if let Some(entry) = page
            .entries
            .iter()
            .find(|entry| entry.name.to_lowercase() == wanted)
        {
            return Ok(Looked::Found(entry.clone()));
        }
        match page.entries.last() {
            Some(last) => {
                empty = 0;
                after = Some(last.name.clone());
            }
            None => empty += 1,
        }
        if page.end || read >= MAX_FOLDER_ENTRIES || empty > MAX_EMPTY_PAGES {
            return Ok(Looked::Missing);
        }
    }
}

/// Lists `folder` page by page until its end, delivering each page to `sink`
/// as it arrives. See "A listing in pages" in the module documentation.
///
/// Returns how many entries were delivered and how the listing ended.
///
/// # Errors
///
/// Whatever the target answered a page request with, or
/// [`TargetError::Unexpected`] for a folder that keeps answering with an
/// empty page that says more follows. The pages delivered before it stay
/// delivered.
pub(crate) async fn list<S: FolderSource + ?Sized>(
    source: &S,
    memory: &Mutex<Browsed>,
    folder: &RemotePath,
    select: Option<String>,
    cancel: &CancelToken,
    sink: &mut (dyn FnMut(FolderEvent) + Send),
) -> Result<(u64, ListingEnd), TargetError> {
    // Only a folder named by a path is a place the explorer stands in.
    let named = match folder {
        RemotePath::Absolute { path } => Some(path.as_str()),
        RemotePath::Staging { .. } => None,
    };
    let mut select = select;
    let mut after: Option<String> = None;
    let mut delivered = 0u64;
    let mut opened = false;
    let mut empty = 0u32;
    loop {
        if cancel.is_cancelled() {
            return Ok((delivered, ListingEnd::Cancelled));
        }
        let page = source.page(folder, after.as_deref()).await?;
        if !page.end && page.entries.is_empty() {
            // Nothing to continue after: ask again, a bounded number of times.
            empty += 1;
            if empty > MAX_EMPTY_PAGES {
                return Err(TargetError::Unexpected);
            }
            continue;
        }
        empty = 0;
        if let Some(last) = page.entries.last() {
            after = Some(last.name.clone());
        }
        delivered = delivered.saturating_add(page.entries.len() as u64);
        if let Some(path) = named {
            let mut memory = lock(memory);
            if !opened {
                // The target listed it: it is a folder under this name.
                memory.remember(path);
                memory.listing(path);
            }
            memory.listed(path, &page.entries);
        }
        let end = page.end;
        sink(if opened {
            FolderEvent::Page {
                entries: page.entries,
                end,
            }
        } else {
            FolderEvent::Opened {
                folder: folder.clone(),
                select: select.take(),
                entries: page.entries,
                end,
            }
        });
        opened = true;
        if end {
            return Ok((delivered, ListingEnd::Complete));
        }
        if delivered >= MAX_FOLDER_ENTRIES {
            return Ok((delivered, ListingEnd::Limit));
        }
    }
}

/// Finds the place the path `asked` names and lists it: [`locate`], then
/// [`list`].
///
/// # Errors
///
/// See [`locate`] and [`list`]. Nothing is delivered to `sink` unless the
/// folder was found and the target answered its first page.
pub(crate) async fn open<S: FolderSource + ?Sized>(
    source: &S,
    memory: &Mutex<Browsed>,
    asked: &str,
    cancel: &CancelToken,
    sink: &mut (dyn FnMut(FolderEvent) + Send),
) -> Result<FolderReport, TargetError> {
    let Some(located) = locate(source, memory, asked, cancel).await? else {
        return Ok(FolderReport {
            folder: None,
            select: None,
            entries: 0,
            end: ListingEnd::Cancelled,
        });
    };
    let folder = RemotePath::Absolute {
        path: located.folder,
    };
    let select = located.select;
    let (entries, end) = list(source, memory, &folder, select.clone(), cancel, sink).await?;
    Ok(FolderReport {
        folder: Some(folder),
        select,
        entries,
        end,
    })
}

#[cfg(test)]
mod tests;
