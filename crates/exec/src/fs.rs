//! Agent-side file operations and the transfer commit engine (ADR-0012,
//! ST-M003-004/005).
//!
//! [`FileStore`] services the file-operation half of the wire schema against a
//! real filesystem at the agent's privilege. It is the counterpart to the
//! [`Broker`](crate::Broker): where the broker runs shell jobs, the file store
//! reads directories and files, stages and commits transfers, and prunes the
//! session's staging area. Every operation is a bounded, stateless
//! request/response step — the console composes them into a transfer.
//!
//! # Integrity by construction (REQ-N-007)
//!
//! Bytes reach a final filename by exactly one path: [`Operation::FsWrite`]
//! appends to a session-namespaced **part** file, and [`Operation::FsCommit`]
//! flushes it, verifies its length **and** its SHA-256 against the values the
//! console declared, and only then atomically renames it into place on the same
//! volume. There is no wire operation that writes an unverified byte to a final
//! name, so a published file that failed verification is unrepresentable. A
//! commit that fails verification leaves the part untouched for a resume.
//!
//! # Two path authorities (ADR-0012)
//!
//! [`RemotePath::Absolute`] names a full target path an elevated user may
//! reach; [`RemotePath::Staging`] names one filename component the agent
//! resolves under this session's own staging root. Both are structurally
//! validated on the wire (`..`, UNC and device namespaces, reserved device
//! names, and alternate-data-stream colons are rejected by
//! [`cretlink_protocol::Message::validate`] before an operation ever reaches
//! here); the store adds a resolution-time guard that a staging reference can
//! never escape its root.
//!
//! # A file sent to a folder (ST-M010-001)
//!
//! A transfer whose destination is an absolute path keeps its part **beside
//! the destination**, in the folder the file is meant for. The part is
//! therefore created with that folder's own permissions and on that folder's
//! own drive, and the verified commit is a rename inside one folder: it works
//! on every drive the agent can write, and the file appears under its name
//! only once its length and SHA-256 are verified. Such a file is the
//! customer's to keep: it is not recorded in the staging manifest and no
//! cleanup removes it. What the session does clean up is the **unfinished
//! part** of such a transfer: the store remembers the parts it began and has
//! neither committed nor discarded, and removes them with the staging area.
//! A part is opened without following a link, so a link planted under a part's
//! name is never written through.
//!
//! # Drives and folder removal (ST-M010-001)
//!
//! [`Operation::FsDrives`] lists the drives (see the `drives` module), and
//! [`Operation::RemoveTree`] removes a folder with everything in it, one
//! bounded step at a time, without ever leaving the folder (see the `tree`
//! module).
//!
//! # Honest failures (REQ-N-008, REQ-N-015)
//!
//! Every failure is a secret-free [`FsError`] carrying a wire [`ErrorCode`] and
//! a fixed message — never remote file contents, never the offending path.
//! Cleanup reports honest per-artifact outcomes rather than a blanket success.
//! A folder removal names the entries it could not remove: those paths are its
//! result, bounded in number and length.

mod drives;
mod manifest;
mod tree;

#[cfg(test)]
mod tests;

use std::collections::{BTreeSet, BinaryHeap};
use std::io::SeekFrom;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use cretlink_protocol::{
    B64Bytes, CleanupFailure, CleanupTrigger, DigestAlgo, ErrorCode, FileEntry, FileKind, OpResult,
    Operation, RemotePath, SessionId, StagedEntry, limits,
};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use manifest::{MANIFEST_FILE, Manifest};

/// The subdirectory of a staging root that holds in-progress part files, kept
/// out of directory listings and swept by every cleanup trigger.
const PARTS_DIR: &str = ".parts";

/// The read buffer used when hashing a file for a digest or commit.
const HASH_BUF_BYTES: usize = 128 * 1024;

/// The most unfinished transfers to a folder one session's store remembers for
/// its cleanup. A transfer that would begin one more is refused
/// ([`ErrorCode::StorageExhausted`]) rather than left untracked, so the list
/// stays bounded and every part the session leaves is one it knows of
/// (REQ-N-006, REQ-F-019).
const MAX_UNFINISHED_TRANSFERS: usize = 1024;

/// A secret-free file-operation failure.
///
/// It carries the wire [`ErrorCode`] the console should receive and a fixed,
/// path-free message. The message never echoes remote file contents or the
/// offending path (REQ-N-015); the code is what the console acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsError {
    /// The machine-readable category.
    pub code: ErrorCode,
    /// A fixed, secret-free description.
    pub message: &'static str,
}

impl FsError {
    /// Builds a failure from a code and a fixed message.
    #[must_use]
    const fn new(code: ErrorCode, message: &'static str) -> Self {
        Self { code, message }
    }

    fn path_not_found() -> Self {
        Self::new(ErrorCode::PathNotFound, "the path does not exist")
    }

    fn wrong_kind(message: &'static str) -> Self {
        Self::new(ErrorCode::WrongPathKind, message)
    }

    fn integrity(message: &'static str) -> Self {
        Self::new(ErrorCode::IntegrityMismatch, message)
    }

    fn outside_staging() -> Self {
        Self::new(
            ErrorCode::OutsideStagingRoot,
            "the resolved path escaped the staging root",
        )
    }

    fn storage_exhausted() -> Self {
        Self::new(
            ErrorCode::StorageExhausted,
            "the operation would exceed the staging quota",
        )
    }

    fn internal() -> Self {
        Self::new(ErrorCode::Internal, "the file operation failed")
    }

    /// Maps a raw I/O error onto a wire [`ErrorCode`], preferring the precise
    /// Windows status code and falling back to the portable [`std::io::ErrorKind`]
    /// so the crate maps sensibly on every platform it is tested on.
    fn from_io(context: &'static str, err: &std::io::Error) -> Self {
        let code = io_error_code(err);
        Self::new(code, context)
    }
}

/// Classifies a raw I/O error into the wire vocabulary.
fn io_error_code(err: &std::io::Error) -> ErrorCode {
    // Windows surfaces the most precise signal in its own error code. The
    // numbers are Windows error codes: on another system the same number means
    // something else (17 is "file exists" and 39 "directory not empty" on
    // Linux), so they are read on Windows only.
    #[cfg(windows)]
    if let Some(raw) = err.raw_os_error() {
        match raw {
            2 | 3 => return ErrorCode::PathNotFound, // FILE/PATH_NOT_FOUND
            5 => return ErrorCode::AccessDenied,     // ACCESS_DENIED
            32 | 33 => return ErrorCode::FileInUse,  // SHARING/LOCK_VIOLATION
            17 => return ErrorCode::Unsupported,     // NOT_SAME_DEVICE
            80 | 183 => return ErrorCode::AlreadyExists, // FILE_EXISTS/ALREADY_EXISTS
            145 => return ErrorCode::DirNotEmpty,    // DIR_NOT_EMPTY
            267 => return ErrorCode::WrongPathKind,  // DIRECTORY (invalid dir op)
            _ => {}
        }
    }
    match err.kind() {
        std::io::ErrorKind::NotFound => ErrorCode::PathNotFound,
        std::io::ErrorKind::PermissionDenied => ErrorCode::AccessDenied,
        std::io::ErrorKind::AlreadyExists => ErrorCode::AlreadyExists,
        std::io::ErrorKind::DirectoryNotEmpty => ErrorCode::DirNotEmpty,
        std::io::ErrorKind::NotADirectory | std::io::ErrorKind::IsADirectory => {
            ErrorCode::WrongPathKind
        }
        std::io::ErrorKind::CrossesDevices => ErrorCode::Unsupported,
        // The disk is full, or this account's share of it is used up: said as
        // such, so that the caller frees space instead of trying again
        // (ST-M010-001, REQ-N-015).
        std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded => {
            ErrorCode::StorageExhausted
        }
        // Held by another program in a way that excludes this operation.
        std::io::ErrorKind::ResourceBusy | std::io::ErrorKind::ExecutableFileBusy => {
            ErrorCode::FileInUse
        }
        _ => ErrorCode::Internal,
    }
}

/// Makes `text` fit one line of at most `max_bytes` bytes, for a text field of
/// a result that the wire requires to be bounded and on one line.
///
/// A control character or a line separator — which a file name on some systems
/// may hold — becomes the replacement character, so the text cannot break a
/// list or a log into forged lines. A text that is too long is cut on a
/// character boundary and ends in `…`, so it is never taken for a whole one.
fn one_line(text: &str, max_bytes: usize) -> String {
    const CUT: char = '…';
    debug_assert!(max_bytes >= CUT.len_utf8());
    let mut line = String::with_capacity(text.len().min(max_bytes));
    for character in text.chars() {
        // The same rule as the protocol's own one-line check.
        let breaks_the_line =
            character.is_control() || character == '\u{2028}' || character == '\u{2029}';
        line.push(if breaks_the_line {
            char::REPLACEMENT_CHARACTER
        } else {
            character
        });
    }
    if line.len() <= max_bytes {
        return line;
    }
    let mut end = max_bytes.saturating_sub(CUT.len_utf8());
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    line.truncate(end);
    line.push(CUT);
    line
}

/// Serves the file-operation schema for one session against a real filesystem.
///
/// A store is bound to one [`SessionId`] and owns a staging root beneath a base
/// directory. Absolute-path operations touch the target directly; staging
/// operations resolve under `base/<session>`. No operation depends on state in
/// memory: the part files on disk and the persistent manifest are the only
/// state an operation reads, so the store is correct across an agent restart
/// and holds no lock a crash could strand.
///
/// The one thing the store remembers is which transfers to a folder it began
/// and has not finished, so that [`FileStore::cleanup`] can remove their parts
/// when the session ends (ST-M010-001, REQ-F-019). It is bookkeeping for the
/// cleanup only, shared by the clones of one store, and gone with the process:
/// an agent that is killed leaves such a part, visible, beside its destination.
#[derive(Debug, Clone)]
pub struct FileStore {
    session: SessionId,
    staging_root: PathBuf,
    /// The parts, beside their destinations, of the transfers to a folder that
    /// this store began and has neither committed nor discarded.
    unfinished: Arc<Mutex<BTreeSet<PathBuf>>>,
}

/// The honest outcome of a startup crash-recovery sweep across every session
/// staging tree beneath the base (ST-M003-005 AC5b, REQ-F-019, REQ-N-008).
///
/// A previous agent run may have exited — cleanly or by a crash — before its
/// `OnShutdown` artifacts were removed, or with a removal that failed. The next
/// start sweeps the whole base so nothing outlives the run that created it, and
/// reports what it did (and what it could not do, to be retried again).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StartupSweepReport {
    /// Session staging trees inspected.
    pub sessions_scanned: u32,
    /// Session directories removed once nothing but emptied bookkeeping remained.
    pub sessions_removed: u32,
    /// Artifacts and orphaned part files removed.
    pub artifacts_removed: u32,
    /// Artifacts that could not be removed; their manifest records are kept so a
    /// later start retries them (never reported as success — REQ-N-008).
    pub failed: Vec<CleanupFailure>,
}

impl StartupSweepReport {
    /// Whether the sweep removed everything it selected (no residual failures).
    #[must_use]
    pub fn ok(&self) -> bool {
        self.failed.is_empty()
    }
}

/// One session's staging residue as seen by a read-only census: the manifest's
/// staged records and a count of orphaned part files, with the owning session
/// carried so a recovery report can name exactly where a resource belongs
/// (ST-M006-003, REQ-S-017).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStaging {
    /// The session that owns this staging tree.
    pub session: SessionId,
    /// The manifest's staged records (kept exactly as recorded; never mutated).
    pub staged: Vec<StagedEntry>,
    /// Orphaned in-progress part files still present under `.parts`.
    pub orphaned_parts: u32,
}

/// A read-only census of staged residue across every session tree beneath a
/// base — the inspect-side counterpart of [`FileStore::sweep_all_sessions`]. It
/// removes nothing: it reports what a sweep *would* reclaim so a recovery verb
/// can show a customer their residue before anything is touched (ST-M006-003,
/// REQ-F-019). A base that does not exist yet is a clean, empty census.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StagingCensus {
    /// Sessions that hold at least one staged record or orphaned part; a session
    /// whose tree is already empty is omitted so a clean base reads as empty
    /// rather than a row of zeros.
    pub sessions: Vec<SessionStaging>,
}

impl StagingCensus {
    /// The total number of staged records across every session.
    #[must_use]
    pub fn staged_count(&self) -> usize {
        self.sessions.iter().map(|s| s.staged.len()).sum()
    }

    /// The total number of orphaned part files across every session.
    #[must_use]
    pub fn orphaned_parts(&self) -> u32 {
        self.sessions
            .iter()
            .fold(0u32, |acc, s| acc.saturating_add(s.orphaned_parts))
    }

    /// Whether the census found no staged residue at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

impl FileStore {
    /// Binds a store to `session`, rooting its staging area under the platform
    /// default base (`%ProgramData%\CretLink\staging` on Windows, a temp
    /// directory elsewhere). The staging directory itself is created lazily on
    /// first use, so a session that only touches absolute paths never creates it.
    #[must_use]
    pub fn for_session(session: SessionId) -> Self {
        Self::with_base(session, default_staging_base())
    }

    /// Binds a store to `session` with an explicit staging base directory. Used
    /// by the session layer to place staging where policy dictates, and by tests
    /// to sandbox it.
    #[must_use]
    pub fn with_base(session: SessionId, base: impl Into<PathBuf>) -> Self {
        let staging_root = base.into().join(session.to_string());
        Self {
            session,
            staging_root,
            unfinished: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    /// The absolute staging root for this session.
    #[must_use]
    pub fn staging_root(&self) -> &Path {
        &self.staging_root
    }

    /// Performs one file operation, returning its [`OpResult`] on success or a
    /// secret-free [`FsError`] on failure.
    ///
    /// The match is exhaustive over [`Operation`] on purpose: the non-file
    /// variants are never routed here (the session layer dispatches `Exec` and
    /// friends to the [`Broker`](crate::Broker)), and listing them explicitly
    /// means adding any future operation breaks this build until a deliberate
    /// decision is made.
    pub async fn perform(&self, op: Operation) -> Result<OpResult, FsError> {
        match op {
            Operation::FsStat { path } => self.stat(&path).await,
            Operation::FsList {
                path,
                continue_after,
            } => self.list(&path, continue_after.as_deref()).await,
            Operation::CreateDir { path } => self.create_dir(&path).await,
            Operation::FsRead { path, offset, len } => self.read(&path, offset, len).await,
            Operation::FsWrite { dest, offset, data } => {
                self.write(&dest, offset, data.into_inner()).await
            }
            Operation::FsCommit {
                dest,
                len,
                algo,
                digest,
                overwrite,
                source,
                cleanup,
            } => {
                self.commit(CommitArgs {
                    dest,
                    len,
                    algo,
                    digest,
                    overwrite,
                    source,
                    cleanup,
                })
                .await
            }
            Operation::FsAbort { dest } => self.abort(&dest).await,
            Operation::FsDigest { path, prefix_len } => self.digest(&path, prefix_len).await,
            Operation::FsMove {
                from,
                to,
                overwrite,
            } => self.rename(&from, &to, overwrite).await,
            Operation::FsDelete { path } => self.delete(&path).await,
            Operation::RemoveDir { path } => self.remove_dir(&path).await,
            Operation::ListStaged {} => self.list_staged().await,
            Operation::StagingCleanup { trigger } => self.cleanup(trigger).await,
            Operation::FsDrives {} => self.drives().await,
            Operation::RemoveTree { path, stop } => self.remove_tree(&path, stop).await,
            // Not file operations: the session layer routes these to the broker
            // and never here. Answering fail-closed keeps a misroute honest.
            Operation::SystemInfo {}
            | Operation::Exec { .. }
            | Operation::WriteStdin { .. }
            | Operation::ListJobs {}
            | Operation::TermOpen { .. }
            | Operation::TermResize { .. }
            | Operation::TermClose { .. }
            | Operation::DesktopCapture { .. }
            | Operation::DesktopPointer { .. }
            | Operation::DesktopScroll { .. }
            | Operation::DesktopKey { .. }
            | Operation::DesktopType { .. }
            | Operation::DesktopHotkey { .. }
            | Operation::DesktopWait { .. }
            | Operation::ClipboardGet {}
            | Operation::ClipboardSet { .. } => {
                Err(FsError::new(ErrorCode::Internal, "not a file operation"))
            }
        }
    }

    // --- inspection -------------------------------------------------------

    async fn stat(&self, path: &RemotePath) -> Result<OpResult, FsError> {
        let resolved = self.resolve(path)?;
        let entry = match tokio::fs::symlink_metadata(&resolved).await {
            Ok(meta) => Some(file_entry(entry_name(path, &resolved), &meta)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(FsError::from_io("failed to stat the path", &e)),
        };
        // Probe for a pending part so a resume can size what is already present.
        let partial_len = self.part_len(path).await;
        Ok(OpResult::PathStat { entry, partial_len })
    }

    async fn list(
        &self,
        path: &RemotePath,
        continue_after: Option<&str>,
    ) -> Result<OpResult, FsError> {
        let dir = self.resolve(path)?;
        match tokio::fs::symlink_metadata(&dir).await {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(FsError::wrong_kind("the path is not a directory")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(FsError::path_not_found());
            }
            Err(e) => return Err(FsError::from_io("failed to open the directory", &e)),
        }
        let hide_internal = dir == self.staging_root;
        let (entries, end) = list_dir(&dir, continue_after, hide_internal).await?;
        Ok(OpResult::DirEntries { entries, end })
    }

    /// Lists the drives of this computer (ST-M010-001). The system is asked
    /// off the async runtime: a drive that is slow to answer must not hold up
    /// the runtime's other tasks.
    async fn drives(&self) -> Result<OpResult, FsError> {
        let drives = tokio::task::spawn_blocking(drives::list)
            .await
            .map_err(|_| FsError::internal())?;
        Ok(OpResult::Drives { drives })
    }

    async fn read(&self, path: &RemotePath, offset: u64, len: u32) -> Result<OpResult, FsError> {
        let resolved = self.resolve(path)?;
        let mut file = tokio::fs::File::open(&resolved)
            .await
            .map_err(|e| FsError::from_io("failed to open the file", &e))?;
        let meta = file
            .metadata()
            .await
            .map_err(|e| FsError::from_io("failed to stat the file", &e))?;
        if meta.is_dir() {
            return Err(FsError::wrong_kind("the path is a directory"));
        }
        let file_len = meta.len();
        if offset >= file_len {
            return Ok(OpResult::FileData {
                data: B64Bytes::new(Vec::new()),
                eof: true,
                file_len,
            });
        }
        file.seek(SeekFrom::Start(offset))
            .await
            .map_err(|e| FsError::from_io("failed to seek the file", &e))?;
        let remaining = file_len - offset;
        let want = remaining.min(u64::from(len)) as usize;
        let mut buf = vec![0u8; want];
        let mut filled = 0;
        while filled < want {
            let n = file
                .read(&mut buf[filled..])
                .await
                .map_err(|e| FsError::from_io("failed to read the file", &e))?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        buf.truncate(filled);
        let eof = offset + filled as u64 >= file_len;
        Ok(OpResult::FileData {
            data: B64Bytes::new(buf),
            eof,
            file_len,
        })
    }

    async fn digest(
        &self,
        path: &RemotePath,
        prefix_len: Option<u64>,
    ) -> Result<OpResult, FsError> {
        let resolved = self.resolve(path)?;
        match tokio::fs::symlink_metadata(&resolved).await {
            Ok(meta) if meta.is_dir() => {
                return Err(FsError::wrong_kind("the path is a directory"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(FsError::path_not_found());
            }
            Err(e) => return Err(FsError::from_io("failed to stat the file", &e)),
        }
        let (digest, hashed) = sha256_file(&resolved, prefix_len)
            .await
            .map_err(|e| FsError::from_io("failed to read the file", &e))?;
        if let Some(want) = prefix_len
            && hashed < want
        {
            // The file is shorter than the requested prefix, so the resume
            // verification cannot be satisfied.
            return Err(FsError::integrity(
                "the file is shorter than the requested prefix",
            ));
        }
        Ok(OpResult::FileDigest {
            algo: DigestAlgo::Sha256,
            digest: B64Bytes::new(digest.to_vec()),
            len: prefix_len.unwrap_or(hashed),
        })
    }

    // --- transfer ---------------------------------------------------------

    async fn write(
        &self,
        dest: &RemotePath,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<OpResult, FsError> {
        let part = self.part_path(dest)?;
        let to_folder = matches!(dest, RemotePath::Absolute { .. });
        if !to_folder {
            self.ensure_staging_dirs().await?;
        }
        // The part is a regular file this store wrote, or it does not exist
        // yet. Anything else under its name — a link, a folder — was put there
        // by someone else and is never written to.
        let current = match tokio::fs::symlink_metadata(&part).await {
            Ok(meta) if meta.file_type().is_file() => meta.len(),
            Ok(_) => {
                return Err(FsError::wrong_kind(
                    "the part of this transfer is not a regular file",
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(FsError::from_io("failed to stat the part file", &e)),
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
        if to_folder {
            // The part lies outside the staging area, so the store remembers
            // it for its cleanup before the first byte is written.
            self.remember_unfinished(&part)?;
        } else {
            self.check_staging_quota(new_len, None).await?;
        }
        if let Err(error) = append_to_part(&part, &data).await {
            // A part that was never created is nothing to clean up.
            if to_folder && tokio::fs::symlink_metadata(&part).await.is_err() {
                self.forget_unfinished(&part);
            }
            return Err(error);
        }
        Ok(OpResult::WriteAccepted { part_len: new_len })
    }

    async fn commit(&self, args: CommitArgs) -> Result<OpResult, FsError> {
        let CommitArgs {
            dest,
            len,
            algo,
            digest,
            overwrite,
            source,
            cleanup,
        } = args;
        // Only SHA-256 exists today; reject anything else rather than assume.
        let DigestAlgo::Sha256 = algo;
        let is_staging = matches!(dest, RemotePath::Staging { .. });
        // A staging commit must record where the artifact came from (REQ-S-017).
        let source = if is_staging {
            match source {
                Some(s) => s,
                None => {
                    return Err(FsError::new(
                        ErrorCode::Unsupported,
                        "a staging commit requires a source label",
                    ));
                }
            }
        } else {
            source.unwrap_or_default()
        };

        let final_path = self.resolve(&dest)?;
        let part = self.part_path(&dest)?;
        if is_staging {
            self.ensure_staging_dirs().await?;
        }

        // Materialise a zero-length part if the caller committed without writing.
        let part_len = match tokio::fs::symlink_metadata(&part).await {
            Ok(meta) if meta.file_type().is_file() => meta.len(),
            // Never publish what is not a regular file this store wrote.
            Ok(_) => {
                return Err(FsError::wrong_kind(
                    "the part of this transfer is not a regular file",
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if len != 0 {
                    return Err(FsError::integrity(
                        "no staged bytes for the declared length",
                    ));
                }
                if !is_staging {
                    self.remember_unfinished(&part)?;
                }
                tokio::fs::File::create(&part)
                    .await
                    .map_err(|e| FsError::from_io("failed to create the part file", &e))?;
                0
            }
            Err(e) => return Err(FsError::from_io("failed to stat the part file", &e)),
        };

        // Verify length, then digest, before anything is published.
        if part_len != len {
            return Err(FsError::integrity("the staged length does not match"));
        }
        if digest.as_bytes().len() != limits::DIGEST_LEN {
            return Err(FsError::integrity("the declared digest is malformed"));
        }
        let (computed, hashed) = sha256_file(&part, None)
            .await
            .map_err(|e| FsError::from_io("failed to read the part file", &e))?;
        if hashed != len || computed.as_slice() != digest.as_bytes() {
            return Err(FsError::integrity("the staged digest does not match"));
        }

        // Flush the verified bytes to disk before the rename publishes them. The
        // handle is opened for writing (without truncation) because a durable
        // flush needs write access on Windows.
        {
            let file = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&part)
                .await
                .map_err(|e| FsError::from_io("failed to open the part file", &e))?;
            file.sync_all()
                .await
                .map_err(|e| FsError::from_io("failed to flush the part file", &e))?;
        }

        // Resolve the overwrite decision against the final path.
        let replaced = match tokio::fs::symlink_metadata(&final_path).await {
            Ok(meta) => {
                if !overwrite {
                    return Err(FsError::new(
                        ErrorCode::AlreadyExists,
                        "the destination already exists",
                    ));
                }
                if meta.is_dir() {
                    return Err(FsError::wrong_kind("the destination is a directory"));
                }
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(FsError::from_io("failed to stat the destination", &e)),
        };

        let canonical_path = final_path.to_string_lossy().into_owned();

        if is_staging {
            // Record the artifact before publishing it, so a crash can leave a
            // recorded-but-absent entry (harmless: cleanup treats it as gone)
            // but never a published-but-untracked one.
            self.check_staging_quota(len, Some(&canonical_path)).await?;
            let mut manifest = self.load_manifest().await?;
            if manifest.len() >= limits::MAX_DIR_PAGE
                && !manifest
                    .entries()
                    .iter()
                    .any(|e| e.canonical_path == canonical_path)
            {
                return Err(FsError::storage_exhausted());
            }
            manifest.insert(StagedEntry {
                canonical_path: canonical_path.clone(),
                source,
                len,
                algo: DigestAlgo::Sha256,
                digest: B64Bytes::new(computed.to_vec()),
                cleanup,
                created_ms: now_ms(),
            });
            self.store_manifest(&manifest).await?;

            if let Err(e) = tokio::fs::rename(&part, &final_path).await {
                // Publishing failed; retract the record we just wrote.
                let mut manifest = self.load_manifest().await.unwrap_or_default();
                if manifest.remove(&canonical_path) {
                    let _ = self.store_manifest(&manifest).await;
                }
                return Err(FsError::from_io("failed to publish the file", &e));
            }
        } else {
            tokio::fs::rename(&part, &final_path)
                .await
                .map_err(|e| FsError::from_io("failed to publish the file", &e))?;
            // The part is the file now: nothing is left for a cleanup.
            self.forget_unfinished(&part);
        }

        Ok(OpResult::Committed {
            canonical_path,
            len,
            algo: DigestAlgo::Sha256,
            digest: B64Bytes::new(computed.to_vec()),
            replaced,
        })
    }

    async fn abort(&self, dest: &RemotePath) -> Result<OpResult, FsError> {
        let part = self.part_path(dest)?;
        match tokio::fs::remove_file(&part).await {
            Ok(()) => {}
            // Idempotent: the desired state (no part) is already reached.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            // The part is still there, and stays remembered for the cleanup.
            Err(e) => return Err(FsError::from_io("failed to discard the part file", &e)),
        }
        self.forget_unfinished(&part);
        Ok(OpResult::Ack {})
    }

    // --- mutation ---------------------------------------------------------

    async fn create_dir(&self, path: &RemotePath) -> Result<OpResult, FsError> {
        if matches!(path, RemotePath::Staging { .. }) {
            self.ensure_staging_dirs().await?;
        }
        let resolved = self.resolve(path)?;
        match tokio::fs::create_dir(&resolved).await {
            Ok(()) => Ok(OpResult::Ack {}),
            Err(e) => Err(FsError::from_io("failed to create the directory", &e)),
        }
    }

    async fn rename(
        &self,
        from: &RemotePath,
        to: &RemotePath,
        overwrite: bool,
    ) -> Result<OpResult, FsError> {
        let from_path = self.resolve(from)?;
        let to_path = self.resolve(to)?;
        if matches!(to, RemotePath::Staging { .. }) {
            self.ensure_staging_dirs().await?;
        }
        // An atomic rename is same-volume only; a copy is composed console-side.
        if !same_volume(&from_path, &to_path) {
            return Err(FsError::new(
                ErrorCode::Unsupported,
                "a cross-volume move is not supported",
            ));
        }
        match tokio::fs::symlink_metadata(&to_path).await {
            Ok(meta) => {
                if !overwrite {
                    return Err(FsError::new(
                        ErrorCode::AlreadyExists,
                        "the destination already exists",
                    ));
                }
                if meta.is_dir() {
                    return Err(FsError::wrong_kind("the destination is a directory"));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(FsError::from_io("failed to stat the destination", &e)),
        }
        tokio::fs::rename(&from_path, &to_path)
            .await
            .map_err(|e| FsError::from_io("failed to move the path", &e))?;
        Ok(OpResult::Ack {})
    }

    async fn delete(&self, path: &RemotePath) -> Result<OpResult, FsError> {
        let resolved = self.resolve(path)?;
        let meta = match tokio::fs::symlink_metadata(&resolved).await {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(FsError::path_not_found());
            }
            Err(e) => return Err(FsError::from_io("failed to stat the path", &e)),
        };
        let ft = meta.file_type();
        if ft.is_dir() && !ft.is_symlink() {
            return Err(FsError::wrong_kind("the path is a directory"));
        }
        // A reparse point is removed, not followed: try file removal first, then
        // directory removal for a directory symlink/junction.
        let removed = match tokio::fs::remove_file(&resolved).await {
            Ok(()) => Ok(()),
            Err(_) if ft.is_symlink() => tokio::fs::remove_dir(&resolved).await,
            Err(e) => Err(e),
        };
        removed.map_err(|e| FsError::from_io("failed to delete the file", &e))?;
        // Keep the manifest consistent if a tracked staging artifact was deleted.
        self.forget_if_staged(&resolved).await;
        Ok(OpResult::Ack {})
    }

    async fn remove_dir(&self, path: &RemotePath) -> Result<OpResult, FsError> {
        let resolved = self.resolve(path)?;
        match tokio::fs::symlink_metadata(&resolved).await {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(FsError::wrong_kind("the path is not a directory")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(FsError::path_not_found());
            }
            Err(e) => return Err(FsError::from_io("failed to stat the path", &e)),
        }
        tokio::fs::remove_dir(&resolved)
            .await
            .map_err(|e| FsError::from_io("failed to remove the directory", &e))?;
        Ok(OpResult::Ack {})
    }

    /// One step of removing a folder with everything in it (ST-M010-001), or,
    /// with `stop`, the end of a removal the caller will not continue.
    async fn remove_tree(&self, path: &RemotePath, stop: bool) -> Result<OpResult, FsError> {
        if stop {
            // Nothing is touched: the request only says that the removal ended.
            return Ok(tree::StepReport::stopped().into_result());
        }
        self.remove_tree_step(path, tree::StepBounds::protocol())
            .await
            .map(tree::StepReport::into_result)
    }

    /// Runs one removal step within `bounds`, off the async runtime.
    ///
    /// If this future is dropped while the step runs — the session task is
    /// aborted — the step is told to stop and ends at its next entry instead
    /// of working on unobserved until its bounds.
    async fn remove_tree_step(
        &self,
        path: &RemotePath,
        bounds: tree::StepBounds,
    ) -> Result<tree::StepReport, FsError> {
        let root = self.resolve(path)?;
        let stop = Arc::new(AtomicBool::new(false));
        let _stop_when_dropped = StopOnDrop(Arc::clone(&stop));
        tokio::task::spawn_blocking(move || {
            tree::remove_step(&root, bounds, &|| stop.load(Ordering::SeqCst))
        })
        .await
        .map_err(|_| FsError::internal())?
    }

    // --- staging ----------------------------------------------------------

    async fn list_staged(&self) -> Result<OpResult, FsError> {
        let manifest = self.load_manifest().await?;
        let entries = manifest
            .entries()
            .iter()
            .take(limits::MAX_DIR_PAGE)
            .cloned();
        Ok(OpResult::StagedList {
            entries: entries.collect(),
        })
    }

    /// Removes staged artifacts a `trigger` selects and every part file, and
    /// reports honest per-artifact outcomes. Records for artifacts that were
    /// removed (or were already gone) are dropped; records for artifacts that
    /// could not be removed are kept so a later cleanup can retry.
    pub async fn cleanup(&self, trigger: CleanupTrigger) -> Result<OpResult, FsError> {
        let mut manifest = self.load_manifest().await?;
        let selected = manifest.selected(trigger);
        let mut removed: u32 = 0;
        let mut failed: Vec<CleanupFailure> = Vec::new();

        for canonical_path in selected {
            match tokio::fs::remove_file(&canonical_path).await {
                Ok(()) => {
                    removed = removed.saturating_add(1);
                    manifest.remove(&canonical_path);
                }
                // Already gone: the desired state is reached; drop the record.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    manifest.remove(&canonical_path);
                }
                Err(e) if failed.len() < limits::MAX_DIR_PAGE => {
                    failed.push(CleanupFailure {
                        canonical_path,
                        reason: cleanup_reason(&e).to_string(),
                    });
                }
                Err(_) => {}
            }
        }

        // Sweep orphaned part files regardless of trigger; they are transient.
        removed = removed.saturating_add(self.sweep_parts(&mut failed).await);
        // So are the parts of unfinished transfers to a folder, which lie
        // beside their destinations (ST-M010-001).
        removed = removed.saturating_add(self.sweep_unfinished(&mut failed).await);

        self.store_manifest(&manifest).await?;
        Ok(OpResult::CleanupReport { removed, failed })
    }

    /// Removes the part of every transfer to a folder that this store began
    /// and never finished, and reports the ones it could not remove. A part
    /// that is removed, or already gone, is forgotten; one that could not be
    /// removed stays remembered, so a later cleanup tries again.
    async fn sweep_unfinished(&self, failed: &mut Vec<CleanupFailure>) -> u32 {
        let parts: Vec<PathBuf> = self.unfinished().iter().cloned().collect();
        let mut removed: u32 = 0;
        for part in parts {
            match tokio::fs::remove_file(&part).await {
                Ok(()) => removed = removed.saturating_add(1),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    if failed.len() < limits::MAX_DIR_PAGE {
                        failed.push(CleanupFailure {
                            canonical_path: part.to_string_lossy().into_owned(),
                            reason: cleanup_reason(&e).to_string(),
                        });
                    }
                    continue;
                }
            }
            self.forget_unfinished(&part);
        }
        removed
    }

    async fn sweep_parts(&self, failed: &mut Vec<CleanupFailure>) -> u32 {
        let parts_dir = self.staging_root.join(PARTS_DIR);
        let mut rd = match tokio::fs::read_dir(&parts_dir).await {
            Ok(rd) => rd,
            Err(_) => return 0,
        };
        let mut removed: u32 = 0;
        loop {
            let entry = match rd.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(_) => break,
            };
            let path = entry.path();
            match tokio::fs::remove_file(&path).await {
                Ok(()) => removed = removed.saturating_add(1),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) if failed.len() < limits::MAX_DIR_PAGE => failed.push(CleanupFailure {
                    canonical_path: path.to_string_lossy().into_owned(),
                    reason: cleanup_reason(&e).to_string(),
                }),
                Err(_) => {}
            }
        }
        removed
    }

    // --- startup crash recovery -------------------------------------------

    /// Sweeps every session staging tree under the platform default base with
    /// the `Shutdown` trigger, for crash recovery at agent startup.
    ///
    /// It removes everything a normal shutdown would (`OnShutdown` **and**
    /// `OnSessionEnd` artifacts) plus orphaned parts, deletes each session
    /// directory once nothing but emptied bookkeeping remains, and reports
    /// honest per-artifact failures for the next start to retry (REQ-F-019,
    /// REQ-N-008). A `Keep` artifact and any unrecognised content are left
    /// untouched, so the sweep never destroys what policy or the customer meant
    /// to keep.
    pub async fn sweep_startup() -> StartupSweepReport {
        Self::sweep_all_sessions(default_staging_base(), CleanupTrigger::Shutdown).await
    }

    /// The base-directory sweep behind [`FileStore::sweep_startup`], with an
    /// explicit base and trigger so it can be sandboxed and driven with any
    /// trigger by tests. Never errors: a base that does not exist yet is a clean
    /// no-op, and a per-session failure leaves that tree for the next start
    /// rather than aborting the whole sweep.
    pub async fn sweep_all_sessions(
        base: impl Into<PathBuf>,
        trigger: CleanupTrigger,
    ) -> StartupSweepReport {
        let base = base.into();
        let mut report = StartupSweepReport::default();
        let mut rd = match tokio::fs::read_dir(&base).await {
            Ok(rd) => rd,
            // No base yet: nothing was ever staged. A clean no-op.
            Err(_) => return report,
        };

        loop {
            let entry = match rd.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(_) => break,
            };
            // Foreign-dir safe: a session staging tree is a *directory* whose
            // name is a session id. A stray file, or a folder named anything
            // else, belongs to someone else and is left exactly as found.
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            let Ok(session) = name.parse::<SessionId>() else {
                continue;
            };
            match entry.file_type().await {
                Ok(kind) if kind.is_dir() => {}
                _ => continue,
            }

            report.sessions_scanned = report.sessions_scanned.saturating_add(1);
            let store = Self::with_base(session, base.clone());
            match store.cleanup(trigger).await {
                Ok(OpResult::CleanupReport { removed, failed }) => {
                    report.artifacts_removed = report.artifacts_removed.saturating_add(removed);
                    let clean = failed.is_empty();
                    for failure in failed {
                        if report.failed.len() < limits::MAX_DIR_PAGE {
                            report.failed.push(failure);
                        }
                    }
                    // Only reclaim the directory when the sweep left nothing we
                    // must keep; a residual failure keeps the tree so the next
                    // start retries it.
                    if clean && Self::try_reclaim_session_dir(store.staging_root()).await {
                        report.sessions_removed = report.sessions_removed.saturating_add(1);
                    }
                }
                // `cleanup` only yields a `CleanupReport`; any other shape or a
                // per-session error leaves that tree for the next start.
                Ok(_) => {}
                Err(_) => {}
            }
        }

        report
    }

    // --- read-only residual census (ST-M006-003) --------------------------

    /// Censuses every session staging tree under the platform default base
    /// without removing anything — the inspect-side entry point behind the
    /// owned-resource inventory's recovery verb (ST-M006-003, REQ-F-019).
    pub async fn census_startup() -> StagingCensus {
        Self::census_all_sessions(default_staging_base()).await
    }

    /// The base-directory census behind [`FileStore::census_startup`], with an
    /// explicit base so it can be sandboxed by tests. It mirrors
    /// [`FileStore::sweep_all_sessions`]'s foreign-directory safety exactly —
    /// only directories named as session ids are examined — but reads rather
    /// than removes. Never errors: a base that does not exist yet is an empty
    /// census, and an unreadable session tree is simply omitted.
    pub async fn census_all_sessions(base: impl Into<PathBuf>) -> StagingCensus {
        let base = base.into();
        let mut census = StagingCensus::default();
        let mut rd = match tokio::fs::read_dir(&base).await {
            Ok(rd) => rd,
            // No base yet: nothing was ever staged. A clean, empty census.
            Err(_) => return census,
        };

        loop {
            let entry = match rd.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(_) => break,
            };
            // Same foreign-dir safety as the sweep: a session staging tree is a
            // directory whose name parses as a session id. Anything else is
            // someone else's and is not reported as CretLink residue.
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            let Ok(session) = name.parse::<SessionId>() else {
                continue;
            };
            match entry.file_type().await {
                Ok(kind) if kind.is_dir() => {}
                _ => continue,
            }

            let store = Self::with_base(session, base.clone());
            let staging = store.census().await;
            // Omit spent trees so a clean base reads as empty, not a row of
            // zeros; a tree with any staged record or orphaned part is residue.
            if !staging.staged.is_empty() || staging.orphaned_parts > 0 {
                census.sessions.push(staging);
            }
        }

        census
    }

    /// Reads this session's staging residue without removing anything: the
    /// manifest's staged records and a count of orphaned part files. The
    /// read-only counterpart to [`FileStore::cleanup`]. A manifest that cannot
    /// be read yields no enumerable records rather than an error — the sweep
    /// remains the authority on removal and its honest failure reporting.
    pub async fn census(&self) -> SessionStaging {
        let staged = match self.load_manifest().await {
            Ok(manifest) => manifest.entries().to_vec(),
            Err(_) => Vec::new(),
        };
        SessionStaging {
            session: self.session,
            staged,
            orphaned_parts: self.count_orphaned_parts().await,
        }
    }

    /// Counts orphaned part files under `.parts` without removing them.
    async fn count_orphaned_parts(&self) -> u32 {
        let parts_dir = self.staging_root.join(PARTS_DIR);
        let mut rd = match tokio::fs::read_dir(&parts_dir).await {
            Ok(rd) => rd,
            Err(_) => return 0,
        };
        let mut count: u32 = 0;
        loop {
            match rd.next_entry().await {
                Ok(Some(_)) => count = count.saturating_add(1),
                Ok(None) => break,
                Err(_) => break,
            }
        }
        count
    }

    /// Removes a session staging directory when it holds only its own emptied
    /// bookkeeping — an entry-free manifest and an empty `.parts` directory.
    /// Returns whether the directory was removed. Any unrecognised entry (a
    /// `Keep` artifact the trigger spared, or foreign content) makes this a
    /// no-op, so a directory is reclaimed only when it is genuinely spent.
    async fn try_reclaim_session_dir(root: &Path) -> bool {
        let mut rd = match tokio::fs::read_dir(root).await {
            Ok(rd) => rd,
            Err(_) => return false,
        };
        loop {
            let entry = match rd.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(_) => return false,
            };
            let file_name = entry.file_name();
            match file_name.to_string_lossy().as_ref() {
                // Bookkeeping is droppable only when it records no survivors.
                MANIFEST_FILE => match Manifest::load(&entry.path()).await {
                    Ok(manifest) if manifest.len() == 0 => {}
                    _ => return false,
                },
                // The transient parts dir is droppable only when it is empty
                // (the cleanup already swept orphaned parts).
                PARTS_DIR => {
                    if !Self::dir_is_empty(&entry.path()).await {
                        return false;
                    }
                }
                // Anything else is real content that must not be touched.
                _ => return false,
            }
        }
        // Only spent bookkeeping remained: drop it, then the directory itself.
        let _ = tokio::fs::remove_file(root.join(MANIFEST_FILE)).await;
        let _ = tokio::fs::remove_dir_all(root.join(PARTS_DIR)).await;
        tokio::fs::remove_dir(root).await.is_ok()
    }

    /// Whether `dir` has no entries. A missing directory counts as empty.
    async fn dir_is_empty(dir: &Path) -> bool {
        match tokio::fs::read_dir(dir).await {
            Ok(mut rd) => matches!(rd.next_entry().await, Ok(None)),
            Err(_) => true,
        }
    }

    // --- helpers ----------------------------------------------------------

    /// Resolves a wire path to an absolute filesystem path, guarding that a
    /// staging reference cannot escape its root.
    fn resolve(&self, path: &RemotePath) -> Result<PathBuf, FsError> {
        match path {
            RemotePath::Absolute { path } => Ok(PathBuf::from(path)),
            RemotePath::Staging { name } => {
                // A staging name must be a single safe component. The explicit
                // check is the defence-in-depth backstop behind wire validation
                // and must hold identically on every platform: a backslash is a
                // separator on Windows but a literal filename byte on Unix, so
                // relying on `Path::join`/`parent` alone would let `..\x` slip
                // through on Unix. Reject any separator or traversal outright.
                if !is_safe_staging_name(name) {
                    return Err(FsError::outside_staging());
                }
                let joined = self.staging_root.join(name);
                if joined.parent() != Some(self.staging_root.as_path()) {
                    return Err(FsError::outside_staging());
                }
                Ok(joined)
            }
        }
    }

    /// The part-file path for a destination. Staging parts live under the
    /// staging root's `.parts` directory; an absolute destination's part lives
    /// beside the final file (same volume, so the commit rename is atomic),
    /// named by a session-scoped digest so it never collides with a real file
    /// and stays within the component-length limit.
    fn part_path(&self, dest: &RemotePath) -> Result<PathBuf, FsError> {
        match dest {
            RemotePath::Staging { name } => {
                if !is_safe_staging_name(name) {
                    return Err(FsError::outside_staging());
                }
                Ok(self.staging_root.join(PARTS_DIR).join(name))
            }
            RemotePath::Absolute { .. } => {
                let final_path = self.resolve(dest)?;
                let parent = final_path.parent().ok_or_else(|| {
                    FsError::wrong_kind("the destination has no parent directory")
                })?;
                Ok(parent.join(self.part_name_for(&final_path)))
            }
        }
    }

    fn part_name_for(&self, final_path: &Path) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.session.to_string().as_bytes());
        hasher.update(b"\0");
        hasher.update(final_path.to_string_lossy().as_bytes());
        let digest = hasher.finalize();
        format!(".cretlink-part.{}", hex(&digest[..8]))
    }

    /// The current length of the pending part for `dest`, or `None` if there is
    /// no part (or it cannot be probed).
    async fn part_len(&self, dest: &RemotePath) -> Option<u64> {
        let part = self.part_path(dest).ok()?;
        tokio::fs::symlink_metadata(&part)
            .await
            .ok()
            .map(|m| m.len())
    }

    async fn ensure_staging_dirs(&self) -> Result<(), FsError> {
        tokio::fs::create_dir_all(self.staging_root.join(PARTS_DIR))
            .await
            .map_err(|e| FsError::from_io("failed to create the staging area", &e))
    }

    /// The parts of the unfinished transfers to a folder. A poisoned lock is
    /// read as it is: the set holds plain paths, and no panic can leave it
    /// half-changed.
    fn unfinished(&self) -> std::sync::MutexGuard<'_, BTreeSet<PathBuf>> {
        self.unfinished
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Remembers `part` as the part of an unfinished transfer to a folder, so
    /// that the cleanup removes it if the transfer never finishes. Refuses a
    /// new one beyond [`MAX_UNFINISHED_TRANSFERS`].
    fn remember_unfinished(&self, part: &Path) -> Result<(), FsError> {
        let mut unfinished = self.unfinished();
        if unfinished.contains(part) {
            return Ok(());
        }
        if unfinished.len() >= MAX_UNFINISHED_TRANSFERS {
            return Err(FsError::new(
                ErrorCode::StorageExhausted,
                "too many unfinished transfers; finish or discard some first",
            ));
        }
        unfinished.insert(part.to_path_buf());
        Ok(())
    }

    /// Forgets `part`: its transfer was committed, or the part was discarded.
    fn forget_unfinished(&self, part: &Path) {
        self.unfinished().remove(part);
    }

    fn manifest_path(&self) -> PathBuf {
        self.staging_root.join(MANIFEST_FILE)
    }

    async fn load_manifest(&self) -> Result<Manifest, FsError> {
        Manifest::load(&self.manifest_path())
            .await
            .map_err(|_| FsError::internal())
    }

    async fn store_manifest(&self, manifest: &Manifest) -> Result<(), FsError> {
        self.ensure_staging_dirs().await?;
        manifest
            .store(&self.manifest_path())
            .await
            .map_err(|_| FsError::internal())
    }

    /// Drops the manifest record for a deleted path when it names a tracked
    /// staging artifact, so the manifest never points at a file that is gone.
    async fn forget_if_staged(&self, resolved: &Path) {
        if !resolved.starts_with(&self.staging_root) {
            return;
        }
        let canonical = resolved.to_string_lossy();
        if let Ok(mut manifest) = self.load_manifest().await
            && manifest.remove(&canonical)
        {
            let _ = self.store_manifest(&manifest).await;
        }
    }

    /// Fails with [`ErrorCode::StorageExhausted`] when committing or writing
    /// `new_len` bytes would push the session's staged total over the quota. An
    /// `exclude` canonical path (an overwrite of an already-recorded artifact)
    /// is not double-counted.
    async fn check_staging_quota(
        &self,
        new_len: u64,
        exclude: Option<&str>,
    ) -> Result<(), FsError> {
        let manifest = self.load_manifest().await?;
        let existing: u64 = manifest
            .entries()
            .iter()
            .filter(|e| Some(e.canonical_path.as_str()) != exclude)
            .map(|e| e.len)
            .sum();
        if existing.saturating_add(new_len) > limits::MAX_STAGING_BYTES {
            return Err(FsError::storage_exhausted());
        }
        Ok(())
    }
}

/// Grouped [`Operation::FsCommit`] arguments, so the commit path takes one
/// value rather than a long parameter list.
struct CommitArgs {
    dest: RemotePath,
    len: u64,
    algo: DigestAlgo,
    digest: B64Bytes,
    overwrite: bool,
    source: Option<String>,
    cleanup: cretlink_protocol::CleanupPolicy,
}

/// Tells a running folder-removal step to stop when it is dropped, which
/// happens when the future that waits for the step is dropped.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Appends `data` to the part file at `part`, creating it if needed, and makes
/// sure the bytes have reached the operating system before it returns.
///
/// The part is opened **without following a link**: if the last part of its
/// name is a link, nothing is written through it. A full disk is reported as
/// [`ErrorCode::StorageExhausted`], whichever of the two calls meets it.
async fn append_to_part(part: &Path, data: &[u8]) -> Result<(), FsError> {
    let mut options = tokio::fs::OpenOptions::new();
    options.create(true).append(true);
    // Refuse a symbolic link as the part itself (ELOOP) instead of opening
    // what it points at.
    #[cfg(unix)]
    options.custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits());
    // FILE_FLAG_OPEN_REPARSE_POINT: open a link itself, never what it points
    // at. It has no effect on a regular file.
    #[cfg(windows)]
    options.custom_flags(0x0020_0000);
    let mut file = options
        .open(part)
        .await
        .map_err(|e| FsError::from_io("failed to open the part file", &e))?;
    file.write_all(data)
        .await
        .map_err(|e| FsError::from_io("failed to write the part file", &e))?;
    // `tokio::fs::File` buffers writes and dispatches the actual write to a
    // background task: `write_all` resolves once the bytes are buffered, not
    // once they reach the OS file, and dropping the handle does not flush.
    // Flush now so the appended bytes are visible to the next write's length
    // check and to the commit's length/digest verification (otherwise a
    // subsequent stat can race the still-pending write and see a short
    // file — a genuine integrity hazard, not just a test flake). The flush is
    // also where the error of that background write surfaces, a full disk
    // included.
    file.flush()
        .await
        .map_err(|e| FsError::from_io("failed to flush the part file", &e))
}

/// Lists one bounded, name-ordered page of `dir`.
///
/// Only the smallest [`limits::MAX_DIR_PAGE`] names greater than `continue_after`
/// are retained, via a bounded max-heap, so a directory with millions of entries
/// still costs one page of memory. `end` is false when more names remain beyond
/// the page.
async fn list_dir(
    dir: &Path,
    continue_after: Option<&str>,
    hide_internal: bool,
) -> Result<(Vec<FileEntry>, bool), FsError> {
    let mut rd = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| FsError::from_io("failed to open the directory", &e))?;
    let mut heap: BinaryHeap<String> = BinaryHeap::new();
    let mut overflow = false;
    loop {
        let entry = match rd.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(e) => return Err(FsError::from_io("failed to read the directory", &e)),
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if hide_internal && (name == MANIFEST_FILE || name == PARTS_DIR) {
            continue;
        }
        if let Some(cursor) = continue_after
            && name.as_str() <= cursor
        {
            continue;
        }
        heap.push(name);
        if heap.len() > limits::MAX_DIR_PAGE {
            heap.pop();
            overflow = true;
        }
    }
    let names = heap.into_sorted_vec();
    let mut entries = Vec::with_capacity(names.len());
    for name in names {
        let child = dir.join(&name);
        // A file may vanish between listing and stat; skip it rather than fail.
        if let Ok(meta) = tokio::fs::symlink_metadata(&child).await {
            entries.push(file_entry(name, &meta));
        }
    }
    Ok((entries, !overflow))
}

/// Hashes a file with SHA-256, up to `limit` bytes when set, returning the
/// digest and the number of bytes hashed.
async fn sha256_file(path: &Path, limit: Option<u64>) -> std::io::Result<([u8; 32], u64)> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_BUF_BYTES];
    let mut total: u64 = 0;
    loop {
        let cap = match limit {
            Some(l) if total >= l => break,
            Some(l) => ((l - total).min(buf.len() as u64)) as usize,
            None => buf.len(),
        };
        let n = file.read(&mut buf[..cap]).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    let out = hasher.finalize();
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&out);
    Ok((digest, total))
}

/// Builds a [`FileEntry`] from `lstat` metadata (a reparse point is described,
/// never followed).
fn file_entry(name: String, meta: &std::fs::Metadata) -> FileEntry {
    let ft = meta.file_type();
    let kind = if ft.is_symlink() {
        FileKind::Symlink
    } else if ft.is_dir() {
        FileKind::Dir
    } else if ft.is_file() {
        FileKind::File
    } else {
        FileKind::Other
    };
    FileEntry {
        name,
        kind,
        len: meta.len(),
        modified_ms: meta.modified().ok().and_then(system_time_to_ms),
        readonly: meta.permissions().readonly(),
    }
}

/// The name reported for a stat: the final path component, falling back to the
/// whole path text for a bare drive root.
fn entry_name(path: &RemotePath, resolved: &Path) -> String {
    if let RemotePath::Staging { name } = path {
        return name.clone();
    }
    match resolved.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => resolved.to_string_lossy().into_owned(),
    }
}

/// Whether `name` is a single, safe staging component: non-empty, not a
/// current/parent-directory reference, and free of any path separator or
/// drive/ADS colon. The separator check names both `/` and `\` explicitly so
/// the guard is identical on every platform (a backslash is only a separator on
/// Windows), and a NUL is rejected because it cannot appear in a real filename.
fn is_safe_staging_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', ':', '\0'])
}

/// Whether two absolute paths sit on the same volume (drive), so a rename is a
/// same-volume atomic move. When the platform exposes no drive prefix (non-
/// Windows), assume same-volume and let the OS report a cross-device error.
fn same_volume(a: &Path, b: &Path) -> bool {
    match (drive_letter(a), drive_letter(b)) {
        (Some(x), Some(y)) => x == y,
        _ => true,
    }
}

fn drive_letter(path: &Path) -> Option<char> {
    for component in path.components() {
        if let Component::Prefix(prefix) = component
            && let Prefix::Disk(disk) | Prefix::VerbatimDisk(disk) = prefix.kind()
        {
            return Some((disk as char).to_ascii_uppercase());
        }
    }
    None
}

/// A short, secret-free reason for a cleanup failure, drawn from the error kind.
fn cleanup_reason(err: &std::io::Error) -> &'static str {
    match io_error_code(err) {
        ErrorCode::AccessDenied => "access denied",
        ErrorCode::FileInUse => "in use",
        ErrorCode::PathNotFound => "not found",
        ErrorCode::WrongPathKind => "wrong kind",
        ErrorCode::DirNotEmpty => "not empty",
        _ => "i/o error",
    }
}

fn system_time_to_ms(time: SystemTime) -> Option<u64> {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
}

fn now_ms() -> u64 {
    system_time_to_ms(SystemTime::now()).unwrap_or(0)
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// The default staging base directory: `%ProgramData%\CretLink\staging` on
/// Windows (the agent is elevated and this is machine-scoped), falling back to a
/// temp directory when the variable is unset or off Windows.
///
/// Public so the owned-resource inventory's recovery verb can name and sweep the
/// exact tree the agent stages into (ST-M006-003), rather than re-deriving it.
pub fn default_staging_base() -> PathBuf {
    if let Some(program_data) = std::env::var_os("ProgramData") {
        return PathBuf::from(program_data).join("CretLink").join("staging");
    }
    std::env::temp_dir().join("cretlink").join("staging")
}
