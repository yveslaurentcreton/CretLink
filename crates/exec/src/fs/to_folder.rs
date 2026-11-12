//! A file sent to a folder, on Windows (ST-M010-001, ADR-0012).
//!
//! The folder a file is sent to may belong to an account that is not
//! elevated, and a program of that account can swap the folder for a junction
//! between two requests of one transfer, or between two steps of one request.
//! A transfer that resolved `folder\name` again for every chunk, for the
//! commit and for the cleanup would then write, publish or delete in the
//! place the junction leads to.
//!
//! So every operation here opens the folder **once**, as a verified folder
//! (see the `anchor` module): it must be a real folder that lies where the
//! request says, and a link in its place is refused. Everything else happens
//! inside that opened folder: the part is opened by its one name relative to
//! the folder's handle, it is read, flushed and removed through its own
//! handle, and it is published by a rename that gives it a name in the
//! directory it lies in. No path is resolved a second time, so nothing that
//! happens to a path meanwhile changes where the bytes go.
//!
//! The part itself must be this transfer's own: a regular file with exactly
//! one name. A link under its name is opened as the link and refused, and a
//! second name of a file elsewhere (a hard link) is refused too — neither is
//! written to and neither is published.
//!
//! The functions block: they work on the file system, so the caller runs
//! them off the async runtime. A commit of a large file takes a while; it is
//! told when nobody waits for it any more — the session task is gone — and
//! then publishes nothing.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use cretlink_protocol::{B64Bytes, DigestAlgo, ErrorCode, OpResult, limits};
use sha2::{Digest, Sha256};
use windows::Wdk::Storage::FileSystem::FILE_NON_DIRECTORY_FILE;
use windows::Win32::Storage::FileSystem::{
    DELETE, FILE_APPEND_DATA, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_DATA,
};

use super::anchor::{self, Disposition, Folder, Hold, Kind};
use super::{FileStore, FsError, HASH_BUF_BYTES, io_error_code};

/// Others may read the part while it is written or verified, and nothing
/// more: nobody writes to it, renames it or removes it meanwhile.
const READERS_ONLY: u32 = FILE_SHARE_READ.0;

/// Every use by others stays allowed.
const SHARED: u32 = FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0;

/// The rights to append a chunk to the part and to read its length.
const TO_APPEND: u32 = FILE_APPEND_DATA.0 | FILE_READ_ATTRIBUTES.0;

/// The rights to verify the part, to flush it (which takes a right to write)
/// and to publish it (a rename takes the right to delete).
const TO_PUBLISH: u32 =
    FILE_READ_DATA.0 | FILE_WRITE_DATA.0 | FILE_APPEND_DATA.0 | FILE_READ_ATTRIBUTES.0 | DELETE.0;

/// The rights to remove an entry and to read what it is.
const TO_REMOVE: u32 = DELETE.0 | FILE_READ_ATTRIBUTES.0;

/// Where one file sent to a folder goes.
#[derive(Debug)]
pub(super) struct Place {
    /// The folder, as the request names it.
    pub(super) folder: PathBuf,
    /// The name the file gets in the folder.
    pub(super) name: String,
    /// The name of the transfer's part in the folder.
    pub(super) part_name: String,
    /// The part as the request's path names it: what the store remembers an
    /// unfinished transfer by.
    pub(super) part: PathBuf,
    /// The destination as the request names it.
    pub(super) asked: String,
}

/// What became of a part that was to be removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Removal {
    /// The part was there and is removed.
    Removed,
    /// There was no part: it was never written, or its folder is gone and
    /// took it along.
    Absent,
}

/// Appends one chunk to the part of a transfer ([`cretlink_protocol::Operation::FsWrite`]).
pub(super) fn write(
    store: &FileStore,
    place: &Place,
    offset: u64,
    data: &[u8],
) -> Result<OpResult, FsError> {
    let folder = open_folder(&place.folder)?;
    let existing = match open_part(&folder, &place.part_name, Disposition::Open, TO_APPEND) {
        Ok(part) => {
            require_own_part(&part)?;
            Some(part)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(part_error("failed to open the part file", &error)),
    };
    // The length is the opened part's own, not that of whatever a path
    // names.
    let current = match &existing {
        Some(part) => part_len(part)?,
        None => 0,
    };
    if offset != current {
        return Err(FsError::new(
            ErrorCode::WriteConflict,
            "the write offset does not match the current part length",
        ));
    }
    let new_len = current + data.len() as u64;
    if new_len > limits::MAX_TRANSFER_BYTES {
        return Err(FsError::new(
            ErrorCode::LimitExceeded,
            "the part exceeds the maximum transfer size",
        ));
    }
    // The part lies outside the staging area, so the store remembers it for
    // its cleanup before the part exists and before the first byte is
    // written. A chunk that is refused up to here has created nothing.
    store.remember_unfinished(&place.part)?;
    let mut part = match existing {
        Some(part) => part,
        None => create_part(store, &folder, place, TO_APPEND)?,
    };
    part.write_all(data)
        .map_err(|e| FsError::from_io("failed to write the part file", &e))?;
    Ok(OpResult::WriteAccepted { part_len: new_len })
}

/// Verifies the part of a transfer and publishes it under its name
/// ([`cretlink_protocol::Operation::FsCommit`]).
///
/// `abandoned` is asked while the part is read and once more before the file
/// gets its name: once it answers `true` nobody waits for this commit any
/// more, and nothing is published.
pub(super) fn commit(
    store: &FileStore,
    place: &Place,
    len: u64,
    digest: &[u8],
    overwrite: bool,
    abandoned: &dyn Fn() -> bool,
) -> Result<OpResult, FsError> {
    let folder = open_folder(&place.folder)?;
    let (mut part, created) =
        match open_part(&folder, &place.part_name, Disposition::Open, TO_PUBLISH) {
            Ok(part) => {
                require_own_part(&part)?;
                (part, false)
            }
            // An empty file is committed without a chunk: its part is made
            // now.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if len != 0 {
                    return Err(FsError::integrity(
                        "no staged bytes for the declared length",
                    ));
                }
                store.remember_unfinished(&place.part)?;
                (create_part(store, &folder, place, TO_PUBLISH)?, true)
            }
            Err(error) => return Err(part_error("failed to open the part file", &error)),
        };
    let wanted = Wanted {
        len,
        digest,
        overwrite,
    };
    match publish(&folder, &mut part, place, &wanted, abandoned) {
        Ok(committed) => {
            // The part is the file now: nothing is left for a cleanup.
            store.forget_unfinished(&place.part);
            Ok(committed)
        }
        Err(error) => {
            // A part of bytes that were sent stays for a repeated commit. An
            // empty one that this commit made holds nothing to resume: it is
            // removed again, so that a refused commit leaves neither a file
            // nor a part to remember.
            if created && anchor::delete(&part).is_ok() {
                store.forget_unfinished(&place.part);
            }
            Err(error)
        }
    }
}

/// Discards the part of a transfer ([`cretlink_protocol::Operation::FsAbort`]).
/// Idempotent: a part that is not there is the state that was asked for.
pub(super) fn discard(store: &FileStore, place: &Place) -> Result<OpResult, FsError> {
    // A part that could not be removed is still there, and stays remembered
    // for the cleanup.
    remove_part(&place.part)?;
    store.forget_unfinished(&place.part);
    Ok(OpResult::Ack {})
}

/// Removes the part at `part` from its folder: a file or a link under that
/// name, through its handle, and never a folder.
///
/// The folder is opened as a verified folder first. One that is no longer a
/// real folder at its place — it was swapped for a link — is refused
/// ([`ErrorCode::InvalidPath`]): the part does not lie where that path leads
/// now, and nothing is removed there.
pub(super) fn remove_part(part: &Path) -> Result<Removal, FsError> {
    let (Some(folder), Some(name)) = (part.parent(), part.file_name().and_then(|n| n.to_str()))
    else {
        return Err(FsError::wrong_kind(
            "the destination has no parent directory",
        ));
    };
    let folder = match open_folder(folder) {
        Ok(folder) => folder,
        // The folder is gone, and the part with it.
        Err(error) if error.code == ErrorCode::PathNotFound => return Ok(Removal::Absent),
        Err(error) => return Err(error),
    };
    let entry = match anchor::open_at(&folder, name, TO_REMOVE, SHARED, Disposition::Open, 0) {
        Ok(entry) => entry,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Removal::Absent);
        }
        Err(error) => return Err(FsError::from_io("failed to discard the part file", &error)),
    };
    let kind = anchor::kind_of(&entry)
        .map_err(|e| FsError::from_io("failed to stat the part file", &e))?;
    if kind == Kind::Dir {
        return Err(FsError::wrong_kind(
            "the part of this transfer is not a file",
        ));
    }
    anchor::delete(&entry).map_err(|e| FsError::from_io("failed to discard the part file", &e))?;
    Ok(Removal::Removed)
}

/// Opens the folder a file is sent to: a real folder that lies where the
/// request says.
fn open_folder(path: &Path) -> Result<Folder, FsError> {
    let folder = Folder::open(path, Hold::Shared).map_err(|refusal| match refusal {
        anchor::Refusal::Elsewhere => FsError::new(ErrorCode::InvalidPath, anchor::LEADS_ELSEWHERE),
        anchor::Refusal::Io(error) => {
            FsError::from_io("failed to open the folder of the destination", &error)
        }
    })?;
    match folder.kind() {
        Kind::Dir => Ok(folder),
        // A link in the place of the folder: the file would go where the
        // link leads, which is not the place that was asked for.
        Kind::Link => Err(FsError::new(
            ErrorCode::InvalidPath,
            "the folder of the destination is a link",
        )),
        Kind::File => Err(FsError::wrong_kind(
            "the folder of the destination is not a folder",
        )),
    }
}

/// Opens or creates the part `name` in the opened folder, as a file and
/// never as a folder, shared with readers only.
fn open_part(
    folder: &Folder,
    name: &str,
    disposition: Disposition,
    access: u32,
) -> std::io::Result<File> {
    anchor::open_at(
        folder,
        name,
        access,
        READERS_ONLY,
        disposition,
        FILE_NON_DIRECTORY_FILE.0,
    )
}

/// Creates the part of a transfer that the store has just remembered. If it
/// cannot be created, there is nothing to clean up and the store forgets it
/// again — unless the name turned out to be taken: what lies under the name
/// of this transfer's part is then left to the cleanup.
fn create_part(
    store: &FileStore,
    folder: &Folder,
    place: &Place,
    access: u32,
) -> Result<File, FsError> {
    open_part(folder, &place.part_name, Disposition::Create, access).map_err(|error| {
        if io_error_code(&error) != ErrorCode::AlreadyExists {
            store.forget_unfinished(&place.part);
        }
        part_error("failed to create the part file", &error)
    })
}

/// The error for a part that could not be opened. A folder under the part's
/// name is said as what it is.
fn part_error(context: &'static str, error: &std::io::Error) -> FsError {
    if io_error_code(error) == ErrorCode::WrongPathKind {
        not_a_part()
    } else {
        FsError::from_io(context, error)
    }
}

/// The refusal of what lies under a part's name without being a part.
fn not_a_part() -> FsError {
    FsError::wrong_kind("the part of this transfer is not a regular file")
}

/// Requires that an opened part is this transfer's own: a regular file with
/// exactly one name. A link was put there by someone else, and so was a
/// second name of a file that lies elsewhere; neither is written to and
/// neither is published.
fn require_own_part(part: &File) -> Result<(), FsError> {
    let stat = |e: std::io::Error| FsError::from_io("failed to stat the part file", &e);
    if anchor::kind_of(part).map_err(stat)? != Kind::File {
        return Err(not_a_part());
    }
    if anchor::link_count(part).map_err(stat)? != 1 {
        return Err(not_a_part());
    }
    Ok(())
}

/// The length of an opened part.
fn part_len(part: &File) -> Result<u64, FsError> {
    part.metadata()
        .map(|facts| facts.len())
        .map_err(|e| FsError::from_io("failed to stat the part file", &e))
}

/// What a commit declares about the file it publishes.
struct Wanted<'a> {
    /// The length of the file.
    len: u64,
    /// Its SHA-256.
    digest: &'a [u8],
    /// Whether a file of the same name is replaced.
    overwrite: bool,
}

/// Verifies the opened part against the declared length and digest, flushes
/// it, and gives it its name in the folder it lies in.
fn publish(
    folder: &Folder,
    part: &mut File,
    place: &Place,
    wanted: &Wanted<'_>,
    abandoned: &dyn Fn() -> bool,
) -> Result<OpResult, FsError> {
    let Wanted {
        len,
        digest,
        overwrite,
    } = *wanted;
    // Verify length, then digest, before anything is published. Both are
    // read through the handle that is published afterwards, and nobody
    // writes to the part while that handle is open.
    if part_len(part)? != len {
        return Err(FsError::integrity("the staged length does not match"));
    }
    if digest.len() != limits::DIGEST_LEN {
        return Err(FsError::integrity("the declared digest is malformed"));
    }
    let hashed = sha256_of(part, abandoned)
        .map_err(|e| FsError::from_io("failed to read the part file", &e))?;
    let Some((computed, hashed)) = hashed else {
        return Err(interrupted());
    };
    if hashed != len || computed.as_slice() != digest {
        return Err(FsError::integrity("the staged digest does not match"));
    }
    // Flush the verified bytes to disk before the rename publishes them.
    part.sync_all()
        .map_err(|e| FsError::from_io("failed to flush the part file", &e))?;
    if abandoned() {
        return Err(interrupted());
    }

    // What lies under the file's name, for the answers that say so plainly.
    // The rename below decides for itself whether the name is free.
    let replaced = match anchor::open_at(
        folder,
        &place.name,
        FILE_READ_ATTRIBUTES.0,
        SHARED,
        Disposition::Open,
        0,
    ) {
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
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(FsError::from_io("failed to stat the destination", &error)),
    };

    if let Err(error) = anchor::rename_in_folder(part, &place.name, overwrite) {
        // Windows refuses to replace a file that another program holds open
        // with "access denied". It is said as what it is, so that the
        // technician closes the program instead of looking for a missing
        // permission.
        if io_error_code(&error) == ErrorCode::AccessDenied && is_held(folder, &place.name) {
            return Err(FsError::new(
                ErrorCode::FileInUse,
                "the destination is in use by another program",
            ));
        }
        return Err(FsError::from_io("failed to publish the file", &error));
    }

    Ok(OpResult::Committed {
        canonical_path: committed_path(folder, place),
        len,
        algo: DigestAlgo::Sha256,
        digest: B64Bytes::new(computed.to_vec()),
        replaced,
    })
}

/// Whether another program holds the entry `name` of the folder open in a
/// way that excludes its removal, and with it its replacement.
fn is_held(folder: &Folder, name: &str) -> bool {
    match anchor::open_at(folder, name, DELETE.0, SHARED, Disposition::Open, 0) {
        Ok(_) => false,
        Err(error) => io_error_code(&error) == ErrorCode::FileInUse,
    }
}

/// The path a committed file is reported under: where its folder really
/// lies, and its name. A location that the wire would not accept as a path
/// — a part of it has a name that the path rules refuse — is not reported;
/// the destination as it was asked stands in for it.
fn committed_path(folder: &Folder, place: &Place) -> String {
    let real = anchor::plain(&folder.path().join(&place.name));
    if cretlink_protocol::validate_remote_absolute_path("canonical_path", &real).is_ok() {
        real
    } else {
        place.asked.clone()
    }
}

/// The answer of a commit that nobody waits for any more. Nobody reads it;
/// it only ends the work.
fn interrupted() -> FsError {
    FsError::new(ErrorCode::Internal, "the transfer was interrupted")
}

/// Hashes an opened file from its start with SHA-256, returning the digest
/// and the number of bytes hashed, or `None` once `abandoned` answers `true`.
fn sha256_of(
    file: &mut File,
    abandoned: &dyn Fn() -> bool,
) -> std::io::Result<Option<([u8; 32], u64)>> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_BUF_BYTES];
    let mut total: u64 = 0;
    loop {
        if abandoned() {
            return Ok(None);
        }
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&hasher.finalize());
    Ok(Some((digest, total)))
}
