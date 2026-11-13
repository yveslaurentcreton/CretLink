//! One entry, changed inside the folder that holds it, on Windows
//! (ST-M010-004, ADR-0012).
//!
//! Deleting a file, removing an empty folder, creating a folder and moving an
//! entry each change one entry of a folder. The agent that does it is
//! elevated, and the folder may belong to an account that is not. A program
//! of that account can make a folder on the way a junction, or swap the
//! folder for one between two requests or between a check and the change, so
//! that an operation which resolved its path by name would delete, create or
//! move in the place the junction leads to.
//!
//! So each of these operations takes up the folder that holds the entry
//! **once**, as a verified folder (see the `anchor` module): it must be a
//! real folder that lies where the request says, and a path that reaches it
//! through a link is refused ([`ErrorCode::InvalidPath`]) before anything
//! changes. Everything after that happens inside the opened folder. The
//! entry is opened by its one name relative to the folder's handle, what it
//! is is read from that handle, and it is removed or renamed through that
//! same handle; a new folder is created relative to the handle of the folder
//! it is created in. No path is resolved a second time, so nothing that
//! happens to a path meanwhile changes where the operation acts.
//!
//! A move takes up both folders before it changes anything, and renames
//! through the handle of the entry into the handle of the destination's
//! folder.
//!
//! # A link as the entry itself
//!
//! Only the way to the entry must be free of links. A link that *is* the
//! entry keeps the meaning it always had: a delete removes the link and never
//! what it points at, an empty-folder removal refuses it, a folder is not
//! created under its name, and a move moves the link. The entry is opened as
//! the link it is, never followed.
//!
//! # The staging area
//!
//! The session's own staging folder is CretLink's own: since ST-M006-020 no
//! other account can change a folder there. An operation on a staging name
//! keeps working by path and never comes here. One end of a move may be a
//! staging name while the other is a path of the request: that end is then
//! taken as it is ([`Side::Staged`]), and the folder of the other end is
//! verified.
//!
//! The functions block: they work on the file system, so the caller runs
//! them off the async runtime.

use std::fs::File;
use std::path::{Path, PathBuf};

use cretlink_protocol::ErrorCode;
use windows::Win32::Storage::FileSystem::{
    DELETE, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

use super::anchor::{self, Disposition, Folder, Hold, Kind};
use super::{FsError, io_error_code};

/// Every use of an entry by others stays allowed while it is open here.
const SHARED: u32 = FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0;

/// The right to read what an entry is.
const TO_LOOK: u32 = FILE_READ_ATTRIBUTES.0;

/// The rights to rename an entry (which takes the right to delete) and to
/// read what it is.
const TO_MOVE: u32 = DELETE.0 | FILE_READ_ATTRIBUTES.0;

/// One end of a move.
#[derive(Debug)]
pub(super) enum Side {
    /// A path that the request names. The folder that holds the entry is
    /// taken up as a verified folder.
    Named(PathBuf),
    /// A file under its name in the session's own staging folder, which is
    /// CretLink's own: the folder is taken as it is.
    Staged(PathBuf),
}

impl Side {
    /// Takes up the folder of this end and names the entry in it.
    fn open(&self) -> Result<(Folder, &str), FsError> {
        match self {
            Self::Named(path) => {
                let (folder, name) = split(path).ok_or_else(|| {
                    FsError::new(
                        ErrorCode::InvalidPath,
                        "a drive cannot be moved or replaced",
                    )
                })?;
                Ok((open_folder(folder)?, name))
            }
            Self::Staged(path) => {
                // A staging name is one name in the staging folder.
                let (folder, name) = split(path).ok_or_else(FsError::outside_staging)?;
                let folder = Folder::open_own(folder)
                    .map_err(|error| not_found_or(&error, "failed to open the staging area"))?;
                if folder.kind() != Kind::Dir {
                    return Err(FsError::not_ours());
                }
                Ok((folder, name))
            }
        }
    }
}

/// Deletes the file or the link at `path`
/// ([`cretlink_protocol::Operation::FsDelete`]). A folder is refused; a link
/// is removed as the link, never what it points at.
pub(super) fn delete(path: &Path) -> Result<(), FsError> {
    let Some((folder, name)) = split(path) else {
        // A drive is a folder.
        return Err(FsError::wrong_kind("the path is a directory"));
    };
    delete_in(&open_folder(folder)?, name)
}

/// Deletes the file or the link `name` in the opened folder.
pub(super) fn delete_in(folder: &Folder, name: &str) -> Result<(), FsError> {
    const FAILED: &str = "failed to delete the file";
    let a_folder = || FsError::wrong_kind("the path is a directory");
    let entry = match anchor::open_at_for_removal(folder, name) {
        Ok(entry) => entry,
        // A folder is refused as a folder, as it always was: also one that
        // cannot be opened for a removal, because it is in use or protected.
        Err(_) if kind_at(folder, name) == Some(Kind::Dir) => return Err(a_folder()),
        Err(error) => return Err(not_found_or(&error, FAILED)),
    };
    if kind_of(&entry)? == Kind::Dir {
        return Err(a_folder());
    }
    anchor::delete(&entry).map_err(|e| FsError::from_io(FAILED, &e))
}

/// Removes the empty folder at `path`
/// ([`cretlink_protocol::Operation::RemoveDir`]). A file is refused, and so
/// is a link, whatever it points at.
pub(super) fn remove_dir(path: &Path) -> Result<(), FsError> {
    let Some((folder, name)) = split(path) else {
        return Err(FsError::new(
            ErrorCode::InvalidPath,
            "a drive cannot be removed as a folder",
        ));
    };
    remove_dir_in(&open_folder(folder)?, name)
}

/// Removes the empty folder `name` in the opened folder.
pub(super) fn remove_dir_in(folder: &Folder, name: &str) -> Result<(), FsError> {
    const FAILED: &str = "failed to remove the directory";
    let not_a_folder = || FsError::wrong_kind("the path is not a directory");
    let entry = match anchor::open_at_for_removal(folder, name) {
        Ok(entry) => entry,
        // What is not a folder is refused as that, as it always was: also a
        // file that cannot be opened for a removal, because it is in use or
        // protected.
        Err(_) if kind_at(folder, name).is_some_and(|kind| kind != Kind::Dir) => {
            return Err(not_a_folder());
        }
        Err(error) => return Err(not_found_or(&error, FAILED)),
    };
    if kind_of(&entry)? != Kind::Dir {
        return Err(not_a_folder());
    }
    anchor::delete(&entry).map_err(|e| FsError::from_io(FAILED, &e))
}

/// Creates the folder at `path`
/// ([`cretlink_protocol::Operation::CreateDir`]): one level, in a folder that
/// exists. A name that is taken, also by a link, is refused.
pub(super) fn create_dir(path: &Path) -> Result<(), FsError> {
    let Some((folder, name)) = split(path) else {
        // A drive is there already.
        return Err(FsError::new(
            ErrorCode::AlreadyExists,
            "the path already exists",
        ));
    };
    create_dir_in(&open_folder(folder)?, name)
}

/// Creates the folder `name` in the opened folder, relative to its handle.
pub(super) fn create_dir_in(folder: &Folder, name: &str) -> Result<(), FsError> {
    anchor::create_dir_at(folder, name)
        .map_err(|e| FsError::from_io("failed to create the directory", &e))
}

/// Moves the entry at `from` to `to`
/// ([`cretlink_protocol::Operation::FsMove`]): a rename on one volume, never
/// a copy. With `overwrite` a file or a link under the destination's name is
/// replaced; a folder there never is.
///
/// Both folders are taken up before anything changes. The destination comes
/// first, as it always did: a name that is taken there is said before the
/// source is looked at.
pub(super) fn rename(from: &Side, to: &Side, overwrite: bool) -> Result<(), FsError> {
    let (into, new_name) = to.open()?;
    check_destination(&into, new_name, overwrite)?;
    let (out_of, name) = from.open()?;
    move_between(&out_of, name, &into, new_name, overwrite)
}

/// Moves the entry `name` of the opened folder `out_of` to the name
/// `new_name` in the opened folder `into`: what [`rename`] does once both
/// folders are taken up. For the tests that swap a folder's path in between.
#[cfg(test)]
pub(super) fn rename_between(
    out_of: &Folder,
    name: &str,
    into: &Folder,
    new_name: &str,
    overwrite: bool,
) -> Result<(), FsError> {
    check_destination(into, new_name, overwrite)?;
    move_between(out_of, name, into, new_name, overwrite)
}

/// Looks at what lies under the destination's name, for the answers that say
/// so plainly. The rename decides for itself whether the name is free.
fn check_destination(into: &Folder, new_name: &str, overwrite: bool) -> Result<(), FsError> {
    match anchor::open_at(into, new_name, TO_LOOK, SHARED, Disposition::Open, 0) {
        Ok(existing) => {
            if !overwrite {
                return Err(FsError::new(
                    ErrorCode::AlreadyExists,
                    "the destination already exists",
                ));
            }
            let kind = anchor::kind_of(&existing)
                .map_err(|e| FsError::from_io("failed to stat the destination", &e))?;
            if kind == Kind::Dir {
                return Err(FsError::wrong_kind("the destination is a directory"));
            }
            Ok(())
        }
        Err(error) if io_error_code(&error) == ErrorCode::PathNotFound => Ok(()),
        Err(error) => Err(FsError::from_io("failed to stat the destination", &error)),
    }
}

/// Opens the entry `name` in `out_of` and renames it, through its own
/// handle, to `new_name` in `into`.
fn move_between(
    out_of: &Folder,
    name: &str,
    into: &Folder,
    new_name: &str,
    overwrite: bool,
) -> Result<(), FsError> {
    let entry = anchor::open_at(out_of, name, TO_MOVE, SHARED, Disposition::Open, 0)
        .map_err(|error| not_found_or(&error, "failed to move the path"))?;
    anchor::rename_into(&entry, into, new_name, overwrite)
        .map_err(|e| FsError::from_io("failed to move the path", &e))
}

/// The folder that holds the entry at `path`, and the entry's name in it.
/// `None` for a path with no folder above it: the root of a drive.
fn split(path: &Path) -> Option<(&Path, &str)> {
    Some((path.parent()?, path.file_name()?.to_str()?))
}

/// Takes up the folder that holds an entry: a real folder that lies where the
/// request says.
pub(super) fn open_folder(path: &Path) -> Result<Folder, FsError> {
    let folder = Folder::open(path, Hold::Shared).map_err(|refusal| match refusal {
        anchor::Refusal::Elsewhere => FsError::new(ErrorCode::InvalidPath, anchor::LEADS_ELSEWHERE),
        anchor::Refusal::Io(error) => not_found_or(&error, "failed to open the folder of the path"),
    })?;
    match folder.kind() {
        Kind::Dir => Ok(folder),
        // A link in the place of the folder: the entry would be the one in
        // the place the link leads to, which is not the place that was asked
        // for.
        Kind::Link => Err(FsError::new(
            ErrorCode::InvalidPath,
            "the folder of the path is a link",
        )),
        // A file where the path goes on: to Windows there is no such path,
        // and that is what a request by name was always answered.
        Kind::File => Err(FsError::path_not_found()),
    }
}

/// What the entry `name` of the opened folder is, read without any right
/// that another program's use of the entry could refuse. `None` when it is
/// not there or cannot be looked at.
fn kind_at(folder: &Folder, name: &str) -> Option<Kind> {
    let entry = anchor::open_at(folder, name, TO_LOOK, SHARED, Disposition::Open, 0).ok()?;
    anchor::kind_of(&entry).ok()
}

/// What an opened entry is.
fn kind_of(entry: &File) -> Result<Kind, FsError> {
    anchor::kind_of(entry).map_err(|e| FsError::from_io("failed to stat the path", &e))
}

/// The failure for an entry or a folder that could not be opened: "the path
/// does not exist" when it is not there, as these operations always said it,
/// and else the system's reason with `context`.
fn not_found_or(error: &std::io::Error, context: &'static str) -> FsError {
    if io_error_code(error) == ErrorCode::PathNotFound {
        FsError::path_not_found()
    } else {
        FsError::from_io(context, error)
    }
}
