//! The removal of a folder with everything in it, one bounded step at a time
//! (ST-M010-001, ADR-0012).
//!
//! [`remove_step`] answers [`cretlink_protocol::Operation::RemoveTree`]. It
//! walks the folder depth first, removes what it meets, and stops at its
//! bounds; the caller asks again while the answer is
//! [`TreeState::More`]. Nothing is kept between two steps: a step that follows
//! another walks the folder again from its top, where what the earlier step
//! removed is simply no longer there.
//!
//! # Never out of the folder
//!
//! The agent that runs this is elevated, and the folder may belong to someone
//! who is not. A program of that person could try to turn the removal against
//! the rest of the computer: plant a link in the folder, or swap a folder for a
//! link while the walk is on its way to it, so that "remove what is in here"
//! reaches a place that was never meant.
//!
//! Two rules keep the walk in its folder:
//!
//! - **A link is removed as a link.** A symbolic link, a junction, a volume
//!   mount point or any other entry that stands for another place is removed
//!   itself and never walked. What it points at is not looked at.
//! - **Nothing is removed by its name alone.** On Windows every removal opens
//!   the entry without following a link, reads **from the opened handle** what
//!   the entry is and where it really lies, and removes it **through that same
//!   handle**. An entry whose real place is not inside the folder is refused
//!   ([`ErrorCode::InvalidPath`]), whatever the name said and whatever was
//!   swapped in the meantime: the check and the removal are about one object,
//!   so there is no moment between them to exploit.
//!
//! A folder that is a reparse point without standing for another place — a
//! folder of a cloud drive such as OneDrive — holds its own content and is
//! walked like any folder, under the same second rule.
//!
//! Off Windows the walk never follows the last part of a name either (it
//! removes with `unlink` and `rmdir`), but it does not verify the opened
//! object: a non-Windows agent is a development and test tool that is never
//! elevated on a customer computer.
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
//! Only a folder that is gone is [`TreeState::Removed`]. An entry that could
//! not be removed keeps every folder above it, and the answer says which
//! entry and why. An entry that someone else removed in the meantime is not a
//! failure and is not counted as removed by this step.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cretlink_protocol::{ErrorCode, OpResult, TreeFailure, TreeState, limits};

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
}

impl StepReport {
    /// The answer to a request that only ends a removal: nothing was touched.
    pub(super) fn stopped() -> Self {
        Self {
            state: TreeState::Stopped,
            removed_files: 0,
            removed_dirs: 0,
            failed: Vec::new(),
            failed_more: 0,
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
        }
    }
}

/// What an entry is, read without following a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A folder that holds its own content.
    Dir,
    /// An entry that stands for another place: a symbolic link, a junction,
    /// a volume mount point.
    Link,
    /// Anything else: a file, or a device, pipe or socket.
    File,
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
    #[cfg_attr(
        not(windows),
        allow(
            dead_code,
            reason = "only the Windows walk reads the kind from the opened entry"
        )
    )]
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
/// `stop` is asked before every entry: once it answers `true` the step ends
/// where it is and reports [`TreeState::Stopped`] with what it had removed
/// until then. Blocking: it works on the file system, so the caller runs it
/// off the async runtime.
///
/// # Errors
///
/// When the folder itself cannot be taken up: it does not exist
/// ([`ErrorCode::PathNotFound`]), it is a file ([`ErrorCode::WrongPathKind`]),
/// it is a drive ([`ErrorCode::InvalidPath`]), or it cannot be opened. Nothing
/// was removed then.
pub(super) fn remove_step(
    root: &Path,
    bounds: StepBounds,
    stop: &dyn Fn() -> bool,
) -> Result<StepReport, FsError> {
    if is_drive(root) {
        return Err(FsError::new(
            ErrorCode::InvalidPath,
            "a drive cannot be removed as a folder",
        ));
    }
    let (guard, kind) = sys::Guard::open(root)
        .map_err(|refusal| root_error(&refusal, "failed to open the folder"))?;
    let mut walk = Walk {
        guard,
        bounds,
        stop,
        started: Instant::now(),
        removed_files: 0,
        removed_dirs: 0,
        failed: Vec::new(),
        failed_total: 0,
    };
    match kind {
        Kind::File => Err(FsError::wrong_kind("the path is not a folder")),
        // A link in the place of the folder is removed as a link: what it
        // points at is not walked.
        Kind::Link => {
            walk.guard
                .remove(root, false)
                .map_err(|refusal| root_error(&refusal, "failed to remove the link"))?;
            walk.removed_files = 1;
            Ok(walk.report(TreeState::Removed))
        }
        Kind::Dir => {
            let top = open_level(root)
                .map_err(|refusal| root_error(&refusal, "failed to list the folder"))?;
            let state = walk.run(top);
            Ok(walk.report(state))
        }
    }
}

/// Whether `path` names a whole drive rather than a folder on one: `C:\` has
/// no folder above it, and neither has `/`. A drive is never removed "as a
/// folder", whoever asks.
fn is_drive(path: &Path) -> bool {
    path.parent().is_none()
}

/// The error for a folder that cannot be taken up at all.
fn root_error(refusal: &Refusal, context: &'static str) -> FsError {
    match refusal.code() {
        ErrorCode::PathNotFound => FsError::path_not_found(),
        code => FsError::new(code, context),
    }
}

/// Starts listing the folder at `path`.
fn open_level(path: &Path) -> Result<Level, Refusal> {
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
        match open_level(path) {
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
        let gone = if kept {
            false
        } else {
            match self.guard.remove(&path, true) {
                Ok(()) => {
                    self.removed_dirs += 1;
                    true
                }
                // Someone else removed it: it is gone, though not by this step.
                Err(refusal) if refusal.is_gone() => true,
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
                path: one_line(&path.to_string_lossy(), limits::MAX_TREE_FAILURE_PATH_BYTES),
                code: refusal.code(),
            });
        }
    }

    /// The report of this step, ending in `state`.
    fn report(&mut self, state: TreeState) -> StepReport {
        let failed = std::mem::take(&mut self.failed);
        StepReport {
            state,
            removed_files: self.removed_files,
            removed_dirs: self.removed_dirs,
            failed_more: self.failed_total - failed.len() as u64,
            failed,
        }
    }
}

#[cfg(not(windows))]
mod sys {
    use std::path::Path;

    use super::{Kind, Refusal};

    /// The walk's access to the file system. Off Windows it holds nothing:
    /// see the module documentation.
    pub(super) struct Guard;

    impl Guard {
        /// Takes up the folder at `root` and says what it is.
        pub(super) fn open(root: &Path) -> Result<(Self, Kind), Refusal> {
            let kind = Self.kind(root)?;
            Ok((Self, kind))
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
    #![allow(
        unsafe_code,
        reason = "Localized Win32 handle calls that keep a folder removal \
                  inside its folder (ST-M010-001): the final path of an opened \
                  entry and its removal through that handle. ADR-0001 and \
                  ADR-0006 permit reviewed, localized unsafe in one platform \
                  adapter."
    )]

    use std::ffi::c_void;
    use std::fs::{File, OpenOptions};
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        DELETE, FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
        FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX,
        FILE_DISPOSITION_INFO_EX_FLAGS, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, FileDispositionInfo, FileDispositionInfoEx,
        GETFINALPATHNAMEBYHANDLE_FLAGS, GetFinalPathNameByHandleW, SetFileInformationByHandle,
        VOLUME_NAME_NT,
    };

    use super::{Kind, Refusal};

    /// The path separator in a final path.
    const SEPARATOR: u16 = b'\\' as u16;

    /// The longest final path the walk reads, in UTF-16 units. A path on
    /// Windows has at most 32767 of them; the device prefix adds a few.
    const MAX_FINAL_PATH_UNITS: usize = 33_000;

    // Windows error codes that say the extended form of a removal is not
    // offered here: by an older Windows, or by a file system such as FAT.
    const ERROR_INVALID_FUNCTION: i32 = 1;
    const ERROR_NOT_SUPPORTED: i32 = 50;
    const ERROR_INVALID_PARAMETER: i32 = 87;

    /// The walk's hold on the folder being removed: where the folder really
    /// lies, read from its opened handle. Every entry is measured against it.
    pub(super) struct Guard {
        /// The final path of the folder, without a separator at its end.
        root: Vec<u16>,
    }

    impl Guard {
        /// Takes up the folder at `root`: reads what it is and where it lies.
        pub(super) fn open(root: &Path) -> Result<(Self, Kind), Refusal> {
            let entry = open(root, FILE_READ_ATTRIBUTES.0)?;
            let kind = kind_of(&entry)?;
            let mut place = final_path(&entry)?;
            while place.last() == Some(&SEPARATOR) {
                place.pop();
            }
            Ok((Self { root: place }, kind))
        }

        /// What the entry at `path` is, read from the opened entry without
        /// following a link. A folder is about to be listed, so it must also
        /// lie in the folder being removed.
        pub(super) fn kind(&self, path: &Path) -> Result<Kind, Refusal> {
            let entry = open(path, FILE_READ_ATTRIBUTES.0)?;
            let kind = kind_of(&entry)?;
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
            let entry = open_for_removal(path)?;
            if (kind_of(&entry)? == Kind::Dir) != dir {
                return Err(Refusal::Changed);
            }
            self.require_inside(&entry)?;
            delete(&entry)
        }

        /// Refuses an opened entry that is neither the folder being removed
        /// nor inside it.
        fn require_inside(&self, entry: &File) -> Result<(), Refusal> {
            let place = final_path(entry)?;
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

    /// Opens the entry at `path` itself — never what a link there points at —
    /// with the given access, leaving every other use of it by others allowed.
    fn open(path: &Path, access: u32) -> Result<File, Refusal> {
        OpenOptions::new()
            .access_mode(access)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
            // `BACKUP_SEMANTICS` lets a folder be opened; `OPEN_REPARSE_POINT`
            // opens a link instead of following it.
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)
            .map_err(Refusal::Io)
    }

    /// Opens the entry at `path` for removal. The right to change attributes
    /// is asked for as well, so that a read-only entry can be made removable
    /// where the file system needs that; an entry that does not grant it is
    /// opened with the right to delete alone.
    fn open_for_removal(path: &Path) -> Result<File, Refusal> {
        let wanted = DELETE.0 | FILE_READ_ATTRIBUTES.0;
        match open(path, wanted | FILE_WRITE_ATTRIBUTES.0) {
            Err(Refusal::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                open(path, wanted)
            }
            opened => opened,
        }
    }

    /// What an opened entry is. The standard library reads it from the handle
    /// and calls a reparse point a symbolic link exactly when its tag marks it
    /// as standing for another place (a symbolic link, a junction, a volume
    /// mount point); a folder with any other reparse tag is a folder.
    fn kind_of(entry: &File) -> Result<Kind, Refusal> {
        let kind = entry.metadata()?.file_type();
        Ok(if kind.is_symlink() {
            Kind::Link
        } else if kind.is_dir() {
            Kind::Dir
        } else {
            Kind::File
        })
    }

    /// Where an opened entry really lies: its final path, with every link on
    /// the way resolved, in the device form that every volume has.
    fn final_path(entry: &File) -> Result<Vec<u16>, Refusal> {
        let handle = HANDLE(entry.as_raw_handle());
        let flags = GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | VOLUME_NAME_NT.0);
        let mut place = vec![0u16; 512];
        loop {
            // SAFETY: the handle is open for as long as `entry` is borrowed,
            // and `place` is a writable buffer whose length the wrapper
            // passes with it.
            let len = unsafe { GetFinalPathNameByHandleW(handle, &mut place, flags) } as usize;
            if len == 0 {
                return Err(Refusal::Io(std::io::Error::last_os_error()));
            }
            // A length below the buffer's is the path, without its
            // terminator; anything else is the size the buffer must have.
            if len < place.len() {
                place.truncate(len);
                return Ok(place);
            }
            if len > MAX_FINAL_PATH_UNITS {
                return Err(Refusal::Io(std::io::Error::from(
                    std::io::ErrorKind::InvalidInput,
                )));
            }
            place.resize(len + 1, 0);
        }
    }

    /// Removes an opened entry through its handle.
    fn delete(entry: &File) -> Result<(), Refusal> {
        let handle = HANDLE(entry.as_raw_handle());
        // The form of Windows 10 1809 and later: the name is gone at once,
        // even while others still hold the entry open, and a read-only entry
        // is removed like any other.
        let extended = FILE_DISPOSITION_INFO_EX {
            Flags: FILE_DISPOSITION_INFO_EX_FLAGS(
                FILE_DISPOSITION_FLAG_DELETE.0
                    | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS.0
                    | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE.0,
            ),
        };
        // SAFETY: the handle is open for as long as `entry` is borrowed, and
        // the pointer is to an initialized value of the structure this class
        // takes, with its size, read only during the call.
        let removed = unsafe {
            SetFileInformationByHandle(
                handle,
                FileDispositionInfoEx,
                std::ptr::from_ref(&extended).cast::<c_void>(),
                size_of_u32::<FILE_DISPOSITION_INFO_EX>(),
            )
        };
        let error = match removed {
            Ok(()) => return Ok(()),
            Err(error) => os_error(&error),
        };
        if !matches!(
            error.raw_os_error(),
            Some(ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED | ERROR_INVALID_PARAMETER)
        ) {
            return Err(Refusal::Io(error));
        }
        // An older Windows, or a file system without the extended form (FAT,
        // exFAT): a read-only entry is made removable first, then the entry
        // is marked for removal the plain way.
        let facts = entry.metadata()?;
        let mut permissions = facts.permissions();
        if permissions.readonly() {
            #[allow(
                clippy::permissions_set_readonly_false,
                reason = "Windows only: this clears the read-only attribute of an entry that is being removed"
            )]
            permissions.set_readonly(false);
            entry.set_permissions(permissions)?;
        }
        let plain = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: as above, for the plain class and its structure.
        unsafe {
            SetFileInformationByHandle(
                handle,
                FileDispositionInfo,
                std::ptr::from_ref(&plain).cast::<c_void>(),
                size_of_u32::<FILE_DISPOSITION_INFO>(),
            )
        }
        .map_err(|error| Refusal::Io(os_error(&error)))
    }

    /// The size of `T` as the `u32` the Win32 calls take.
    fn size_of_u32<T>() -> u32 {
        u32::try_from(size_of::<T>()).unwrap_or(0)
    }

    /// The I/O error for a failed Win32 call, carrying the Windows error code
    /// so that it is classified like any other. The `windows` crate reports
    /// such a failure as an `HRESULT` that holds the code in its low half.
    fn os_error(error: &windows::core::Error) -> std::io::Error {
        const WIN32_FACILITY: u32 = 0x8007_0000;
        let hresult = error.code().0.cast_unsigned();
        if hresult & 0xFFFF_0000 == WIN32_FACILITY {
            std::io::Error::from_raw_os_error((hresult & 0xFFFF).cast_signed())
        } else {
            std::io::Error::other("the removal failed")
        }
    }
}

#[cfg(test)]
mod tests;
