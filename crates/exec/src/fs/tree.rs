//! The removal of a folder with everything in it, one bounded step at a time
//! (ST-M010-001, ADR-0012).
//!
//! [`remove_step`] answers [`cretlink_protocol::Operation::RemoveTree`]. It
//! walks the folder depth first, removes what it meets, and stops at its
//! bounds; the caller asks again while the answer is
//! [`TreeState::More`]. Nothing is kept between two steps: a step that follows
//! another walks the folder again from its top, where what the earlier step
//! removed is simply no longer there. What ties the steps of one removal
//! together is the identity of the folder, which the answer carries and the
//! next request gives back.
//!
//! # Never out of the folder
//!
//! The agent that runs this is elevated, and the folder may belong to someone
//! who is not. A program of that person could try to turn the removal against
//! the rest of the computer: make a folder on the way to it a link, plant a
//! link in the folder, or swap a folder for a link while the walk is on its
//! way to it, so that "remove what is in here" reaches a place that was never
//! meant.
//!
//! Three rules keep the removal in its folder:
//!
//! - **The folder is the one that lies at the path, and stays the same.**
//!   The step opens the folder once, as a verified folder (see the `anchor`
//!   module): what it opened must lie where the path says, so a path that
//!   leads through a link is refused ([`ErrorCode::InvalidPath`]) and nothing
//!   is removed. The step then **holds** the folder: nobody renames or
//!   removes it while the step runs. A step that continues a removal is given
//!   the identity of the folder the removal began with, and refuses a folder
//!   with another identity before it removes anything: between two steps the
//!   folder is not held, and a folder that was swapped for another in that
//!   time is not the one the technician asked to remove.
//! - **A link is removed as a link.** A symbolic link, a junction, a volume
//!   mount point or any other entry that stands for another place is removed
//!   itself and never walked. What it points at is not looked at. That holds
//!   for a link in the place of the folder too, in the first step of a
//!   removal; a removal that began with a folder does not go on with a link.
//! - **Nothing is removed by its name alone.** On Windows every removal opens
//!   the entry without following a link, reads **from the opened handle** what
//!   the entry is and where it really lies, and removes it **through that same
//!   handle**. An entry whose real place is not inside the folder is refused
//!   ([`ErrorCode::InvalidPath`]), whatever the name said and whatever was
//!   swapped in the meantime: the check and the removal are about one object,
//!   so there is no moment between them to exploit. The folder itself is
//!   removed through the handle the step holds.
//!
//! The paths of the entries are built from the folder's verified location in
//! the verbatim form, which Win32 does not rewrite: a name that ends in a dot
//! or a space and the name of a device (`nul`) are plain names, and the entry
//! of that name is the one that is opened.
//!
//! A folder that is a reparse point without standing for another place — a
//! folder of a cloud drive such as OneDrive — holds its own content and is
//! walked like any folder, under the same rules.
//!
//! Off Windows the walk never follows the last part of a name either (it
//! removes with `unlink` and `rmdir`), but it verifies neither the folder's
//! location nor the opened object, and it holds nothing: a non-Windows agent
//! is a development and test tool that is never elevated on a customer
//! computer.
//!
//! # Bounded
//!
//! One step settles — removes, or finds it cannot remove — at most
//! [`StepBounds::entries`] entries, and once it has removed something it
//! answers after [`StepBounds::time`]. A folder nested deeper than
//! [`StepBounds::depth`] is reported and left. The answer names the first
//! [`limits::MAX_TREE_FAILURES`] entries that could not be removed and counts
//! the rest, so it has a fixed largest size.
//!
//! # Honest (REQ-N-008)
//!
//! Only a folder that **this step removed** is [`TreeState::Removed`]. A
//! folder that is gone for another reason — it was renamed away or removed by
//! someone else while the step ran — is reported as not found, never as
//! removed: its content may live on elsewhere. An entry that could not be
//! removed keeps every folder above it, and the answer says which entry and
//! why. An entry inside the folder that someone else removed in the meantime
//! is not a failure and is not counted as removed by this step.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cretlink_protocol::{B64Bytes, ErrorCode, OpResult, TreeFailure, TreeState, limits};

use super::anchor::{self, Folder, Hold, Kind};
use super::{FsError, io_error_code, one_line};

/// The bounds of one step.
#[derive(Debug, Clone, Copy)]
pub(super) struct StepBounds {
    /// The most entries the step settles: removes, or finds it cannot remove.
    pub(super) entries: u64,
    /// How long the step works before it answers, once it has removed
    /// something. A step that has removed nothing yet goes on until
    /// [`Self::entries`], so that a slow disk alone never ends a removal.
    pub(super) time: Duration,
    /// The deepest level below the folder that the step walks.
    pub(super) depth: usize,
}

impl StepBounds {
    /// The bounds of the wire operation.
    pub(super) const fn protocol() -> Self {
        Self {
            entries: limits::MAX_TREE_STEP_ENTRIES,
            time: Duration::from_millis(limits::MAX_TREE_STEP_MS),
            depth: limits::MAX_TREE_DEPTH,
        }
    }
}

/// What one step did and where the removal stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StepReport {
    /// Where the removal stands.
    pub(super) state: TreeState,
    /// Files and links the step removed.
    pub(super) removed_files: u64,
    /// Folders the step removed.
    pub(super) removed_dirs: u64,
    /// The first entries the step could not remove.
    pub(super) failed: Vec<TreeFailure>,
    /// How many more it could not remove.
    pub(super) failed_more: u64,
    /// What identifies the folder, for the step that follows: present exactly
    /// when the state is [`TreeState::More`].
    pub(super) folder_id: Option<Vec<u8>>,
}

impl StepReport {
    /// The answer to a request that only ends a removal: nothing was touched.
    pub(super) fn stopped() -> Self {
        Self::ended(TreeState::Stopped)
    }

    /// A report in `state` of a step that removed nothing and failed at
    /// nothing, to be filled in.
    fn ended(state: TreeState) -> Self {
        Self {
            state,
            removed_files: 0,
            removed_dirs: 0,
            failed: Vec::new(),
            failed_more: 0,
            folder_id: None,
        }
    }

    /// The wire result of this step.
    pub(super) fn into_result(self) -> OpResult {
        OpResult::TreeRemoval {
            state: self.state,
            removed_files: self.removed_files,
            removed_dirs: self.removed_dirs,
            failed: self.failed,
            failed_more: self.failed_more,
            folder_id: self.folder_id.map(B64Bytes::new),
        }
    }
}

/// Why an entry was not removed.
#[derive(Debug)]
enum Refusal {
    /// The operating system refused.
    Io(std::io::Error),
    /// The entry does not lie in the folder being removed.
    #[cfg_attr(
        not(windows),
        allow(
            dead_code,
            reason = "only the Windows walk verifies the place of an opened entry"
        )
    )]
    Outside,
    /// The entry is no longer what it was when the walk met it.
    Changed,
    /// The folder is nested deeper than the walk goes.
    TooDeep,
}

impl Refusal {
    /// Whether the entry is simply no longer there.
    fn is_gone(&self) -> bool {
        matches!(self, Self::Io(error) if io_error_code(error) == ErrorCode::PathNotFound)
    }

    /// The reason in the wire vocabulary.
    fn code(&self) -> ErrorCode {
        match self {
            Self::Io(error) => io_error_code(error),
            Self::Outside => ErrorCode::InvalidPath,
            Self::Changed => ErrorCode::WrongPathKind,
            Self::TooDeep => ErrorCode::LimitExceeded,
        }
    }
}

impl From<std::io::Error> for Refusal {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// One folder the walk is emptying.
struct Level {
    /// The folder.
    path: PathBuf,
    /// The entries of it that the walk has not met yet.
    entries: std::fs::ReadDir,
    /// Whether something in it could not be removed, so that it cannot be
    /// removed either.
    kept: bool,
}

/// The state of one step.
struct Walk<'a> {
    /// The folder being removed, held until the step removes it or ends.
    root: Option<Folder>,
    guard: sys::Guard,
    bounds: StepBounds,
    stop: &'a dyn Fn() -> bool,
    started: Instant,
    removed_files: u64,
    removed_dirs: u64,
    failed: Vec<TreeFailure>,
    failed_total: u64,
}

/// Removes as much of the folder at `root` as one step may.
///
/// `began` is absent in the first step of a removal, which takes the folder
/// that lies at `root`. A step that continues a removal gives the identity
/// the removal began with — the `folder_id` of the answer before — and
/// removes nothing from a folder with another one.
///
/// `stop` is asked before every entry: once it answers `true` the step ends
/// where it is and reports [`TreeState::Stopped`] with what it had removed
/// until then. Blocking: it works on the file system, so the caller runs it
/// off the async runtime.
///
/// # Errors
///
/// When the folder itself cannot be taken up: it does not exist
/// ([`ErrorCode::PathNotFound`]), it is a file ([`ErrorCode::WrongPathKind`]),
/// it is a drive, its path leads through a link, or it is not the folder the
/// removal began with ([`ErrorCode::InvalidPath`]), its file system does not
/// tell its folders apart ([`ErrorCode::Unsupported`]), or it cannot be
/// opened. Nothing was removed then.
pub(super) fn remove_step(
    root: &Path,
    began: Option<&[u8]>,
    bounds: StepBounds,
    stop: &dyn Fn() -> bool,
) -> Result<StepReport, FsError> {
    // Refused from the path alone, before anything on the disk is touched.
    if is_drive(root) {
        return Err(a_drive());
    }
    let folder = Folder::open(root, Hold::ForRemoval).map_err(|refusal| match refusal {
        anchor::Refusal::Elsewhere => FsError::new(ErrorCode::InvalidPath, anchor::LEADS_ELSEWHERE),
        anchor::Refusal::Io(error) => root_error(&error, "failed to open the folder"),
    })?;
    // And again from where the opened folder really lies.
    if folder.is_drive() {
        return Err(a_drive());
    }
    match (folder.kind(), began) {
        (Kind::File, None) => Err(FsError::wrong_kind("the path is not a folder")),
        // A link in the place of the folder is removed as a link: what it
        // points at is not walked.
        (Kind::Link, None) => {
            folder
                .remove()
                .map_err(|error| root_error(&error, "failed to remove the link"))?;
            Ok(StepReport {
                removed_files: 1,
                ..StepReport::ended(TreeState::Removed)
            })
        }
        // A removal that began with a folder goes on with that folder only.
        // What lies at the path now is something else, whatever it is.
        (Kind::File | Kind::Link, Some(_)) => Err(another_folder()),
        (Kind::Dir, began) => {
            // The identity is read before anything is removed: a step that
            // leaves more to do must be able to name its folder.
            let Some(identity) = folder.identity() else {
                return Err(FsError::new(
                    ErrorCode::Unsupported,
                    "the file system does not tell its folders apart",
                ));
            };
            if began.is_some_and(|began| began != identity) {
                return Err(another_folder());
            }
            let guard = sys::Guard::of(&folder)
                .map_err(|error| root_error(&error, "failed to open the folder"))?;
            let top = open_level(folder.path())
                .map_err(|error| root_error(&error, "failed to list the folder"))?;
            let mut walk = Walk {
                root: Some(folder),
                guard,
                bounds,
                stop,
                started: Instant::now(),
                removed_files: 0,
                removed_dirs: 0,
                failed: Vec::new(),
                failed_total: 0,
            };
            let state = walk.run(top);
            Ok(walk.report(state, identity))
        }
    }
}

/// Whether `path` names a whole drive rather than a folder on one: `C:\` has
/// no folder above it, and neither has `/`. A drive is never removed "as a
/// folder", whoever asks.
fn is_drive(path: &Path) -> bool {
    path.parent().is_none()
}

/// The refusal of a drive.
fn a_drive() -> FsError {
    FsError::new(
        ErrorCode::InvalidPath,
        "a drive cannot be removed as a folder",
    )
}

/// The refusal of a step that would continue a removal in another folder
/// than the one it began with.
fn another_folder() -> FsError {
    FsError::new(
        ErrorCode::InvalidPath,
        "the folder is not the one this removal began with",
    )
}

/// The error for a folder that cannot be taken up at all.
fn root_error(error: &std::io::Error, context: &'static str) -> FsError {
    match io_error_code(error) {
        ErrorCode::PathNotFound => FsError::path_not_found(),
        code => FsError::new(code, context),
    }
}

/// Starts listing the folder at `path`.
fn open_level(path: &Path) -> std::io::Result<Level> {
    Ok(Level {
        path: path.to_path_buf(),
        entries: std::fs::read_dir(path)?,
        kept: false,
    })
}

impl Walk<'_> {
    /// Walks from `top` until the folder is done or a bound is reached, and
    /// says where the removal stands.
    fn run(&mut self, top: Level) -> TreeState {
        let mut levels = vec![top];
        loop {
            if (self.stop)() {
                return TreeState::Stopped;
            }
            if self.out_of_bounds() {
                // The folder is not done. A step that removed something made
                // room for the next one; a step that removed nothing met only
                // what cannot be removed, and another step would meet it again.
                return if self.removed() > 0 {
                    TreeState::More
                } else {
                    TreeState::Blocked
                };
            }
            let depth = levels.len();
            let Some(level) = levels.last_mut() else {
                // Unreachable: the loop returns when the last level is popped.
                return TreeState::Incomplete;
            };
            match level.entries.next() {
                Some(Ok(entry)) => {
                    let path = entry.path();
                    // The listing's own word on what the entry is saves a
                    // look at every file. It decides nothing: a folder is
                    // looked at again before it is listed, and every removal
                    // reads the kind from the entry it has opened.
                    let listed = entry.file_type().ok();
                    let is_dir = match listed {
                        Some(kind) => kind.is_dir(),
                        None => match self.guard.kind(&path) {
                            Ok(kind) => kind == Kind::Dir,
                            Err(refusal) => {
                                if !refusal.is_gone() {
                                    self.fail(&path, &refusal);
                                    level.kept = true;
                                }
                                continue;
                            }
                        },
                    };
                    if is_dir {
                        match self.descend(&path, depth) {
                            Ok(Some(next)) => levels.push(next),
                            // It was not a folder after all, or it is gone.
                            Ok(None) => {}
                            Err(refusal) => {
                                self.fail(&path, &refusal);
                                level.kept = true;
                            }
                        }
                    } else {
                        match self.guard.remove(&path, false) {
                            Ok(()) => self.removed_files += 1,
                            Err(refusal) if refusal.is_gone() => {}
                            Err(refusal) => {
                                self.fail(&path, &refusal);
                                level.kept = true;
                            }
                        }
                    }
                }
                // The listing broke off: what it had not shown yet is unknown,
                // so the folder is kept and reported with the reason.
                Some(Err(error)) => {
                    let refusal = Refusal::Io(error);
                    let path = level.path.clone();
                    self.fail(&path, &refusal);
                    level.kept = true;
                    if let Some(state) = self.close(&mut levels) {
                        return state;
                    }
                }
                None => {
                    if let Some(state) = self.close(&mut levels) {
                        return state;
                    }
                }
            }
        }
    }

    /// Takes up the folder at `path`, one level below `depth`, for listing.
    /// `Ok(None)` when there is nothing to walk: it is gone, or it turned out
    /// to be a link or a file, which is then removed as one.
    fn descend(&mut self, path: &Path, depth: usize) -> Result<Option<Level>, Refusal> {
        match self.guard.kind(path) {
            Ok(Kind::Dir) => {}
            // The listing called it a folder and it is not one (any more): it
            // is removed as what it is, never walked.
            Ok(Kind::Link | Kind::File) => {
                return match self.guard.remove(path, false) {
                    Ok(()) => {
                        self.removed_files += 1;
                        Ok(None)
                    }
                    Err(refusal) if refusal.is_gone() => Ok(None),
                    Err(refusal) => Err(refusal),
                };
            }
            Err(refusal) if refusal.is_gone() => return Ok(None),
            Err(refusal) => return Err(refusal),
        }
        if depth > self.bounds.depth {
            return Err(Refusal::TooDeep);
        }
        match open_level(path).map_err(Refusal::Io) {
            Ok(level) => Ok(Some(level)),
            Err(refusal) if refusal.is_gone() => Ok(None),
            Err(refusal) => Err(refusal),
        }
    }

    /// Finishes the innermost folder: removes it unless something in it was
    /// kept, and tells the folder above. Returns the final state once the
    /// folder being removed itself is finished.
    fn close(&mut self, levels: &mut Vec<Level>) -> Option<TreeState> {
        let Level {
            path,
            entries,
            kept,
        } = levels.pop()?;
        // The listing holds the folder open; it is closed before the folder
        // is removed, so that the folder is gone for the one above it too.
        drop(entries);
        let is_root = levels.is_empty();
        let gone = if kept {
            false
        } else {
            let removed = if is_root {
                self.remove_root()
            } else {
                self.guard.remove(&path, true)
            };
            match removed {
                Ok(()) => {
                    self.removed_dirs += 1;
                    true
                }
                // Someone else removed a folder inside: it is gone, though
                // not by this step. The folder being removed itself is a
                // different matter: "removed" is said only of what this step
                // removed, so a folder that vanished is reported as such.
                Err(refusal) if !is_root && refusal.is_gone() => true,
                Err(refusal) => {
                    self.fail(&path, &refusal);
                    false
                }
            }
        };
        match levels.last_mut() {
            Some(above) => {
                if !gone {
                    above.kept = true;
                }
                None
            }
            None => Some(if gone {
                TreeState::Removed
            } else {
                TreeState::Incomplete
            }),
        }
    }

    /// Removes the folder being removed itself, now that it is empty, through
    /// the hold the step has on it.
    fn remove_root(&mut self) -> Result<(), Refusal> {
        match self.root.take() {
            Some(folder) => folder.remove().map_err(Refusal::Io),
            // Unreachable: the folder is given up only here, once.
            None => Err(Refusal::Changed),
        }
    }

    /// Entries this step removed.
    fn removed(&self) -> u64 {
        self.removed_files + self.removed_dirs
    }

    /// Whether the step has reached one of its bounds.
    fn out_of_bounds(&self) -> bool {
        self.removed() + self.failed_total >= self.bounds.entries
            || (self.removed() > 0 && self.started.elapsed() >= self.bounds.time)
    }

    /// Records that `path` could not be removed: the first ones by name and
    /// reason, the rest by count.
    fn fail(&mut self, path: &Path, refusal: &Refusal) {
        self.failed_total += 1;
        if self.failed.len() < limits::MAX_TREE_FAILURES {
            self.failed.push(TreeFailure {
                path: one_line(&anchor::plain(path), limits::MAX_TREE_FAILURE_PATH_BYTES),
                code: refusal.code(),
            });
        }
    }

    /// The report of this step, ending in `state`, of the folder with the
    /// identity `folder`.
    fn report(&mut self, state: TreeState, folder: Vec<u8>) -> StepReport {
        let failed = std::mem::take(&mut self.failed);
        StepReport {
            state,
            removed_files: self.removed_files,
            removed_dirs: self.removed_dirs,
            failed_more: self.failed_total - failed.len() as u64,
            failed,
            // Only a step that leaves more to do has a next step to bind.
            folder_id: (state == TreeState::More).then_some(folder),
        }
    }
}

#[cfg(not(windows))]
mod sys {
    use std::path::Path;

    use super::{Folder, Kind, Refusal};

    /// The walk's access to the file system. Off Windows it holds nothing:
    /// see the module documentation.
    pub(super) struct Guard;

    impl Guard {
        /// The access to what lies in `folder`.
        pub(super) fn of(_folder: &Folder) -> std::io::Result<Self> {
            Ok(Self)
        }

        /// What the entry at `path` is, without following a link.
        pub(super) fn kind(&self, path: &Path) -> Result<Kind, Refusal> {
            let kind = std::fs::symlink_metadata(path)?.file_type();
            Ok(if kind.is_symlink() {
                Kind::Link
            } else if kind.is_dir() {
                Kind::Dir
            } else {
                Kind::File
            })
        }

        /// Removes the entry at `path`: an empty folder when `dir`, else a
        /// file or a link. Neither call follows a link in the last part of
        /// the name: `rmdir` refuses a link, and `unlink` removes the link.
        pub(super) fn remove(&self, path: &Path, dir: bool) -> Result<(), Refusal> {
            if dir {
                std::fs::remove_dir(path)?;
            } else {
                std::fs::remove_file(path)?;
            }
            Ok(())
        }
    }
}

#[cfg(windows)]
mod sys {
    use std::fs::File;
    use std::path::Path;

    use windows::Win32::Storage::FileSystem::{
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    use super::super::anchor;
    use super::{Folder, Kind, Refusal};

    /// The path separator in a final path.
    const SEPARATOR: u16 = b'\\' as u16;

    /// Every other use of an entry by others stays allowed while the walk
    /// has it open.
    const SHARED: u32 = FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0;

    /// The walk's measure for every entry: where the folder being removed
    /// really lies, read from its opened handle.
    pub(super) struct Guard {
        /// The final path of the folder, without a separator at its end.
        root: Vec<u16>,
    }

    impl Guard {
        /// Reads where the opened `folder` lies.
        pub(super) fn of(folder: &Folder) -> std::io::Result<Self> {
            let mut place = folder.place()?;
            while place.last() == Some(&SEPARATOR) {
                place.pop();
            }
            Ok(Self { root: place })
        }

        /// What the entry at `path` is, read from the opened entry without
        /// following a link. A folder is about to be listed, so it must also
        /// lie in the folder being removed.
        pub(super) fn kind(&self, path: &Path) -> Result<Kind, Refusal> {
            let entry = anchor::open_entry(path, FILE_READ_ATTRIBUTES.0, SHARED)?;
            let kind = anchor::kind_of(&entry)?;
            if kind == Kind::Dir {
                self.require_inside(&entry)?;
            }
            Ok(kind)
        }

        /// Removes the entry at `path`: an empty folder when `dir`, else a
        /// file or a link.
        ///
        /// The entry is opened without following a link; what it is and where
        /// it lies are read from that handle, and it is removed through that
        /// handle. So the entry that is removed is the entry that was checked,
        /// whatever its name pointed at a moment earlier or later.
        pub(super) fn remove(&self, path: &Path, dir: bool) -> Result<(), Refusal> {
            let entry = anchor::open_for_removal(path, SHARED)?;
            if (anchor::kind_of(&entry)? == Kind::Dir) != dir {
                return Err(Refusal::Changed);
            }
            self.require_inside(&entry)?;
            Ok(anchor::delete(&entry)?)
        }

        /// Refuses an opened entry that is neither the folder being removed
        /// nor inside it.
        fn require_inside(&self, entry: &File) -> Result<(), Refusal> {
            let place = anchor::place_of(entry)?;
            let root = self.root.as_slice();
            let inside = place.len() >= root.len()
                && place[..root.len()] == *root
                && (place.len() == root.len() || place[root.len()] == SEPARATOR);
            if inside {
                Ok(())
            } else {
                Err(Refusal::Outside)
            }
        }
    }
}

#[cfg(test)]
mod tests;
