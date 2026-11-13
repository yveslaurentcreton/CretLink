//! [`FileStore`] unit tests (ST-M003-004/005).
//!
//! These exercise the store against a real, sandboxed filesystem: each test
//! roots the staging area and its target paths under a unique temp directory it
//! removes on drop. The assertions hold on every CI platform — path resolution,
//! the append-only part / verified-commit discipline, the persistent manifest,
//! and honest cleanup are all platform-independent. The Windows-only status-code
//! mapping is covered indirectly through the portable `ErrorKind` fallback.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cretlink_protocol::{
    B64Bytes, CleanupPolicy, CleanupTrigger, DigestAlgo, ErrorCode, FileKind, OpResult, Operation,
    RemotePath, SessionId,
};
use sha2::{Digest, Sha256};

use super::{FileStore, FsError, SessionStaging, StagingCensus};

/// A unique temp directory removed when the guard drops, so tests never share
/// state or leak files.
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let root = std::env::temp_dir().join(format!("cretlink-fs-test-{pid}-{n}"));
        std::fs::create_dir_all(&root).expect("create sandbox root");
        Self { root }
    }

    /// The staging base handed to the store (kept apart from target paths).
    fn staging_base(&self) -> PathBuf {
        self.root.join("staging-base")
    }

    /// A target directory for absolute-path operations, created eagerly.
    fn target(&self) -> PathBuf {
        let dir = self.root.join("target");
        std::fs::create_dir_all(&dir).expect("create target dir");
        dir
    }

    fn store(&self) -> FileStore {
        FileStore::with_base(SessionId::new(), self.staging_base())
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// An absolute wire path to a name under the sandbox target directory.
fn abs(dir: &Path, name: &str) -> RemotePath {
    RemotePath::Absolute {
        path: dir.join(name).to_string_lossy().into_owned(),
    }
}

fn staging(name: &str) -> RemotePath {
    RemotePath::Staging {
        name: name.to_string(),
    }
}

fn sha256(bytes: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().to_vec()
}

/// Drives a whole transfer: append `bytes` in `chunk` slices, then commit with
/// the correct declared length and digest.
async fn transfer(
    store: &FileStore,
    dest: RemotePath,
    bytes: &[u8],
    chunk: usize,
    overwrite: bool,
    source: Option<&str>,
    cleanup: CleanupPolicy,
) -> Result<OpResult, FsError> {
    let mut offset = 0u64;
    for slice in bytes.chunks(chunk.max(1)) {
        let accepted = store
            .perform(Operation::FsWrite {
                dest: dest.clone(),
                offset,
                data: B64Bytes::new(slice.to_vec()),
            })
            .await?;
        match accepted {
            OpResult::WriteAccepted { part_len } => {
                offset += slice.len() as u64;
                assert_eq!(part_len, offset, "part length tracks the appended bytes");
            }
            other => panic!("expected WriteAccepted, got {other:?}"),
        }
    }
    store
        .perform(Operation::FsCommit {
            dest,
            len: bytes.len() as u64,
            algo: DigestAlgo::Sha256,
            digest: B64Bytes::new(sha256(bytes)),
            overwrite,
            source: source.map(str::to_string),
            cleanup,
        })
        .await
}

#[tokio::test]
async fn stat_missing_path_reports_absent() {
    let sb = Sandbox::new();
    let store = sb.store();
    let result = store
        .perform(Operation::FsStat {
            path: abs(&sb.target(), "nope.txt"),
        })
        .await
        .expect("stat should succeed even when absent");
    match result {
        OpResult::PathStat { entry, partial_len } => {
            assert!(entry.is_none(), "a missing path has no entry");
            assert!(partial_len.is_none(), "no pending part exists");
        }
        other => panic!("expected PathStat, got {other:?}"),
    }
}

#[tokio::test]
async fn create_dir_and_list_reports_children() {
    let sb = Sandbox::new();
    let store = sb.store();
    let target = sb.target();

    store
        .perform(Operation::CreateDir {
            path: abs(&target, "sub"),
        })
        .await
        .expect("create_dir");
    let sub = target.join("sub");
    // Two files and a nested directory to list.
    transfer(
        &store,
        abs(&sub, "a.txt"),
        b"alpha",
        2,
        false,
        None,
        CleanupPolicy::Keep,
    )
    .await
    .expect("commit a.txt");
    transfer(
        &store,
        abs(&sub, "b.txt"),
        b"beta",
        4,
        false,
        None,
        CleanupPolicy::Keep,
    )
    .await
    .expect("commit b.txt");
    store
        .perform(Operation::CreateDir {
            path: abs(&sub, "child"),
        })
        .await
        .expect("nested dir");

    let listing = store
        .perform(Operation::FsList {
            path: abs(&target, "sub"),
            continue_after: None,
        })
        .await
        .expect("list");
    match listing {
        OpResult::DirEntries { entries, end } => {
            assert!(end, "the page is complete");
            let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, vec!["a.txt", "b.txt", "child"], "name-sorted");
            let child = entries.iter().find(|e| e.name == "child").unwrap();
            assert_eq!(child.kind, FileKind::Dir);
            let a = entries.iter().find(|e| e.name == "a.txt").unwrap();
            assert_eq!(a.kind, FileKind::File);
            assert_eq!(a.len, 5, "alpha is five bytes");
        }
        other => panic!("expected DirEntries, got {other:?}"),
    }
}

#[tokio::test]
async fn list_rejects_a_file() {
    let sb = Sandbox::new();
    let store = sb.store();
    let target = sb.target();
    transfer(
        &store,
        abs(&target, "f.txt"),
        b"x",
        1,
        false,
        None,
        CleanupPolicy::Keep,
    )
    .await
    .expect("commit");
    let err = store
        .perform(Operation::FsList {
            path: abs(&target, "f.txt"),
            continue_after: None,
        })
        .await
        .expect_err("listing a file must fail");
    assert_eq!(err.code, ErrorCode::WrongPathKind);
}

#[tokio::test]
async fn write_commit_roundtrip_publishes_verified_bytes() {
    let sb = Sandbox::new();
    let store = sb.store();
    let target = sb.target();
    let payload: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();

    let committed = transfer(
        &store,
        abs(&target, "out.bin"),
        &payload,
        64,
        false,
        None,
        CleanupPolicy::Keep,
    )
    .await
    .expect("commit");
    match committed {
        OpResult::Committed {
            len,
            digest,
            replaced,
            ..
        } => {
            assert_eq!(len, payload.len() as u64);
            assert_eq!(digest.as_bytes(), sha256(&payload).as_slice());
            assert!(!replaced, "nothing was overwritten");
        }
        other => panic!("expected Committed, got {other:?}"),
    }
    let on_disk = std::fs::read(target.join("out.bin")).expect("read published file");
    assert_eq!(on_disk, payload, "the published bytes match exactly");
    // The part file is gone once published.
    let stat = store
        .perform(Operation::FsStat {
            path: abs(&target, "out.bin"),
        })
        .await
        .expect("stat");
    match stat {
        OpResult::PathStat { partial_len, .. } => {
            assert!(partial_len.is_none(), "no part remains after commit")
        }
        other => panic!("expected PathStat, got {other:?}"),
    }
}

#[tokio::test]
async fn commit_with_wrong_digest_leaves_no_file() {
    let sb = Sandbox::new();
    let store = sb.store();
    let target = sb.target();
    store
        .perform(Operation::FsWrite {
            dest: abs(&target, "bad.bin"),
            offset: 0,
            data: B64Bytes::new(b"hello".to_vec()),
        })
        .await
        .expect("write");
    let err = store
        .perform(Operation::FsCommit {
            dest: abs(&target, "bad.bin"),
            len: 5,
            algo: DigestAlgo::Sha256,
            digest: B64Bytes::new(sha256(b"HELLO")), // wrong
            overwrite: false,
            source: None,
            cleanup: CleanupPolicy::Keep,
        })
        .await
        .expect_err("digest mismatch must fail");
    assert_eq!(err.code, ErrorCode::IntegrityMismatch);
    assert!(
        !target.join("bad.bin").exists(),
        "a failed commit publishes nothing"
    );
    // The part survives so a corrected commit can resume.
    let stat = store
        .perform(Operation::FsStat {
            path: abs(&target, "bad.bin"),
        })
        .await
        .expect("stat");
    match stat {
        OpResult::PathStat { partial_len, .. } => {
            assert_eq!(partial_len, Some(5), "the part is intact for a resume")
        }
        other => panic!("expected PathStat, got {other:?}"),
    }
}

#[tokio::test]
async fn commit_with_wrong_length_fails() {
    let sb = Sandbox::new();
    let store = sb.store();
    let target = sb.target();
    store
        .perform(Operation::FsWrite {
            dest: abs(&target, "len.bin"),
            offset: 0,
            data: B64Bytes::new(b"hello".to_vec()),
        })
        .await
        .expect("write");
    let err = store
        .perform(Operation::FsCommit {
            dest: abs(&target, "len.bin"),
            len: 4, // wrong
            algo: DigestAlgo::Sha256,
            digest: B64Bytes::new(sha256(b"hell")),
            overwrite: false,
            source: None,
            cleanup: CleanupPolicy::Keep,
        })
        .await
        .expect_err("length mismatch must fail");
    assert_eq!(err.code, ErrorCode::IntegrityMismatch);
}

#[tokio::test]
async fn write_at_wrong_offset_conflicts() {
    let sb = Sandbox::new();
    let store = sb.store();
    let target = sb.target();
    store
        .perform(Operation::FsWrite {
            dest: abs(&target, "c.bin"),
            offset: 0,
            data: B64Bytes::new(b"abc".to_vec()),
        })
        .await
        .expect("first write");
    let err = store
        .perform(Operation::FsWrite {
            dest: abs(&target, "c.bin"),
            offset: 1, // should be 3
            data: B64Bytes::new(b"def".to_vec()),
        })
        .await
        .expect_err("a gap or rewind must be rejected");
    assert_eq!(err.code, ErrorCode::WriteConflict);
}

#[tokio::test]
async fn commit_without_overwrite_rejects_existing() {
    let sb = Sandbox::new();
    let store = sb.store();
    let target = sb.target();
    transfer(
        &store,
        abs(&target, "e.txt"),
        b"one",
        3,
        false,
        None,
        CleanupPolicy::Keep,
    )
    .await
    .expect("first commit");
    let err = transfer(
        &store,
        abs(&target, "e.txt"),
        b"two",
        3,
        false, // no overwrite
        None,
        CleanupPolicy::Keep,
    )
    .await
    .expect_err("a second commit without overwrite must fail");
    assert_eq!(err.code, ErrorCode::AlreadyExists);
    let on_disk = std::fs::read(target.join("e.txt")).expect("read");
    assert_eq!(on_disk, b"one", "the original file is untouched");
}

#[tokio::test]
async fn commit_with_overwrite_replaces() {
    let sb = Sandbox::new();
    let store = sb.store();
    let target = sb.target();
    transfer(
        &store,
        abs(&target, "e.txt"),
        b"one",
        3,
        false,
        None,
        CleanupPolicy::Keep,
    )
    .await
    .expect("first commit");
    let result = transfer(
        &store,
        abs(&target, "e.txt"),
        b"two",
        3,
        true, // overwrite
        None,
        CleanupPolicy::Keep,
    )
    .await
    .expect("overwrite commit");
    match result {
        OpResult::Committed { replaced, .. } => assert!(replaced, "the prior file was replaced"),
        other => panic!("expected Committed, got {other:?}"),
    }
    let on_disk = std::fs::read(target.join("e.txt")).expect("read");
    assert_eq!(on_disk, b"two");
}

#[tokio::test]
async fn read_honours_offset_and_eof() {
    let sb = Sandbox::new();
    let store = sb.store();
    let target = sb.target();
    transfer(
        &store,
        abs(&target, "r.txt"),
        b"0123456789",
        10,
        false,
        None,
        CleanupPolicy::Keep,
    )
    .await
    .expect("commit");

    let mid = store
        .perform(Operation::FsRead {
            path: abs(&target, "r.txt"),
            offset: 3,
            len: 4,
        })
        .await
        .expect("read");
    match mid {
        OpResult::FileData {
            data,
            eof,
            file_len,
        } => {
            assert_eq!(data.as_bytes(), b"3456");
            assert!(!eof, "more bytes remain");
            assert_eq!(file_len, 10);
        }
        other => panic!("expected FileData, got {other:?}"),
    }

    let tail = store
        .perform(Operation::FsRead {
            path: abs(&target, "r.txt"),
            offset: 8,
            len: 100,
        })
        .await
        .expect("read tail");
    match tail {
        OpResult::FileData { data, eof, .. } => {
            assert_eq!(data.as_bytes(), b"89");
            assert!(eof, "the read reached the end");
        }
        other => panic!("expected FileData, got {other:?}"),
    }

    let past = store
        .perform(Operation::FsRead {
            path: abs(&target, "r.txt"),
            offset: 10,
            len: 4,
        })
        .await
        .expect("read past end");
    match past {
        OpResult::FileData { data, eof, .. } => {
            assert!(data.as_bytes().is_empty());
            assert!(eof);
        }
        other => panic!("expected FileData, got {other:?}"),
    }
}

#[tokio::test]
async fn digest_matches_full_and_prefix() {
    let sb = Sandbox::new();
    let store = sb.store();
    let target = sb.target();
    let payload = b"the quick brown fox";
    transfer(
        &store,
        abs(&target, "d.txt"),
        payload,
        5,
        false,
        None,
        CleanupPolicy::Keep,
    )
    .await
    .expect("commit");

    let full = store
        .perform(Operation::FsDigest {
            path: abs(&target, "d.txt"),
            prefix_len: None,
        })
        .await
        .expect("digest");
    match full {
        OpResult::FileDigest { digest, len, .. } => {
            assert_eq!(digest.as_bytes(), sha256(payload).as_slice());
            assert_eq!(len, payload.len() as u64);
        }
        other => panic!("expected FileDigest, got {other:?}"),
    }

    let prefix = store
        .perform(Operation::FsDigest {
            path: abs(&target, "d.txt"),
            prefix_len: Some(9),
        })
        .await
        .expect("prefix digest");
    match prefix {
        OpResult::FileDigest { digest, len, .. } => {
            assert_eq!(digest.as_bytes(), sha256(&payload[..9]).as_slice());
            assert_eq!(len, 9);
        }
        other => panic!("expected FileDigest, got {other:?}"),
    }

    // A prefix longer than the file cannot be satisfied.
    let err = store
        .perform(Operation::FsDigest {
            path: abs(&target, "d.txt"),
            prefix_len: Some(1000),
        })
        .await
        .expect_err("an over-long prefix must fail");
    assert_eq!(err.code, ErrorCode::IntegrityMismatch);
}

#[tokio::test]
async fn move_then_delete() {
    let sb = Sandbox::new();
    let store = sb.store();
    let target = sb.target();
    transfer(
        &store,
        abs(&target, "from.txt"),
        b"payload",
        7,
        false,
        None,
        CleanupPolicy::Keep,
    )
    .await
    .expect("commit");

    store
        .perform(Operation::FsMove {
            from: abs(&target, "from.txt"),
            to: abs(&target, "to.txt"),
            overwrite: false,
        })
        .await
        .expect("move");
    assert!(!target.join("from.txt").exists());
    assert!(target.join("to.txt").exists());

    store
        .perform(Operation::FsDelete {
            path: abs(&target, "to.txt"),
        })
        .await
        .expect("delete");
    assert!(!target.join("to.txt").exists());

    let err = store
        .perform(Operation::FsDelete {
            path: abs(&target, "to.txt"),
        })
        .await
        .expect_err("deleting a missing file must fail");
    assert_eq!(err.code, ErrorCode::PathNotFound);
}

#[tokio::test]
async fn delete_rejects_a_directory() {
    let sb = Sandbox::new();
    let store = sb.store();
    let target = sb.target();
    store
        .perform(Operation::CreateDir {
            path: abs(&target, "dir"),
        })
        .await
        .expect("create");
    let err = store
        .perform(Operation::FsDelete {
            path: abs(&target, "dir"),
        })
        .await
        .expect_err("delete must refuse a directory");
    assert_eq!(err.code, ErrorCode::WrongPathKind);
}

#[tokio::test]
async fn remove_dir_requires_empty() {
    let sb = Sandbox::new();
    let store = sb.store();
    let target = sb.target();
    store
        .perform(Operation::CreateDir {
            path: abs(&target, "d"),
        })
        .await
        .expect("create");
    transfer(
        &store,
        abs(&target.join("d"), "f.txt"),
        b"x",
        1,
        false,
        None,
        CleanupPolicy::Keep,
    )
    .await
    .expect("commit child");
    let err = store
        .perform(Operation::RemoveDir {
            path: abs(&target, "d"),
        })
        .await
        .expect_err("a non-empty directory must not be removed");
    assert_eq!(err.code, ErrorCode::DirNotEmpty);

    store
        .perform(Operation::FsDelete {
            path: abs(&target.join("d"), "f.txt"),
        })
        .await
        .expect("delete child");
    store
        .perform(Operation::RemoveDir {
            path: abs(&target, "d"),
        })
        .await
        .expect("remove now-empty dir");
    assert!(!target.join("d").exists());
}

#[tokio::test]
async fn abort_discards_the_part_and_is_idempotent() {
    let sb = Sandbox::new();
    let store = sb.store();
    let target = sb.target();
    store
        .perform(Operation::FsWrite {
            dest: abs(&target, "p.bin"),
            offset: 0,
            data: B64Bytes::new(b"partial".to_vec()),
        })
        .await
        .expect("write");
    store
        .perform(Operation::FsAbort {
            dest: abs(&target, "p.bin"),
        })
        .await
        .expect("abort");
    // A second abort with nothing to discard still succeeds.
    store
        .perform(Operation::FsAbort {
            dest: abs(&target, "p.bin"),
        })
        .await
        .expect("idempotent abort");
    let stat = store
        .perform(Operation::FsStat {
            path: abs(&target, "p.bin"),
        })
        .await
        .expect("stat");
    match stat {
        OpResult::PathStat { entry, partial_len } => {
            assert!(entry.is_none());
            assert!(partial_len.is_none(), "the part was discarded");
        }
        other => panic!("expected PathStat, got {other:?}"),
    }
}

#[tokio::test]
async fn staging_commit_requires_a_source() {
    let sb = Sandbox::new();
    let store = sb.store();
    let err = transfer(
        &store,
        staging("art.bin"),
        b"data",
        4,
        false,
        None, // no source
        CleanupPolicy::OnSessionEnd,
    )
    .await
    .expect_err("a staging commit without a source must fail");
    assert_eq!(err.code, ErrorCode::Unsupported);
}

#[tokio::test]
async fn staging_commit_lists_and_cleans_up() {
    let sb = Sandbox::new();
    let store = sb.store();
    transfer(
        &store,
        staging("art.bin"),
        b"artifact-bytes",
        4,
        false,
        Some("C:\\tech\\driver.zip"),
        CleanupPolicy::OnSessionEnd,
    )
    .await
    .expect("staging commit");

    let listed = store
        .perform(Operation::ListStaged {})
        .await
        .expect("list staged");
    match listed {
        OpResult::StagedList { entries } => {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].source, "C:\\tech\\driver.zip");
            assert_eq!(entries[0].len, 14);
            assert_eq!(entries[0].cleanup, CleanupPolicy::OnSessionEnd);
        }
        other => panic!("expected StagedList, got {other:?}"),
    }

    // The published artifact really exists under the staging root.
    let art = store.staging_root().join("art.bin");
    assert!(art.exists(), "the staged artifact is on disk");

    let report = store
        .perform(Operation::StagingCleanup {
            trigger: CleanupTrigger::SessionEnd,
        })
        .await
        .expect("cleanup");
    match report {
        OpResult::CleanupReport { removed, failed } => {
            assert_eq!(removed, 1, "the session-end artifact was removed");
            assert!(failed.is_empty(), "nothing failed");
        }
        other => panic!("expected CleanupReport, got {other:?}"),
    }
    assert!(!art.exists(), "the artifact is gone after cleanup");
    let listed = store
        .perform(Operation::ListStaged {})
        .await
        .expect("list staged after cleanup");
    match listed {
        OpResult::StagedList { entries } => assert!(entries.is_empty(), "the record was dropped"),
        other => panic!("expected StagedList, got {other:?}"),
    }
}

#[tokio::test]
async fn keep_survives_session_end_but_all_purges() {
    let sb = Sandbox::new();
    let store = sb.store();
    transfer(
        &store,
        staging("keep.bin"),
        b"keep-me",
        4,
        false,
        Some("tech-note"),
        CleanupPolicy::Keep,
    )
    .await
    .expect("staging commit");
    let keep = store.staging_root().join("keep.bin");
    assert!(keep.exists());

    // A session-end cleanup leaves a Keep artifact in place.
    let report = store
        .perform(Operation::StagingCleanup {
            trigger: CleanupTrigger::SessionEnd,
        })
        .await
        .expect("session-end cleanup");
    match report {
        OpResult::CleanupReport { removed, .. } => assert_eq!(removed, 0, "Keep is not swept"),
        other => panic!("expected CleanupReport, got {other:?}"),
    }
    assert!(keep.exists(), "a Keep artifact survives session end");

    // An explicit All purge removes even Keep artifacts.
    let report = store
        .perform(Operation::StagingCleanup {
            trigger: CleanupTrigger::All,
        })
        .await
        .expect("all cleanup");
    match report {
        OpResult::CleanupReport { removed, .. } => assert_eq!(removed, 1, "All purges Keep too"),
        other => panic!("expected CleanupReport, got {other:?}"),
    }
    assert!(!keep.exists(), "All removed the Keep artifact");
}

#[tokio::test]
async fn staging_listing_hides_internal_entries() {
    let sb = Sandbox::new();
    let store = sb.store();
    // A committed staging artifact populates the manifest and a part directory.
    transfer(
        &store,
        staging("visible.bin"),
        b"data",
        2,
        false,
        Some("src"),
        CleanupPolicy::Keep,
    )
    .await
    .expect("staging commit");
    // A pending part in the internal directory.
    store
        .perform(Operation::FsWrite {
            dest: staging("pending.bin"),
            offset: 0,
            data: B64Bytes::new(b"half".to_vec()),
        })
        .await
        .expect("write pending");

    let listing = store
        .perform(Operation::FsList {
            path: RemotePath::Absolute {
                path: store.staging_root().to_string_lossy().into_owned(),
            },
            continue_after: None,
        })
        .await
        .expect("list staging root");
    match listing {
        OpResult::DirEntries { entries, .. } => {
            let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, vec!["visible.bin"], "manifest and .parts are hidden");
        }
        other => panic!("expected DirEntries, got {other:?}"),
    }
}

#[tokio::test]
async fn staging_reference_cannot_escape_root() {
    let sb = Sandbox::new();
    let store = sb.store();
    // A staging name must be a single safe component. Wire validation rejects a
    // traversal before it reaches the store; the resolver's own guard is the
    // defence-in-depth backstop this asserts. Every form must be refused on
    // every platform — a backslash is a separator only on Windows, so relying on
    // path semantics alone would let it through on Unix.
    for escape in [
        "..\\escape.bin",
        "../escape.bin",
        "sub/escape.bin",
        "..",
        ".",
    ] {
        let result = store
            .perform(Operation::FsStat {
                path: staging(escape),
            })
            .await;
        match result {
            Err(err) => assert_eq!(
                err.code,
                ErrorCode::OutsideStagingRoot,
                "staging name {escape:?} refused with the wrong code"
            ),
            Ok(ok) => panic!("staging name {escape:?} must be refused, not resolved: {ok:?}"),
        }
    }
}

#[tokio::test]
async fn manifest_persists_across_store_instances() {
    let sb = Sandbox::new();
    let session = SessionId::new();
    {
        let store = FileStore::with_base(session, sb.staging_base());
        transfer(
            &store,
            staging("persist.bin"),
            b"durable",
            3,
            false,
            Some("origin"),
            CleanupPolicy::OnShutdown,
        )
        .await
        .expect("commit");
    }
    // A fresh store for the same session reads the manifest written to disk.
    let reopened = FileStore::with_base(session, sb.staging_base());
    let listed = reopened
        .perform(Operation::ListStaged {})
        .await
        .expect("list staged");
    match listed {
        OpResult::StagedList { entries } => {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].source, "origin");
            assert_eq!(entries[0].cleanup, CleanupPolicy::OnShutdown);
        }
        other => panic!("expected StagedList, got {other:?}"),
    }
}

// --- startup crash-recovery sweep (ST-M003-005 AC5b) ----------------------

/// Stages one committed artifact for `session` under `base` with the given
/// cleanup policy, leaving a real file plus its manifest entry on disk — exactly
/// the state a prior run leaves behind for the next start to reclaim.
async fn stage_artifact(
    base: &Path,
    session: SessionId,
    name: &str,
    cleanup: CleanupPolicy,
) -> PathBuf {
    let store = FileStore::with_base(session, base.to_path_buf());
    transfer(
        &store,
        staging(name),
        b"leftover-bytes",
        4,
        false,
        Some("C:\\tech\\tool.zip"),
        cleanup,
    )
    .await
    .expect("staging commit");
    let artifact = store.staging_root().join(name);
    assert!(artifact.exists(), "the staged artifact is on disk");
    artifact
}

#[tokio::test]
async fn startup_sweep_removes_shutdown_artifact_and_reclaims_dir() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    let session = SessionId::new();
    let artifact = stage_artifact(&base, session, "shutdown.bin", CleanupPolicy::OnShutdown).await;

    let report = FileStore::sweep_all_sessions(base.clone(), CleanupTrigger::Shutdown).await;

    assert!(report.ok(), "a clean sweep reports no failures");
    assert_eq!(report.sessions_scanned, 1);
    assert_eq!(report.artifacts_removed, 1);
    assert_eq!(
        report.sessions_removed, 1,
        "the spent session dir is reclaimed"
    );
    assert!(!artifact.exists(), "the shutdown artifact was removed");
    assert!(
        !base.join(session.to_string()).exists(),
        "the whole session tree is gone"
    );
}

#[tokio::test]
async fn startup_sweep_removes_session_end_artifact() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    let session = SessionId::new();
    // A shutdown-strength sweep also clears artifacts that were only meant to
    // last until session end but survived a crash that skipped teardown.
    let artifact =
        stage_artifact(&base, session, "ephemeral.bin", CleanupPolicy::OnSessionEnd).await;

    let report = FileStore::sweep_all_sessions(base.clone(), CleanupTrigger::Shutdown).await;

    assert!(report.ok());
    assert_eq!(report.artifacts_removed, 1);
    assert_eq!(report.sessions_removed, 1);
    assert!(!artifact.exists(), "the session-end artifact was removed");
}

#[tokio::test]
async fn startup_sweep_keeps_keep_artifact_and_its_dir() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    let session = SessionId::new();
    let artifact = stage_artifact(&base, session, "keep.bin", CleanupPolicy::Keep).await;

    let report = FileStore::sweep_all_sessions(base.clone(), CleanupTrigger::Shutdown).await;

    assert!(report.ok());
    assert_eq!(report.sessions_scanned, 1);
    assert_eq!(report.artifacts_removed, 0, "Keep is not swept at shutdown");
    assert_eq!(report.sessions_removed, 0, "a tree with survivors is kept");
    assert!(artifact.exists(), "the Keep artifact survives");
    assert!(
        base.join(session.to_string()).exists(),
        "its session tree survives too"
    );
}

#[tokio::test]
async fn startup_sweep_ignores_foreign_directories() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    // Something else owns this: a directory whose name is not a session id, and
    // a stray file at the base. Neither is ours to touch.
    std::fs::create_dir_all(&base).expect("create base");
    let foreign_dir = base.join("not-a-session");
    std::fs::create_dir_all(&foreign_dir).expect("create foreign dir");
    let foreign_file = foreign_dir.join("keep.txt");
    std::fs::write(&foreign_file, b"not ours").expect("write foreign file");
    let stray = base.join("stray.txt");
    std::fs::write(&stray, b"not ours either").expect("write stray file");

    let report = FileStore::sweep_all_sessions(base.clone(), CleanupTrigger::Shutdown).await;

    assert!(report.ok());
    assert_eq!(report.sessions_scanned, 0, "no session tree was recognised");
    assert_eq!(report.sessions_removed, 0);
    assert!(
        foreign_dir.exists() && foreign_file.exists(),
        "foreign dir untouched"
    );
    assert!(stray.exists(), "stray file untouched");
}

#[tokio::test]
async fn startup_sweep_keeps_dir_holding_foreign_content() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    let session = SessionId::new();
    // A real session tree, but with an unexpected file dropped inside it. The
    // sweep removes the policy-eligible artifact yet must not reclaim the
    // directory, because it still holds content it did not put there.
    let artifact = stage_artifact(&base, session, "shutdown.bin", CleanupPolicy::OnShutdown).await;
    let intruder = base.join(session.to_string()).join("intruder.dat");
    std::fs::write(&intruder, b"unexpected").expect("write intruder");

    let report = FileStore::sweep_all_sessions(base.clone(), CleanupTrigger::Shutdown).await;

    assert!(report.ok(), "removing the artifact did not fail");
    assert_eq!(
        report.artifacts_removed, 1,
        "the eligible artifact was removed"
    );
    assert_eq!(
        report.sessions_removed, 0,
        "a dir with foreign content is not reclaimed"
    );
    assert!(!artifact.exists(), "the shutdown artifact is gone");
    assert!(
        intruder.exists(),
        "the foreign file is left exactly as found"
    );
    assert!(base.join(session.to_string()).exists(), "the tree is kept");
}

#[tokio::test]
async fn startup_sweep_on_missing_base_is_a_clean_no_op() {
    let sb = Sandbox::new();
    // Nothing was ever staged: the base does not exist.
    let base = sb.staging_base();
    assert!(!base.exists());

    let report = FileStore::sweep_all_sessions(base, CleanupTrigger::Shutdown).await;

    assert!(report.ok());
    assert_eq!(report.sessions_scanned, 0);
    assert_eq!(report.artifacts_removed, 0);
    assert_eq!(report.sessions_removed, 0);
}

#[tokio::test]
async fn startup_sweep_handles_several_sessions_independently() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    let spent = SessionId::new();
    let kept = SessionId::new();
    let spent_art = stage_artifact(&base, spent, "gone.bin", CleanupPolicy::OnShutdown).await;
    let kept_art = stage_artifact(&base, kept, "stay.bin", CleanupPolicy::Keep).await;

    let report = FileStore::sweep_all_sessions(base.clone(), CleanupTrigger::Shutdown).await;

    assert!(report.ok());
    assert_eq!(report.sessions_scanned, 2, "both trees were recognised");
    assert_eq!(
        report.artifacts_removed, 1,
        "only the shutdown artifact went"
    );
    assert_eq!(
        report.sessions_removed, 1,
        "only the spent tree was reclaimed"
    );
    assert!(!spent_art.exists() && !base.join(spent.to_string()).exists());
    assert!(kept_art.exists() && base.join(kept.to_string()).exists());
}

// --- cleanup-failure fault injection and retry (ST-M003-007) --------------

/// A live blockade on a staged artifact's removal, released with
/// [`release_block`]. The mechanism is platform-native and produces a genuine OS
/// refusal from the store's own `remove_file`, never a synthetic error path:
///
/// - on Windows, a held open handle opened **without** share-delete, so another
///   handle opened for `DELETE` (as `remove_file` does) hits a sharing
///   violation. A read-only attribute does not work here: the standard library's
///   `remove_file` deletes read-only files via
///   `FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE`, but it cannot bypass a
///   share-mode lock held by a live handle;
/// - elsewhere, a directory planted at the artifact path, which a file unlink
///   refuses.
#[cfg(windows)]
struct RemovalBlock(#[allow(dead_code)] std::fs::File);
#[cfg(not(windows))]
struct RemovalBlock(PathBuf);

/// Blocks `remove_file` on `path` (see [`RemovalBlock`]).
#[cfg(windows)]
fn block_removal(path: &Path) -> RemovalBlock {
    use std::os::windows::fs::OpenOptionsExt;
    // Share mode FILE_SHARE_READ only: no FILE_SHARE_DELETE, so a concurrent
    // open for DELETE fails with a sharing violation while this handle lives.
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    let handle = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
        .expect("open the artifact with a delete-denying share mode");
    RemovalBlock(handle)
}

#[cfg(not(windows))]
fn block_removal(path: &Path) -> RemovalBlock {
    std::fs::remove_file(path).expect("remove the artifact file");
    std::fs::create_dir(path).expect("plant a directory at the artifact path");
    RemovalBlock(path.to_path_buf())
}

/// Releases the blockade so a later sweep can retry the removal, restoring a
/// plain removable file at `path`.
#[cfg(windows)]
fn release_block(block: RemovalBlock, _path: &Path) {
    // Dropping the handle closes it, lifting the share-delete lock.
    drop(block);
}

#[cfg(not(windows))]
fn release_block(block: RemovalBlock, path: &Path) {
    std::fs::remove_dir(&block.0).expect("remove the planted directory");
    std::fs::write(path, b"leftover-bytes").expect("restore the artifact file");
}

#[tokio::test]
async fn cleanup_failure_is_reported_then_retried_at_next_start() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    let session = SessionId::new();
    // Stage an artifact, then block its removal so the session-end cleanup
    // genuinely cannot delete it.
    let artifact = stage_artifact(&base, session, "locked.bin", CleanupPolicy::OnSessionEnd).await;
    let block = block_removal(&artifact);

    let store = FileStore::with_base(session, base.clone());
    let report = store
        .perform(Operation::StagingCleanup {
            trigger: CleanupTrigger::SessionEnd,
        })
        .await
        .expect("cleanup runs");
    match report {
        OpResult::CleanupReport { removed, failed } => {
            assert_eq!(removed, 0, "the blocked artifact was not removed");
            assert_eq!(
                failed.len(),
                1,
                "a real removal failure is reported honestly, never as success (REQ-N-008)"
            );
            assert!(
                failed[0].canonical_path.ends_with("locked.bin"),
                "the failing artifact is named: {}",
                failed[0].canonical_path
            );
            assert!(
                !failed[0].reason.is_empty(),
                "the failure carries an honest, secret-free reason"
            );
        }
        other => panic!("expected CleanupReport, got {other:?}"),
    }

    // The record is kept for a retry: the artifact is still listed and present.
    let listed = store
        .perform(Operation::ListStaged {})
        .await
        .expect("list staged");
    match listed {
        OpResult::StagedList { entries } => assert_eq!(
            entries.len(),
            1,
            "a failed removal keeps its manifest record so a later start retries it"
        ),
        other => panic!("expected StagedList, got {other:?}"),
    }

    // Release the blockade and run the startup sweep: it retries the removal,
    // succeeds this time, and leaves nothing behind (REQ-F-019, REQ-N-008).
    release_block(block, &artifact);
    let sweep = FileStore::sweep_all_sessions(base.clone(), CleanupTrigger::Shutdown).await;
    assert!(
        sweep.ok(),
        "the retry sweep removes the previously-blocked artifact: {:?}",
        sweep.failed
    );
    assert_eq!(sweep.artifacts_removed, 1, "the retried removal succeeded");
    assert!(
        !artifact.exists(),
        "the artifact is gone after the retried sweep"
    );
    assert!(
        !base.join(session.to_string()).exists(),
        "the spent session tree is reclaimed once the residual is cleared"
    );
}

// --- read-only residual census (ST-M006-003) ------------------------------

#[tokio::test]
async fn census_on_missing_base_is_empty() {
    let sb = Sandbox::new();
    // A base that was never created: nothing was ever staged.
    let census = FileStore::census_all_sessions(sb.staging_base()).await;
    assert_eq!(census, StagingCensus::default(), "a missing base is clean");
    assert!(census.is_empty());
    assert_eq!(census.staged_count(), 0);
    assert_eq!(census.orphaned_parts(), 0);
}

#[tokio::test]
async fn census_reports_staged_records_with_owning_session_without_removing() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    let a = SessionId::new();
    let b = SessionId::new();
    let art_a = stage_artifact(&base, a, "a.bin", CleanupPolicy::OnSessionEnd).await;
    let art_b = stage_artifact(&base, b, "b.bin", CleanupPolicy::Keep).await;

    let census = FileStore::census_all_sessions(base.clone()).await;

    assert!(!census.is_empty());
    assert_eq!(census.staged_count(), 2, "both staged records are counted");
    // Each residue row carries the session that owns it (REQ-S-017).
    let sessions: Vec<SessionId> = census.sessions.iter().map(|s| s.session).collect();
    assert!(
        sessions.contains(&a) && sessions.contains(&b),
        "owning sessions named: {sessions:?}"
    );
    for row in &census.sessions {
        assert_eq!(row.staged.len(), 1, "one record per session tree");
    }

    // A census removes nothing: the residue is still exactly where it was, so a
    // customer can inspect before choosing to remove (ST-M006-003 AC3).
    assert!(
        art_a.exists() && art_b.exists(),
        "census left the residue in place"
    );
}

#[tokio::test]
async fn census_counts_orphaned_parts() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    let session = SessionId::new();
    let store = FileStore::with_base(session, base.clone());
    // An interrupted upload leaves a real part file under `.parts` — residue a
    // sweep would reclaim and a census must surface honestly.
    let parts = store.staging_root().join(".parts");
    tokio::fs::create_dir_all(&parts)
        .await
        .expect("create parts dir");
    tokio::fs::write(parts.join("upload.part"), b"partial")
        .await
        .expect("write orphaned part");

    let census = FileStore::census_all_sessions(base).await;
    assert_eq!(census.staged_count(), 0, "no committed record");
    assert_eq!(census.orphaned_parts(), 1, "the orphaned part is reported");
    assert_eq!(
        census.sessions.len(),
        1,
        "the tree holding a part is residue"
    );
    assert_eq!(census.sessions[0].session, session);
}

#[tokio::test]
async fn census_ignores_foreign_directories() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    // A non-session-id directory under the base is someone else's, never ours.
    tokio::fs::create_dir_all(base.join("not-a-session"))
        .await
        .expect("create foreign dir");
    tokio::fs::write(base.join("not-a-session").join("keep.txt"), b"theirs")
        .await
        .expect("write foreign file");

    let census = FileStore::census_all_sessions(base).await;
    assert!(
        census.is_empty(),
        "foreign directories are not CretLink residue"
    );
}

#[tokio::test]
async fn per_session_census_matches_a_single_tree() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    let session = SessionId::new();
    stage_artifact(&base, session, "one.bin", CleanupPolicy::OnShutdown).await;
    let store = FileStore::with_base(session, base);

    let staging: SessionStaging = store.census().await;
    assert_eq!(staging.session, session);
    assert_eq!(staging.staged.len(), 1);
    assert_eq!(staging.orphaned_parts, 0);
}

// --- drives, folder removal and a file sent to a folder (ST-M010-001) -------

/// A result as the session would send it: within the wire bounds.
#[track_caller]
fn assert_on_the_wire(result: &OpResult) {
    use cretlink_protocol::{Message, RequestId};
    Message::Response {
        id: RequestId(1),
        session: SessionId::new(),
        result: result.clone(),
    }
    .encode()
    .expect("the result is within the wire bounds");
}

/// The names in `dir`, sorted.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("list the folder")
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// Runs a cleanup and returns how many files it removed and what it could not.
async fn cleanup(store: &FileStore, trigger: CleanupTrigger) -> (u32, Vec<String>) {
    match store.cleanup(trigger).await.expect("cleanup runs") {
        OpResult::CleanupReport { removed, failed } => (
            removed,
            failed.into_iter().map(|f| f.canonical_path).collect(),
        ),
        other => panic!("expected CleanupReport, got {other:?}"),
    }
}

#[tokio::test]
async fn the_drives_of_this_computer_are_listed() {
    let sb = Sandbox::new();
    let result = sb
        .store()
        .perform(Operation::FsDrives {})
        .await
        .expect("the drives are listed");
    assert_on_the_wire(&result);
    match result {
        OpResult::Drives { drives } => {
            assert!(!drives.is_empty(), "a computer has at least one drive");
        }
        other => panic!("expected Drives, got {other:?}"),
    }
}

#[tokio::test]
async fn a_folder_with_its_content_is_removed_in_one_operation() {
    use cretlink_protocol::TreeState;

    let sb = Sandbox::new();
    let store = sb.store();
    let target = sb.target();
    let old = target.join("old");
    std::fs::create_dir_all(old.join("sub")).expect("create the folder");
    std::fs::write(old.join("a.txt"), b"a").expect("write");
    std::fs::write(old.join("sub").join("b.txt"), b"b").expect("write");
    std::fs::write(target.join("beside.txt"), b"keep").expect("write");

    let result = store
        .perform(Operation::RemoveTree {
            path: abs(&target, "old"),
            stop: false,
            folder_id: None,
        })
        .await
        .expect("the folder is removed");
    assert_on_the_wire(&result);
    match result {
        OpResult::TreeRemoval {
            state,
            removed_files,
            removed_dirs,
            failed,
            failed_more,
            folder_id,
        } => {
            assert_eq!(state, TreeState::Removed);
            assert_eq!((removed_files, removed_dirs), (2, 2));
            assert!(failed.is_empty());
            assert_eq!(failed_more, 0);
            assert!(folder_id.is_none(), "a removal that ended names no folder");
        }
        other => panic!("expected TreeRemoval, got {other:?}"),
    }
    assert!(!old.exists());
    assert_eq!(names_in(&target), vec!["beside.txt"]);
}

/// A folder in the staging area is named by its staging name and removed
/// with its content like any other.
#[tokio::test]
async fn a_folder_in_the_staging_area_is_removed_by_its_name() {
    use cretlink_protocol::TreeState;

    let sb = Sandbox::new();
    let store = sb.store();
    store
        .perform(Operation::CreateDir {
            path: staging("unpacked"),
        })
        .await
        .expect("create the staged folder");
    let unpacked = store.staging_root().join("unpacked");
    std::fs::create_dir(unpacked.join("inner")).expect("create");
    std::fs::write(unpacked.join("inner").join("tool.txt"), b"tool").expect("write");

    let result = store
        .perform(Operation::RemoveTree {
            path: staging("unpacked"),
            stop: false,
            folder_id: None,
        })
        .await
        .expect("the staged folder is removed");
    assert!(matches!(
        result,
        OpResult::TreeRemoval {
            state: TreeState::Removed,
            removed_files: 1,
            removed_dirs: 2,
            ..
        }
    ));
    assert!(!unpacked.exists());
    assert!(
        store.staging_root().exists(),
        "the staging area itself stays"
    );
}

/// A request that only ends a removal touches nothing, whatever the path.
#[tokio::test]
async fn a_stop_request_ends_a_removal_and_touches_nothing() {
    use cretlink_protocol::TreeState;

    let sb = Sandbox::new();
    let store = sb.store();
    let target = sb.target();
    let old = target.join("old");
    std::fs::create_dir(&old).expect("create");
    std::fs::write(old.join("a.txt"), b"a").expect("write");

    for name in ["old", "not-there"] {
        let result = store
            .perform(Operation::RemoveTree {
                path: abs(&target, name),
                stop: true,
                folder_id: None,
            })
            .await
            .expect("a stop is always answered");
        assert_on_the_wire(&result);
        assert_eq!(
            result,
            OpResult::TreeRemoval {
                state: TreeState::Stopped,
                removed_files: 0,
                removed_dirs: 0,
                failed: Vec::new(),
                failed_more: 0,
                folder_id: None,
            }
        );
    }
    assert!(old.join("a.txt").exists(), "nothing was removed");
}

/// The step of the wire operation is bounded by the protocol's own limits:
/// a folder with more entries than one step settles takes more than one
/// request, and the requests together remove all of it.
#[tokio::test]
async fn a_large_folder_takes_several_requests() {
    use cretlink_protocol::{TreeState, limits};

    let sb = Sandbox::new();
    let store = sb.store();
    let target = sb.target();
    let big = target.join("big");
    std::fs::create_dir(&big).expect("create");
    let files = limits::MAX_TREE_STEP_ENTRIES + 100;
    for n in 0..files {
        std::fs::write(big.join(format!("{n}.txt")), b"").expect("write");
    }

    let (mut removed, mut requests) = (0, 0);
    // What the step before answered as the folder's identity, given back
    // with the next request, as a caller does.
    let mut folder = None;
    loop {
        requests += 1;
        let result = store
            .perform(Operation::RemoveTree {
                path: abs(&target, "big"),
                stop: false,
                folder_id: folder.take(),
            })
            .await
            .expect("a step");
        assert_on_the_wire(&result);
        let OpResult::TreeRemoval {
            state,
            removed_files,
            removed_dirs,
            folder_id,
            ..
        } = result
        else {
            panic!("expected TreeRemoval");
        };
        assert!(removed_files + removed_dirs <= limits::MAX_TREE_STEP_ENTRIES);
        removed += removed_files + removed_dirs;
        match state {
            TreeState::More => {
                assert!(big.exists());
                assert!(folder_id.is_some(), "more to do names the folder");
                folder = folder_id;
            }
            TreeState::Removed => {
                assert!(folder_id.is_none(), "the end names no folder");
                break;
            }
            other => panic!("unexpected state {other:?}"),
        }
        assert!(requests < 1000, "the removal must end");
    }
    assert!(requests >= 2, "more entries than one step settles");
    assert_eq!(removed, files + 1, "every file and the folder itself");
    assert!(!big.exists());
}

/// A file sent to a folder is the customer's to keep: the staging manifest
/// does not record it and no cleanup removes it. The staging area stays the
/// one place for what the session cleans up.
#[tokio::test]
async fn a_file_sent_to_a_folder_is_kept_and_not_staged() {
    let sb = Sandbox::new();
    let store = sb.store();
    let documents = sb.target().join("Documents").join("Invoices");
    std::fs::create_dir_all(&documents).expect("create the folder");

    let committed = transfer(
        &store,
        abs(&documents, "invoice.pdf"),
        b"%PDF-sample",
        4,
        false,
        None,
        CleanupPolicy::OnSessionEnd,
    )
    .await
    .expect("the file is committed into the folder");
    assert!(matches!(
        committed,
        OpResult::Committed {
            len: 11,
            replaced: false,
            ..
        }
    ));
    assert_eq!(names_in(&documents), vec!["invoice.pdf"]);

    match store.perform(Operation::ListStaged {}).await.expect("list") {
        OpResult::StagedList { entries } => {
            assert!(entries.is_empty(), "a file in a folder is not staged");
        }
        other => panic!("expected StagedList, got {other:?}"),
    }
    // Not even the purge of everything staged removes it.
    for trigger in [
        CleanupTrigger::SessionEnd,
        CleanupTrigger::Shutdown,
        CleanupTrigger::All,
    ] {
        assert_eq!(cleanup(&store, trigger).await, (0, Vec::new()));
    }
    assert_eq!(
        std::fs::read(documents.join("invoice.pdf")).expect("read"),
        b"%PDF-sample"
    );
}

/// While a transfer to a folder is under way its bytes lie in one part beside
/// the destination, under a name of CretLink's own; the file's name appears
/// only with the verified commit, and the part is gone then.
#[tokio::test]
async fn the_part_lies_beside_the_destination_until_the_verified_commit() {
    let sb = Sandbox::new();
    let store = sb.store();
    let folder = sb.target();
    let dest = abs(&folder, "setup.exe");
    let payload = b"installer-bytes";

    store
        .perform(Operation::FsWrite {
            dest: dest.clone(),
            offset: 0,
            data: B64Bytes::new(payload.to_vec()),
        })
        .await
        .expect("write");
    let during = names_in(&folder);
    assert_eq!(
        during.len(),
        1,
        "one part and nothing under the file's name"
    );
    assert!(during[0].starts_with(".cretlink-part."), "{during:?}");

    store
        .perform(Operation::FsCommit {
            dest,
            len: payload.len() as u64,
            algo: DigestAlgo::Sha256,
            digest: B64Bytes::new(sha256(payload)),
            overwrite: false,
            source: None,
            cleanup: CleanupPolicy::Keep,
        })
        .await
        .expect("commit");
    assert_eq!(names_in(&folder), vec!["setup.exe"]);
}

/// A transfer to a folder that never finishes leaves its part beside the
/// destination. The session's cleanup removes that part, with every trigger,
/// and has nothing more to remove afterwards.
#[tokio::test]
async fn the_part_of_an_unfinished_transfer_to_a_folder_is_removed_by_the_cleanup() {
    let sb = Sandbox::new();
    let store = sb.store();
    let folder = sb.target();
    std::fs::write(folder.join("theirs.txt"), b"theirs").expect("write");
    store
        .perform(Operation::FsWrite {
            dest: abs(&folder, "half.bin"),
            offset: 0,
            data: B64Bytes::new(b"half of it".to_vec()),
        })
        .await
        .expect("write");
    assert_eq!(names_in(&folder).len(), 2);

    assert_eq!(
        cleanup(&store, CleanupTrigger::SessionEnd).await,
        (1, Vec::new())
    );
    assert_eq!(
        names_in(&folder),
        vec!["theirs.txt"],
        "the part is gone and nothing else in the folder was touched"
    );
    assert_eq!(
        cleanup(&store, CleanupTrigger::SessionEnd).await,
        (0, Vec::new())
    );
}

/// A transfer that was committed or discarded leaves nothing for the cleanup.
#[tokio::test]
async fn a_finished_or_discarded_transfer_leaves_nothing_for_the_cleanup() {
    let sb = Sandbox::new();
    let store = sb.store();
    let folder = sb.target();

    transfer(
        &store,
        abs(&folder, "done.bin"),
        b"done",
        2,
        false,
        None,
        CleanupPolicy::Keep,
    )
    .await
    .expect("commit");
    store
        .perform(Operation::FsWrite {
            dest: abs(&folder, "dropped.bin"),
            offset: 0,
            data: B64Bytes::new(b"dropped".to_vec()),
        })
        .await
        .expect("write");
    store
        .perform(Operation::FsAbort {
            dest: abs(&folder, "dropped.bin"),
        })
        .await
        .expect("abort");

    assert_eq!(cleanup(&store, CleanupTrigger::All).await, (0, Vec::new()));
    assert_eq!(names_in(&folder), vec!["done.bin"]);
}

/// A part the cleanup cannot remove is reported with its path, never passed
/// over, and the next cleanup tries again (REQ-F-019, REQ-N-008).
#[tokio::test]
async fn a_part_the_cleanup_cannot_remove_is_reported_and_tried_again() {
    let sb = Sandbox::new();
    let store = sb.store();
    let folder = sb.target();
    let dest = abs(&folder, "stuck.bin");
    store
        .perform(Operation::FsWrite {
            dest: dest.clone(),
            offset: 0,
            data: B64Bytes::new(b"stuck".to_vec()),
        })
        .await
        .expect("write");
    let part = store.part_path(&dest).expect("the part of this transfer");
    let block = block_removal(&part);

    let (removed, failed) = cleanup(&store, CleanupTrigger::SessionEnd).await;
    assert_eq!(removed, 0);
    assert_eq!(failed, vec![part.to_string_lossy().into_owned()]);

    release_block(block, &part);
    assert_eq!(
        cleanup(&store, CleanupTrigger::SessionEnd).await,
        (1, Vec::new())
    );
    assert!(names_in(&folder).is_empty());
}

/// The store remembers a bounded number of unfinished transfers to folders: a
/// transfer beyond it is refused before anything is written, never begun
/// untracked, and one that finishes makes room again.
#[tokio::test]
async fn unfinished_transfers_to_folders_are_bounded() {
    let sb = Sandbox::new();
    let store = sb.store();
    let folder = sb.target();
    let write = |name: String| {
        let store = store.clone();
        let dest = abs(&folder, &name);
        async move {
            store
                .perform(Operation::FsWrite {
                    dest,
                    offset: 0,
                    data: B64Bytes::new(b"x".to_vec()),
                })
                .await
        }
    };
    for n in 0..super::MAX_UNFINISHED_TRANSFERS {
        write(format!("{n}.bin")).await.expect("within the bound");
    }

    let refused = write("one-too-many.bin".to_owned())
        .await
        .expect_err("beyond the bound");
    assert_eq!(refused.code, ErrorCode::StorageExhausted);
    assert_eq!(
        names_in(&folder).len(),
        super::MAX_UNFINISHED_TRANSFERS,
        "nothing was written for the refused transfer"
    );

    // An unfinished transfer the store already knows can go on.
    store
        .perform(Operation::FsWrite {
            dest: abs(&folder, "0.bin"),
            offset: 1,
            data: B64Bytes::new(b"y".to_vec()),
        })
        .await
        .expect("a known transfer continues");
    // Discarding one makes room for a new one.
    store
        .perform(Operation::FsAbort {
            dest: abs(&folder, "1.bin"),
        })
        .await
        .expect("abort");
    write("now-it-fits.bin".to_owned())
        .await
        .expect("room again");

    let (removed, failed) = cleanup(&store, CleanupTrigger::SessionEnd).await;
    assert_eq!(removed as usize, super::MAX_UNFINISHED_TRANSFERS);
    assert!(failed.is_empty());
    assert!(names_in(&folder).is_empty());
}

/// Makes `link` a symbolic link to the file `target`. `false` when this
/// account may not create one — on Windows that takes a privilege — and the
/// caller has nothing to test.
#[cfg(any(unix, windows))]
fn plant_file_link(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let planted = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let planted = std::os::windows::fs::symlink_file(target, link);
    match planted {
        Ok(()) => true,
        Err(error) => {
            eprintln!("skipped: this account may not create symbolic links ({error})");
            false
        }
    }
}

/// A link that someone planted under the name of a transfer's part is never
/// written through and never published: the write and the commit are refused
/// and the file the link points at keeps its content. Discarding the transfer
/// removes the link, as the link it is.
#[cfg(any(unix, windows))]
#[tokio::test]
async fn a_link_planted_under_the_parts_name_is_never_written_through() {
    let sb = Sandbox::new();
    let store = sb.store();
    let folder = sb.target();
    let victim = sb.root.join("victim.txt");
    std::fs::write(&victim, b"untouched").expect("write the victim");
    let dest = abs(&folder, "tool.exe");
    let part = store.part_path(&dest).expect("the part of this transfer");
    if !plant_file_link(&victim, &part) {
        return;
    }

    let refused = store
        .perform(Operation::FsWrite {
            dest: dest.clone(),
            offset: 0,
            data: B64Bytes::new(b"payload".to_vec()),
        })
        .await
        .expect_err("a link is not a part");
    assert_eq!(refused.code, ErrorCode::WrongPathKind);

    let refused = store
        .perform(Operation::FsCommit {
            dest: dest.clone(),
            len: 9,
            algo: DigestAlgo::Sha256,
            digest: B64Bytes::new(sha256(b"untouched")),
            overwrite: true,
            source: None,
            cleanup: CleanupPolicy::Keep,
        })
        .await
        .expect_err("a link is never published as the file");
    assert_eq!(refused.code, ErrorCode::WrongPathKind);

    assert_eq!(std::fs::read(&victim).expect("read"), b"untouched");
    assert!(!folder.join("tool.exe").exists());

    store
        .perform(Operation::FsAbort { dest })
        .await
        .expect("the link is discarded");
    assert!(names_in(&folder).is_empty(), "the link itself is gone");
    assert_eq!(std::fs::read(&victim).expect("read"), b"untouched");
}

/// A folder under the name of a transfer's part is not a part either: nothing
/// is written, nothing is published, and the folder is not removed as a part.
#[tokio::test]
async fn a_folder_under_the_parts_name_is_refused() {
    let sb = Sandbox::new();
    let store = sb.store();
    let folder = sb.target();
    let dest = abs(&folder, "tool.exe");
    let part = store.part_path(&dest).expect("the part of this transfer");
    std::fs::create_dir(&part).expect("plant a folder");
    std::fs::write(part.join("theirs.txt"), b"theirs").expect("write");

    let refused = store
        .perform(Operation::FsWrite {
            dest: dest.clone(),
            offset: 0,
            data: B64Bytes::new(b"payload".to_vec()),
        })
        .await
        .expect_err("a folder is not a part");
    assert_eq!(refused.code, ErrorCode::WrongPathKind);
    let refused = store
        .perform(Operation::FsCommit {
            dest: dest.clone(),
            len: 0,
            algo: DigestAlgo::Sha256,
            digest: B64Bytes::new(sha256(b"")),
            overwrite: true,
            source: None,
            cleanup: CleanupPolicy::Keep,
        })
        .await
        .expect_err("a folder is never published as the file");
    assert_eq!(refused.code, ErrorCode::WrongPathKind);
    store
        .perform(Operation::FsAbort { dest })
        .await
        .expect_err("a folder is not discarded as a part");

    assert!(part.join("theirs.txt").exists(), "the folder is untouched");
    assert!(!folder.join("tool.exe").exists());
}

/// A file without content is committed without a chunk: the commit makes its
/// empty part itself. When such a commit is refused, the part it made is
/// removed again and forgotten — it holds nothing to resume. Otherwise every
/// refused commit would leave an empty part in the customer's folder and one
/// more remembered transfer, until no transfer could begin at all.
#[tokio::test]
async fn a_refused_commit_of_an_empty_file_leaves_no_part_and_no_memory_of_one() {
    let sb = Sandbox::new();
    let store = sb.store();
    let folder = sb.target();
    let commit_empty = |name: String, digest: Vec<u8>| {
        let store = store.clone();
        let dest = abs(&folder, &name);
        async move {
            store
                .perform(Operation::FsCommit {
                    dest,
                    len: 0,
                    algo: DigestAlgo::Sha256,
                    digest: B64Bytes::new(digest),
                    overwrite: false,
                    source: None,
                    cleanup: CleanupPolicy::Keep,
                })
                .await
        }
    };

    // More refused commits than the store remembers unfinished transfers:
    // each declares a digest that is not that of an empty file.
    for n in 0..super::MAX_UNFINISHED_TRANSFERS + 8 {
        let refused = commit_empty(format!("empty-{n}.txt"), sha256(b"not empty"))
            .await
            .expect_err("the digest is not that of an empty file");
        assert_eq!(refused.code, ErrorCode::IntegrityMismatch, "commit {n}");
    }
    assert!(
        names_in(&folder).is_empty(),
        "no refused commit left its part behind"
    );

    // A name that is taken refuses the commit as well, and leaves nothing.
    std::fs::write(folder.join("taken.txt"), b"theirs").expect("write");
    let refused = commit_empty("taken.txt".to_owned(), sha256(b""))
        .await
        .expect_err("the name is taken");
    assert_eq!(refused.code, ErrorCode::AlreadyExists);
    assert_eq!(names_in(&folder), vec!["taken.txt"]);
    assert_eq!(
        std::fs::read(folder.join("taken.txt")).expect("read"),
        b"theirs"
    );

    // Nothing is remembered: a transfer can begin, and an empty file is
    // committed.
    let committed = commit_empty("empty.txt".to_owned(), sha256(b""))
        .await
        .expect("an empty file is committed without a chunk");
    assert!(matches!(
        committed,
        OpResult::Committed {
            len: 0,
            replaced: false,
            ..
        }
    ));
    assert_eq!(std::fs::read(folder.join("empty.txt")).expect("read"), b"");
    assert_eq!(names_in(&folder), vec!["empty.txt", "taken.txt"]);
    assert_eq!(
        cleanup(&store, CleanupTrigger::SessionEnd).await,
        (0, Vec::new()),
        "and nothing is left for the cleanup"
    );
}

/// Beneath that check, the part is opened in a way that refuses a link in the
/// last part of its name, so a link swapped in after the check is not written
/// through either.
#[cfg(unix)]
#[tokio::test]
async fn the_part_is_opened_without_following_a_link() {
    let sb = Sandbox::new();
    let folder = sb.target();
    let victim = folder.join("victim.txt");
    std::fs::write(&victim, b"untouched").expect("write the victim");
    let link = folder.join("part");
    std::os::unix::fs::symlink(&victim, &link).expect("plant the link");

    super::append_to_part(&link, b"payload")
        .await
        .expect_err("the link is not opened");
    assert_eq!(std::fs::read(&victim).expect("read"), b"untouched");

    // A link to nothing is not followed into creating its target either.
    let dangling = folder.join("dangling");
    std::os::unix::fs::symlink(folder.join("created.txt"), &dangling).expect("plant");
    super::append_to_part(&dangling, b"payload")
        .await
        .expect_err("the link is not opened");
    assert!(!folder.join("created.txt").exists());
}

/// A disk that is full refuses the bytes of a part, and the transfer is told
/// so in the storage category instead of a nameless failure. Linux only: the
/// part is `/dev/full`, the device whose every write fails with "no space
/// left", so no disk has to be filled.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_full_disk_is_reported_as_exhausted_storage() {
    for data in [vec![7u8; 1], vec![7u8; 256 * 1024]] {
        let refused = super::append_to_part(Path::new("/dev/full"), &data)
            .await
            .expect_err("a full disk takes no bytes");
        assert_eq!(refused.code, ErrorCode::StorageExhausted, "{refused:?}");
    }
}

/// The operating system's reasons are told apart in the wire vocabulary on
/// every platform, from the kind of the error rather than from a number that
/// means something else on another system.
#[test]
fn io_errors_are_classified_by_their_kind() {
    use std::io::{Error, ErrorKind};

    for (kind, code) in [
        (ErrorKind::NotFound, ErrorCode::PathNotFound),
        (ErrorKind::PermissionDenied, ErrorCode::AccessDenied),
        (ErrorKind::AlreadyExists, ErrorCode::AlreadyExists),
        (ErrorKind::DirectoryNotEmpty, ErrorCode::DirNotEmpty),
        (ErrorKind::NotADirectory, ErrorCode::WrongPathKind),
        (ErrorKind::IsADirectory, ErrorCode::WrongPathKind),
        (ErrorKind::CrossesDevices, ErrorCode::Unsupported),
        (ErrorKind::StorageFull, ErrorCode::StorageExhausted),
        (ErrorKind::QuotaExceeded, ErrorCode::StorageExhausted),
        (ErrorKind::ResourceBusy, ErrorCode::FileInUse),
        (ErrorKind::ExecutableFileBusy, ErrorCode::FileInUse),
        (ErrorKind::Other, ErrorCode::Internal),
    ] {
        assert_eq!(super::io_error_code(&Error::from(kind)), code, "{kind:?}");
    }
}

/// Creating a folder that is already there is "already exists" on every
/// platform (off Windows the system's number for it is the one Windows uses
/// for "not the same device").
#[tokio::test]
async fn creating_a_folder_that_exists_is_already_exists() {
    let sb = Sandbox::new();
    let store = sb.store();
    let target = sb.target();
    store
        .perform(Operation::CreateDir {
            path: abs(&target, "twice"),
        })
        .await
        .expect("first");
    let error = store
        .perform(Operation::CreateDir {
            path: abs(&target, "twice"),
        })
        .await
        .expect_err("second");
    assert_eq!(error.code, ErrorCode::AlreadyExists);
}

/// What a cleanup could not remove is named by a path that the wire carries,
/// whatever the name on the disk is: cut to the length of a path, and on one
/// line. The answer of a cleanup can therefore always be sent.
#[test]
fn a_cleanup_failure_names_its_path_within_the_wire_bounds() {
    use cretlink_protocol::limits::MAX_PATH_BYTES;

    let named = super::cleanup_failure("C:\\Temp\\locked.bin", "in use");
    assert_eq!(named.canonical_path, "C:\\Temp\\locked.bin");
    assert_eq!(named.reason, "in use");

    let long = format!("/tmp/{}", "n".repeat(2 * MAX_PATH_BYTES));
    let broken = "/tmp/first\nsecond.part";
    for path in [long.as_str(), broken] {
        let failure = super::cleanup_failure(path, "in use");
        assert!(failure.canonical_path.len() <= MAX_PATH_BYTES);
        assert!(!failure.canonical_path.contains('\n'));
        assert_on_the_wire(&OpResult::CleanupReport {
            removed: 0,
            failed: vec![failure],
        });
    }
}

#[test]
fn a_text_for_the_wire_is_kept_on_one_line_and_within_its_bound() {
    use super::one_line;

    assert_eq!(one_line("C:\\Temp\\report.txt", 64), "C:\\Temp\\report.txt");
    // A line break, a tab and a line separator are each replaced.
    assert_eq!(
        one_line("a\nb\tc\u{2028}d", 64),
        "a\u{FFFD}b\u{FFFD}c\u{FFFD}d"
    );
    // Exactly at the bound is whole; beyond it is cut and marked.
    assert_eq!(one_line("abcdefgh", 8), "abcdefgh");
    let cut = one_line("abcdefghi", 8);
    assert_eq!(cut, "abcde…");
    assert!(cut.len() <= 8);
    // The cut never splits a character.
    let cut = one_line(&"é".repeat(10), 8);
    assert_eq!(cut, "éé…");
    assert!(cut.len() <= 8);
}

// --- Windows only: a file sent to a folder stays in the folder that was
// --- opened, a held destination, a drive root and another drive
// --- (ST-M010-001) ----------------------------------------------------------

/// The request of one chunk for `dest`.
fn chunk(dest: &RemotePath, offset: u64, bytes: &[u8]) -> Operation {
    Operation::FsWrite {
        dest: dest.clone(),
        offset,
        data: B64Bytes::new(bytes.to_vec()),
    }
}

/// The request that commits `bytes` as the file `dest`.
fn commit_of(dest: &RemotePath, bytes: &[u8], overwrite: bool) -> Operation {
    Operation::FsCommit {
        dest: dest.clone(),
        len: bytes.len() as u64,
        algo: DigestAlgo::Sha256,
        digest: B64Bytes::new(sha256(bytes)),
        overwrite,
        source: None,
        cleanup: CleanupPolicy::Keep,
    }
}

/// A junction in the place of the folder a file is sent to is not that
/// folder: the file would go where the junction leads. Every step of a
/// transfer refuses it — the chunk, the commit and the discard — and nothing
/// is written where it leads.
#[cfg(windows)]
#[tokio::test]
async fn a_junction_in_the_place_of_the_folder_is_refused_by_every_step_of_a_transfer() {
    use super::testing::junction;

    let sb = Sandbox::new();
    let store = sb.store();
    let target = sb.target();
    let real = target.join("real");
    std::fs::create_dir(&real).expect("create");
    let link = target.join("link");
    junction(&link, &real);
    let dest = abs(&link, "tool.exe");

    for step in [
        chunk(&dest, 0, b"payload"),
        commit_of(&dest, b"", true),
        Operation::FsAbort { dest: dest.clone() },
    ] {
        let refused = store
            .perform(step.clone())
            .await
            .expect_err("a link is not the folder");
        assert_eq!(refused.code, ErrorCode::InvalidPath, "{step:?}");
    }
    assert!(
        names_in(&real).is_empty(),
        "nothing was written where the junction leads"
    );
    // Nothing was begun, so nothing is left for the cleanup.
    assert_eq!(
        cleanup(&store, CleanupTrigger::SessionEnd).await,
        (0, Vec::new())
    );

    // A junction further up in the path is refused the same way.
    std::fs::create_dir(real.join("inside")).expect("create");
    let through = abs(&link.join("inside"), "tool.exe");
    let refused = store
        .perform(chunk(&through, 0, b"payload"))
        .await
        .expect_err("a path through a link is not the folder");
    assert_eq!(refused.code, ErrorCode::InvalidPath);
    assert!(names_in(&real.join("inside")).is_empty());
}

/// A transfer has begun in a folder. Before its next request the folder is
/// moved away and a junction to another place takes its name — with a file
/// under the name of the transfer's part put there, so that a transfer that
/// resolved the path again would find everything it looks for. Nothing is
/// written, published or removed in that place: every step refuses, and the
/// cleanup reports the part as not removed and keeps it in mind.
#[cfg(windows)]
#[tokio::test]
async fn a_folder_swapped_for_a_junction_gets_nothing_published_or_removed_in_its_target() {
    use super::testing::junction;

    let sb = Sandbox::new();
    let store = sb.store();
    let target = sb.target();
    let folder = target.join("Downloads");
    let elsewhere = target.join("elsewhere");
    std::fs::create_dir(&folder).expect("create");
    std::fs::create_dir(&elsewhere).expect("create");
    let dest = abs(&folder, "setup.exe");
    let payload = b"installer-bytes";
    let (first, rest) = payload.split_at(5);

    store
        .perform(chunk(&dest, 0, first))
        .await
        .expect("the first chunk");
    let part = store.part_path(&dest).expect("the part of this transfer");
    let part_name = part.file_name().expect("the part has a name").to_owned();

    let moved = target.join("Downloads-moved");
    std::fs::rename(&folder, &moved).expect("move the folder away");
    junction(&folder, &elsewhere);
    let planted = elsewhere.join(&part_name);
    std::fs::rename(moved.join(&part_name), &planted).expect("put the part there");

    // The next chunk, and a commit of exactly what lies there.
    for step in [
        chunk(&dest, 5, rest),
        commit_of(&dest, first, true),
        Operation::FsAbort { dest: dest.clone() },
    ] {
        let refused = store
            .perform(step.clone())
            .await
            .expect_err("the folder is a link now");
        assert_eq!(refused.code, ErrorCode::InvalidPath, "{step:?}");
    }
    assert_eq!(
        names_in(&elsewhere),
        vec![part_name.to_string_lossy().into_owned()],
        "nothing was published there and the file under the part's name stays"
    );
    assert_eq!(std::fs::read(&planted).expect("read"), first, "unchanged");

    // The cleanup does not follow the junction either: the part is reported
    // as not removed, and what lies where the junction leads stays.
    let (removed, failed) = cleanup(&store, CleanupTrigger::SessionEnd).await;
    assert_eq!(removed, 0);
    assert_eq!(failed, vec![part.to_string_lossy().into_owned()]);
    assert!(planted.exists());

    // Everything is put back. The store has kept the part in mind: the
    // transfer goes on where it was and is committed.
    std::fs::remove_dir(&folder).expect("remove the junction");
    std::fs::rename(&planted, moved.join(&part_name)).expect("put the part back");
    std::fs::rename(&moved, &folder).expect("move the folder back");
    store
        .perform(chunk(&dest, 5, rest))
        .await
        .expect("the next chunk");
    store
        .perform(commit_of(&dest, payload, false))
        .await
        .expect("the commit");
    assert_eq!(names_in(&folder), vec!["setup.exe"]);
    assert_eq!(
        std::fs::read(folder.join("setup.exe")).expect("read"),
        payload
    );
    assert!(names_in(&elsewhere).is_empty());
    assert_eq!(
        cleanup(&store, CleanupTrigger::SessionEnd).await,
        (0, Vec::new())
    );
}

/// A junction under the name of a transfer's part is a link, not a part:
/// nothing is written or published through it, and discarding the transfer
/// removes the junction itself and nothing where it leads. A junction needs
/// no privilege, so this runs wherever Windows does.
#[cfg(windows)]
#[tokio::test]
async fn a_junction_under_the_parts_name_is_never_written_through() {
    use super::testing::junction;

    let sb = Sandbox::new();
    let store = sb.store();
    let folder = sb.target();
    let elsewhere = sb.root.join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("create");
    std::fs::write(elsewhere.join("theirs.txt"), b"theirs").expect("write");
    let dest = abs(&folder, "tool.exe");
    let part = store.part_path(&dest).expect("the part of this transfer");
    junction(&part, &elsewhere);

    for step in [chunk(&dest, 0, b"payload"), commit_of(&dest, b"", true)] {
        let refused = store
            .perform(step.clone())
            .await
            .expect_err("a link is not a part");
        assert_eq!(refused.code, ErrorCode::WrongPathKind, "{step:?}");
    }
    assert_eq!(names_in(&elsewhere), vec!["theirs.txt"]);

    store
        .perform(Operation::FsAbort { dest })
        .await
        .expect("the junction is discarded");
    assert!(names_in(&folder).is_empty(), "the junction itself is gone");
    assert_eq!(
        std::fs::read(elsewhere.join("theirs.txt")).expect("read"),
        b"theirs",
        "and what it led to is untouched"
    );
}

/// The part of a transfer has exactly one name. A part that was given a
/// second name somewhere else, and a file from somewhere else that was given
/// a second name where a part is expected, are the same file under two
/// names: writing to one or publishing it would reach the other. Neither is
/// written to and neither is published.
#[cfg(windows)]
#[tokio::test]
async fn a_part_with_a_second_name_is_never_written_to_and_never_published() {
    let sb = Sandbox::new();
    let store = sb.store();
    let folder = sb.target();

    // The part of a running transfer is given a second name.
    let dest = abs(&folder, "report.docx");
    store
        .perform(chunk(&dest, 0, b"first"))
        .await
        .expect("the first chunk");
    let part = store.part_path(&dest).expect("the part of this transfer");
    let alias = sb.root.join("alias.bin");
    std::fs::hard_link(&part, &alias).expect("give the part a second name");
    for step in [chunk(&dest, 5, b"more"), commit_of(&dest, b"first", false)] {
        let refused = store
            .perform(step.clone())
            .await
            .expect_err("a part with two names");
        assert_eq!(refused.code, ErrorCode::WrongPathKind, "{step:?}");
    }
    assert_eq!(std::fs::read(&alias).expect("read"), b"first", "unchanged");
    assert!(!folder.join("report.docx").exists());
    // With the second name gone the part is the transfer's own again.
    std::fs::remove_file(&alias).expect("remove the second name");
    store
        .perform(chunk(&dest, 5, b"more"))
        .await
        .expect("the next chunk");
    store
        .perform(commit_of(&dest, b"firstmore", false))
        .await
        .expect("the commit");
    assert_eq!(
        std::fs::read(folder.join("report.docx")).expect("read"),
        b"firstmore"
    );

    // A file from elsewhere is given a second name where a part is expected.
    let victim = sb.root.join("victim.txt");
    std::fs::write(&victim, b"untouched").expect("write the victim");
    let dest = abs(&folder, "tool.exe");
    let part = store.part_path(&dest).expect("the part of this transfer");
    std::fs::hard_link(&victim, &part).expect("plant the second name");
    // The offset and the digest are those of the victim, so that nothing but
    // the second name stands in the way.
    for step in [
        chunk(&dest, 9, b"appended"),
        commit_of(&dest, b"untouched", true),
    ] {
        let refused = store
            .perform(step.clone())
            .await
            .expect_err("a file with two names is not a part");
        assert_eq!(refused.code, ErrorCode::WrongPathKind, "{step:?}");
    }
    assert_eq!(std::fs::read(&victim).expect("read"), b"untouched");
    assert!(!folder.join("tool.exe").exists());
}

/// A commit of a large file takes a while, and the session can end in the
/// meantime. A commit that nobody waits for any more is told so and publishes
/// nothing: what happens after a session has ended is not the session's
/// doing. The part stays as it is.
///
/// The commit asks whether somebody still waits while it reads the part, and
/// once more before the file gets its name. The test first counts the
/// questions of a commit that goes through, and then answers "nobody" to the
/// last question alone — the one before the name — and to the first.
#[cfg(windows)]
#[tokio::test]
async fn a_commit_that_nobody_waits_for_any_more_publishes_nothing() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let sb = Sandbox::new();
    let store = sb.store();
    let folder = sb.target();
    let place_of = |dest: &RemotePath| {
        store
            .place_in_folder(dest)
            .expect("the destination resolves")
            .expect("it lies in a folder")
    };

    let counted = abs(&folder, "counted.iso");
    store
        .perform(chunk(&counted, 0, b"content"))
        .await
        .expect("the chunk");
    let asked = AtomicUsize::new(0);
    super::to_folder::commit(
        &store,
        &place_of(&counted),
        7,
        &sha256(b"content"),
        false,
        &|| {
            asked.fetch_add(1, Ordering::SeqCst);
            false
        },
    )
    .expect("a commit that somebody waits for");
    let questions = asked.load(Ordering::SeqCst);
    assert!(
        questions >= 2,
        "asked while reading and before the name: {questions}"
    );

    let dest = abs(&folder, "big.iso");
    store
        .perform(chunk(&dest, 0, b"content"))
        .await
        .expect("the chunk");
    let place = place_of(&dest);
    for nobody_from in [questions, 1] {
        let asked = AtomicUsize::new(0);
        let abandoned =
            super::to_folder::commit(&store, &place, 7, &sha256(b"content"), false, &|| {
                asked.fetch_add(1, Ordering::SeqCst) + 1 >= nobody_from
            })
            .expect_err("an abandoned commit does not go through");
        assert_eq!(abandoned.code, ErrorCode::Internal);
        assert!(!folder.join("big.iso").exists(), "nothing was published");
        assert_eq!(names_in(&folder).len(), 2, "the part is still there");
    }

    // Asked for again by a session that waits, the commit goes through.
    store
        .perform(commit_of(&dest, b"content", false))
        .await
        .expect("the commit");
    assert_eq!(names_in(&folder), vec!["big.iso", "counted.iso"]);
}

/// A file is committed into the root of a drive: the root is a folder like
/// any other, opened and verified as one. The test needs the right to write
/// there; an account without it has nothing to test.
#[cfg(windows)]
#[tokio::test]
async fn a_file_is_committed_into_the_root_of_a_drive() {
    let sb = Sandbox::new();
    let store = sb.store();
    let root = PathBuf::from(&sb.root.to_string_lossy()[..3]);
    let name = format!("cretlink-fs-test-{}-root.bin", std::process::id());
    let probe = root.join(format!("{name}.probe"));
    if std::fs::write(&probe, b"").is_err() {
        eprintln!("skipped: this account may not write to the root of the drive");
        return;
    }
    let _ = std::fs::remove_file(&probe);
    let parts_in_the_root = || {
        names_in(&root)
            .into_iter()
            .filter(|entry| entry.starts_with(".cretlink-part."))
            .count()
    };
    let parts_before = parts_in_the_root();

    let committed = transfer(
        &store,
        abs(&root, &name),
        b"in the root",
        4,
        false,
        None,
        CleanupPolicy::Keep,
    )
    .await;
    let on_disk = std::fs::read(root.join(&name));
    let parts_left = parts_in_the_root().saturating_sub(parts_before);
    let _ = std::fs::remove_file(root.join(&name));

    match committed {
        Ok(OpResult::Committed {
            canonical_path,
            len: 11,
            ..
        }) => assert!(
            canonical_path.eq_ignore_ascii_case(&root.join(&name).to_string_lossy()),
            "{canonical_path}"
        ),
        other => panic!("expected Committed, got {other:?}"),
    }
    assert_eq!(on_disk.expect("read the committed file"), b"in the root");
    assert_eq!(parts_left, 0, "no part is left beside it");
}

/// The folder may be spelled in another letter case or by its short name: it
/// is still the place where the folder lies, and the file is committed there.
/// The answer names the file by where it really lies.
#[cfg(windows)]
#[tokio::test]
async fn a_file_is_committed_into_a_folder_spelled_by_a_short_name_or_in_another_letter_case() {
    use super::testing::{reported, short_name};

    let sb = Sandbox::new();
    let store = sb.store();
    let folder = sb.target().join("Long Folder Name");
    std::fs::create_dir(&folder).expect("create");
    let real = reported(&folder);
    let text = folder.to_string_lossy().into_owned();

    let mut spellings = vec![text.to_uppercase(), text.to_lowercase()];
    match short_name(&folder) {
        Some(short) => spellings.push(short),
        None => eprintln!("skipped the short name: this volume keeps none"),
    }
    for (n, spelled) in spellings.iter().enumerate() {
        let name = format!("file-{n}.txt");
        let committed = transfer(
            &store,
            abs(Path::new(spelled), &name),
            b"content",
            3,
            false,
            None,
            CleanupPolicy::Keep,
        )
        .await
        .unwrap_or_else(|error| panic!("{spelled} is refused: {error:?}"));
        match committed {
            OpResult::Committed { canonical_path, .. } => {
                assert_eq!(canonical_path, format!("{real}\\{name}"), "{spelled}");
            }
            other => panic!("expected Committed, got {other:?}"),
        }
        assert_eq!(
            std::fs::read(folder.join(&name)).expect("read"),
            b"content",
            "{spelled}"
        );
    }
    assert_eq!(
        names_in(&folder).len(),
        spellings.len(),
        "the files and no part"
    );
}

/// An existing file that another program holds open is not replaced, even on
/// request: the commit says that the file is in use, the file keeps its
/// content, and the verified part stays so the commit can be repeated once the
/// file is released.
#[cfg(windows)]
#[tokio::test]
async fn a_destination_held_open_by_another_program_is_not_replaced() {
    let sb = Sandbox::new();
    let store = sb.store();
    let folder = sb.target();
    let existing = folder.join("report.docx");
    std::fs::write(&existing, b"the old one").expect("write");
    let dest = abs(&folder, "report.docx");
    let payload = b"the new one";
    let commit = || Operation::FsCommit {
        dest: dest.clone(),
        len: payload.len() as u64,
        algo: DigestAlgo::Sha256,
        digest: B64Bytes::new(sha256(payload)),
        overwrite: true,
        source: None,
        cleanup: CleanupPolicy::Keep,
    };
    store
        .perform(Operation::FsWrite {
            dest: dest.clone(),
            offset: 0,
            data: B64Bytes::new(payload.to_vec()),
        })
        .await
        .expect("write");

    let block = block_removal(&existing);
    let refused = store
        .perform(commit())
        .await
        .expect_err("a held file is not replaced");
    assert_eq!(refused.code, ErrorCode::FileInUse);
    assert_eq!(std::fs::read(&existing).expect("read"), b"the old one");

    release_block(block, &existing);
    let committed = store.perform(commit()).await.expect("the commit repeats");
    assert!(matches!(
        committed,
        OpResult::Committed { replaced: true, .. }
    ));
    assert_eq!(std::fs::read(&existing).expect("read"), b"the new one");
    assert_eq!(names_in(&folder), vec!["report.docx"]);
}

/// A file is committed into a folder on another drive than the one that
/// holds the staging area: the part lies beside the destination, so the
/// verified rename never leaves that drive. The test needs a second fixed
/// drive it may write to; a computer without one has nothing to test.
#[cfg(windows)]
#[tokio::test]
async fn a_file_is_committed_into_a_folder_on_another_drive() {
    use cretlink_protocol::DriveKind;

    let sb = Sandbox::new();
    let store = sb.store();
    let here = sb.root.to_string_lossy()[..3].to_ascii_uppercase();
    let folder = super::drives::list()
        .into_iter()
        .filter(|drive| drive.kind == DriveKind::Fixed && drive.root != here)
        .find_map(|drive| {
            let folder =
                PathBuf::from(&drive.root).join(format!("cretlink-fs-test-{}", std::process::id()));
            std::fs::create_dir_all(&folder).ok().map(|()| folder)
        });
    let Some(folder) = folder else {
        eprintln!("skipped: no second fixed drive that this account may write to");
        return;
    };

    let committed = transfer(
        &store,
        abs(&folder, "driver.inf"),
        b"[Version]",
        4,
        false,
        None,
        CleanupPolicy::Keep,
    )
    .await;
    let on_disk = std::fs::read(folder.join("driver.inf"));
    let left = names_in(&folder);
    let _ = std::fs::remove_dir_all(&folder);

    assert!(
        matches!(committed, Ok(OpResult::Committed { len: 9, .. })),
        "{committed:?}"
    );
    assert_eq!(on_disk.expect("read the committed file"), b"[Version]");
    assert_eq!(left, vec!["driver.inf"], "no part is left beside it");
}

// --- the staging area is CretLink's own (ST-M006-020, REQ-S-024) -------------

use cretlink_protocol::StagedEntry;

use crate::own::OwnFolder;

/// One manifest record that names `path`, as a cleanup selects it at the end
/// of a session.
fn record(path: &Path) -> StagedEntry {
    StagedEntry {
        canonical_path: path.to_string_lossy().into_owned(),
        source: "x".to_string(),
        len: 0,
        algo: DigestAlgo::Sha256,
        digest: B64Bytes::new(vec![0u8; 32]),
        cleanup: CleanupPolicy::OnSessionEnd,
        created_ms: 0,
    }
}

/// Writes a manifest with `records` into the session folder `folder`.
fn write_manifest(folder: &Path, records: &[StagedEntry]) {
    std::fs::create_dir_all(folder).expect("create the session folder");
    let manifest = serde_json::json!({ "version": 1, "staged": records });
    std::fs::write(
        folder.join("manifest.json"),
        serde_json::to_vec(&manifest).expect("json"),
    )
    .expect("write the manifest");
}

fn failures(report: &super::StartupSweepReport) -> Vec<(String, String)> {
    report
        .failed
        .iter()
        .map(|failure| (failure.canonical_path.clone(), failure.reason.clone()))
        .collect()
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// What an account without rights could put under the staging base: a folder
/// named like a session with a manifest that names a file elsewhere. The file
/// stays, whatever sweep runs.
#[tokio::test]
async fn a_planted_manifest_removes_nothing_outside_the_staging_area() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    let victim = sb.target().join("important.txt");
    std::fs::write(&victim, b"keep me").expect("write");
    let planted = base.join(SessionId::new().to_string());
    write_manifest(&planted, &[record(&victim)]);

    let report = FileStore::sweep_all_sessions(base.clone(), CleanupTrigger::Shutdown).await;

    assert_eq!(std::fs::read(&victim).expect("the file stays"), b"keep me");
    assert_eq!(report.artifacts_removed, 0);
    assert_eq!(report.sessions_removed, 0);
    assert_eq!(
        failures(&report),
        vec![(
            text(&victim),
            "outside the staging area; not touched".to_string()
        )]
    );
    assert!(planted.exists(), "the record is kept and named again");
    let again = FileStore::sweep_all_sessions(base.clone(), CleanupTrigger::Shutdown).await;
    assert_eq!(failures(&again), failures(&report));

    // The purge does not touch the file either. It drops the entry, says so,
    // and the next purge finds a spent folder.
    let purge = FileStore::sweep_all_sessions(base.clone(), CleanupTrigger::All).await;
    assert_eq!(std::fs::read(&victim).expect("the file stays"), b"keep me");
    assert_eq!(
        failures(&purge),
        vec![(
            text(&victim),
            "outside the staging area; not touched, entry dropped".to_string()
        )]
    );
    assert!(
        purge.failed[0]
            .reason
            .starts_with(super::OUTSIDE_STAGING_REASON)
    );
    let last = FileStore::sweep_all_sessions(base, CleanupTrigger::All).await;
    assert!(last.ok(), "{:?}", last.failed);
    assert_eq!(last.sessions_removed, 1);
    assert!(!planted.exists());
    assert!(victim.exists());
}

/// The manifest that the manual test MT-4b has a tester plant by hand
/// (`docs/manual-tests.md`) is one a sweep would read if it lay in the staging
/// area: it is then the entry it names that the sweep refuses, not the
/// manifest.
#[tokio::test]
async fn the_manifest_of_the_manual_test_is_read_and_its_entry_refused() {
    const BY_HAND: &str = r#"{"version":1,"staged":[{"canonical_path":"C:\\cretlink-test\\important.txt","source":"x","len":0,"algo":"sha256","digest":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","cleanup":"on_session_end","created_ms":0}]}"#;
    let sb = Sandbox::new();
    let base = sb.staging_base();
    let planted = base.join("11111111-2222-4333-8444-555555555555");
    std::fs::create_dir_all(&planted).expect("create");
    std::fs::write(planted.join("manifest.json"), BY_HAND).expect("write");

    let report = FileStore::sweep_all_sessions(base, CleanupTrigger::Shutdown).await;

    assert_eq!(report.sessions_scanned, 1);
    assert_eq!(
        failures(&report),
        vec![(
            "C:\\cretlink-test\\important.txt".to_string(),
            "outside the staging area; not touched".to_string()
        )]
    );
}

/// A cleanup removes an entry only when it names a file directly in the
/// session's own staging folder.
#[tokio::test]
async fn a_cleanup_removes_only_what_lies_directly_in_its_staging_folder() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    let session = SessionId::new();
    let ours = stage_artifact(&base, session, "ours.bin", CleanupPolicy::OnSessionEnd).await;
    let root = base.join(session.to_string());
    // Another session's artifact, a file below a folder of this session, and
    // this session's own bookkeeping.
    let other_session = SessionId::new();
    let theirs = stage_artifact(&base, other_session, "theirs.bin", CleanupPolicy::Keep).await;
    let nested = root.join("sub").join("deep.bin");
    std::fs::create_dir_all(nested.parent().expect("parent")).expect("create");
    std::fs::write(&nested, b"deep").expect("write");
    let outside = sb.target().join("outside.bin");
    std::fs::write(&outside, b"outside").expect("write");
    let through_parent = root
        .join("..")
        .join(other_session.to_string())
        .join("theirs.bin");
    let bogus = [
        theirs.clone(),
        nested.clone(),
        outside.clone(),
        through_parent.clone(),
        root.join("manifest.json"),
        root.join(".parts"),
        root.clone(),
        base.clone(),
    ];
    let mut records = vec![record(&ours)];
    records.extend(bogus.iter().map(|path| record(path)));
    write_manifest(&root, &records);
    let store = FileStore::with_base(session, base.clone());

    let report = store
        .cleanup(CleanupTrigger::SessionEnd)
        .await
        .expect("cleanup");

    let OpResult::CleanupReport { removed, failed } = report else {
        panic!("expected CleanupReport");
    };
    assert_eq!(removed, 1, "only this session's own artifact");
    assert!(!ours.exists());
    let named: Vec<String> = failed.iter().map(|f| f.canonical_path.clone()).collect();
    let expected: Vec<String> = bogus.iter().map(|path| text(path)).collect();
    assert_eq!(named, expected);
    assert!(
        failed
            .iter()
            .all(|f| f.reason == "outside the staging area; not touched")
    );
    for kept in [&theirs, &nested, &outside] {
        assert!(kept.exists(), "{}", kept.display());
    }
    assert!(root.join("manifest.json").exists());
    assert!(root.join(".parts").is_dir());
}

/// How a manifest spells the folders above the session's folder does not
/// matter: an entry that ends in the session's folder and a file name stands
/// for that file of this session, wherever its spelling seems to lead, and
/// nothing is removed at the path it spells.
#[tokio::test]
async fn an_entry_stands_for_the_file_in_the_sessions_own_folder_however_it_is_spelled() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    let session = SessionId::new();
    let root = base.join(session.to_string());
    let names = ["a.bin", "b.bin", "c.bin", "d.bin"];
    for name in names {
        stage_artifact(&base, session, name, CleanupPolicy::OnSessionEnd).await;
    }
    // A file of the same name lies where one of the spellings leads.
    let decoy_folder = sb.target().join(session.to_string());
    std::fs::create_dir_all(&decoy_folder).expect("create");
    let decoy = decoy_folder.join("d.bin");
    std::fs::write(&decoy, b"not the session's").expect("write");
    let upper = session.to_string().to_uppercase();
    let spelled = [
        format!("C:\\PROGRA~3\\CretLink\\staging\\{session}\\a.bin"),
        format!("c:/programdata/cretlink/STAGING/{upper}/b.bin"),
        format!("{session}/c.bin"),
        text(&decoy),
    ];
    let records: Vec<StagedEntry> = spelled.iter().map(|path| record(Path::new(path))).collect();
    write_manifest(&root, &records);
    let store = FileStore::with_base(session, base);

    let report = store
        .cleanup(CleanupTrigger::SessionEnd)
        .await
        .expect("cleanup");

    let OpResult::CleanupReport { removed, failed } = report else {
        panic!("expected CleanupReport");
    };
    assert_eq!((removed, failed.len()), (4, 0), "{failed:?}");
    for name in names {
        assert!(!root.join(name).exists(), "{name}");
    }
    assert_eq!(
        std::fs::read(&decoy).expect("the other file stays"),
        b"not the session's"
    );
}

/// A store whose base must be private stages, lists and cleans up like any
/// other; its folders are prepared before the first file is written.
#[tokio::test]
async fn a_private_base_serves_a_session_from_its_first_file_to_its_cleanup() {
    let sb = Sandbox::new();
    let data = sb.root.join("CretLink Data");
    let base = OwnFolder::administrators_only(&data, data.join("staging"));
    #[cfg(windows)]
    if !crate::own::tests_can_name_the_administrators() {
        return;
    }
    let session = SessionId::new();
    let store = FileStore::with_base(session, base.clone());

    transfer(
        &store,
        staging("tool.bin"),
        b"payload",
        3,
        false,
        Some("C:\\tech\\tool.bin"),
        CleanupPolicy::OnSessionEnd,
    )
    .await
    .expect("a transfer into a private base");

    let artifact = store.staging_root().join("tool.bin");
    assert!(artifact.exists());
    // On Windows the folders above were private before the first file was
    // written, and what was staged is closed to everyone else by what they
    // hand down.
    #[cfg(windows)]
    {
        assert!(crate::own::tests_is_private(&data));
        assert!(crate::own::tests_is_private(base.path()));
        for made in [
            store.staging_root().to_path_buf(),
            store.staging_root().join("manifest.json"),
            artifact.clone(),
        ] {
            assert!(
                crate::own::tests_closed_to_others(&made),
                "{}",
                made.display()
            );
        }
    }
    let census = FileStore::census_all_sessions(base.clone()).await;
    assert_eq!(census.staged_count(), 1);
    assert!(census.foreign.is_empty(), "{:?}", census.foreign);
    let report = FileStore::sweep_all_sessions(base, CleanupTrigger::Shutdown).await;
    assert!(report.ok(), "{:?}", report.failed);
    assert_eq!((report.artifacts_removed, report.sessions_removed), (1, 1));
    assert!(!artifact.exists());
}

/// A session whose manifest cannot be read is not passed over: the sweep
/// names it, and the census says that what it stages is unknown.
#[tokio::test]
async fn a_session_with_a_damaged_manifest_is_named_and_never_called_clean() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    let session = SessionId::new();
    let artifact = stage_artifact(&base, session, "ours.bin", CleanupPolicy::OnSessionEnd).await;
    let root = base.join(session.to_string());
    std::fs::write(root.join("manifest.json"), b"this is not a manifest").expect("damage");
    let named = vec![(text(&root), "the manifest could not be read".to_string())];

    let report = FileStore::sweep_all_sessions(base.clone(), CleanupTrigger::Shutdown).await;

    assert!(artifact.exists());
    assert_eq!(report.sessions_scanned, 1);
    assert_eq!((report.artifacts_removed, report.sessions_removed), (0, 0));
    assert_eq!(failures(&report), named);
    let census = FileStore::census_all_sessions(base.clone()).await;
    assert!(census.sessions.is_empty());
    assert_eq!(census.unread, vec![text(&root)]);
    assert!(!census.is_empty(), "unknown is not clean");
    // The purge does not guess what such a folder holds either.
    let purge = FileStore::sweep_all_sessions(base, CleanupTrigger::All).await;
    assert_eq!(failures(&purge), named);
    assert!(artifact.exists());
}

/// A file far larger than a manifest can be is not read into memory.
#[tokio::test]
async fn a_manifest_larger_than_a_manifest_can_be_is_not_read() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    let root = base.join(SessionId::new().to_string());
    std::fs::create_dir_all(&root).expect("create");
    // A manifest that would parse, followed by more blanks than a manifest
    // has bytes.
    let padded = format!(
        "{{\"version\":1,\"staged\":[]}}{}",
        " ".repeat(2 * 1024 * 1024 + 1)
    );
    std::fs::write(root.join("manifest.json"), padded).expect("write");

    let report = FileStore::sweep_all_sessions(base, CleanupTrigger::Shutdown).await;

    assert_eq!(
        failures(&report),
        vec![(text(&root), "the manifest could not be read".to_string())]
    );
}

/// Where CretLink never kept anything, looking and sweeping make nothing.
#[tokio::test]
async fn a_sweep_and_a_census_make_nothing_where_nothing_was_kept() {
    #[cfg(windows)]
    if !crate::own::tests_can_name_the_administrators() {
        return;
    }
    let sb = Sandbox::new();
    let data = sb.root.join("CretLink Data");
    let base = OwnFolder::administrators_only(&data, data.join("staging"));

    let census = FileStore::census_all_sessions(base.clone()).await;
    let startup = FileStore::sweep_all_sessions(base.clone(), CleanupTrigger::Shutdown).await;
    let purge = FileStore::sweep_all_sessions(base, CleanupTrigger::All).await;

    assert!(census.is_empty());
    assert_eq!(startup, super::StartupSweepReport::default());
    assert_eq!(purge, super::StartupSweepReport::default());
    assert!(!data.exists(), "nothing was made");
}

/// The manifest is written through a temporary file. A link that someone put
/// under that name is removed, never written through.
#[cfg(unix)]
#[tokio::test]
async fn the_temporary_file_of_a_manifest_is_never_written_through_a_link() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    let session = SessionId::new();
    stage_artifact(&base, session, "first.bin", CleanupPolicy::Keep).await;
    let root = base.join(session.to_string());
    let victim = sb.target().join("important.txt");
    std::fs::write(&victim, b"keep me").expect("write");
    let temporary = root.join(".manifest.json.tmp");
    std::os::unix::fs::symlink(&victim, &temporary).expect("link");

    stage_artifact(&base, session, "second.bin", CleanupPolicy::Keep).await;

    assert_eq!(std::fs::read(&victim).expect("the file stays"), b"keep me");
    assert!(std::fs::symlink_metadata(&temporary).is_err());
    let census = FileStore::census_all_sessions(base).await;
    assert_eq!(census.staged_count(), 2, "the manifest was written");
}

/// A link under the name of a session folder is not a session folder: nothing
/// behind it is read or removed. Every sweep names it, the census names it,
/// and the purge removes the link and leaves what it points at.
#[cfg(unix)]
#[tokio::test]
async fn a_link_in_the_place_of_a_session_folder_is_named_and_never_read() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    std::fs::create_dir_all(&base).expect("create base");
    // A whole staged session lies elsewhere, with a file its manifest would
    // have removed.
    let elsewhere = sb.root.join("elsewhere");
    let session = SessionId::new();
    let artifact = stage_artifact(
        &elsewhere,
        session,
        "theirs.bin",
        CleanupPolicy::OnSessionEnd,
    )
    .await;
    let link = base.join(session.to_string());
    std::os::unix::fs::symlink(elsewhere.join(session.to_string()), &link).expect("link");

    let report = FileStore::sweep_all_sessions(base.clone(), CleanupTrigger::Shutdown).await;

    assert!(artifact.exists(), "nothing behind the link was removed");
    assert_eq!(report.sessions_scanned, 0);
    assert_eq!(
        failures(&report),
        vec![(
            text(&link),
            "not made by CretLink; left as it is".to_string()
        )]
    );
    let census = FileStore::census_all_sessions(base.clone()).await;
    assert!(
        census.sessions.is_empty(),
        "nothing behind the link was read"
    );
    assert_eq!(census.foreign, vec![text(&link)]);
    assert!(!census.is_empty());

    let purge = FileStore::sweep_all_sessions(base.clone(), CleanupTrigger::All).await;

    assert!(purge.ok(), "{:?}", purge.failed);
    assert_eq!((purge.foreign_removed, purge.sessions_removed), (1, 0));
    assert!(
        std::fs::symlink_metadata(&link).is_err(),
        "the link is gone"
    );
    assert!(artifact.exists(), "what it pointed at is not");
    assert!(FileStore::census_all_sessions(base).await.is_empty());
}

/// A manifest that is a link is not read, although the folder is a session's
/// own: the session's files stay, the folder is named, and a cleanup of that
/// session is refused.
#[cfg(unix)]
#[tokio::test]
async fn a_manifest_that_is_a_link_is_not_read() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    let session = SessionId::new();
    let artifact = stage_artifact(&base, session, "ours.bin", CleanupPolicy::OnSessionEnd).await;
    let root = base.join(session.to_string());
    let moved = sb.root.join("manifest-elsewhere.json");
    std::fs::rename(root.join("manifest.json"), &moved).expect("move the manifest");
    std::os::unix::fs::symlink(&moved, root.join("manifest.json")).expect("link");

    let report = FileStore::sweep_all_sessions(base.clone(), CleanupTrigger::Shutdown).await;

    assert!(
        artifact.exists(),
        "the manifest behind the link was not followed"
    );
    assert_eq!(
        failures(&report),
        vec![(
            text(&root),
            "not made by CretLink; left as it is".to_string()
        )]
    );
    let store = FileStore::with_base(session, base.clone());
    let refused = store
        .cleanup(CleanupTrigger::All)
        .await
        .expect_err("a cleanup does not read it either");
    assert_eq!(refused.code, ErrorCode::AccessDenied);
    assert!(artifact.exists());
    let census = FileStore::census_all_sessions(base.clone()).await;
    assert_eq!(census.foreign, vec![text(&root)]);
    assert_eq!(store.census().await.staged.len(), 0);

    // The purge removes the folder whole; the manifest behind the link stays.
    let purge = FileStore::sweep_all_sessions(base, CleanupTrigger::All).await;
    assert!(purge.ok(), "{:?}", purge.failed);
    assert!(!root.exists());
    assert!(moved.exists());
}

/// A folder of parts that is a link is not swept: what lies behind it stays.
#[cfg(unix)]
#[tokio::test]
async fn a_folder_of_parts_that_is_a_link_is_not_swept() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    let session = SessionId::new();
    let root = base.join(session.to_string());
    std::fs::create_dir_all(&root).expect("create");
    let elsewhere = sb.target();
    let theirs = elsewhere.join("theirs.txt");
    std::fs::write(&theirs, b"theirs").expect("write");
    std::os::unix::fs::symlink(&elsewhere, root.join(".parts")).expect("link");

    let report = FileStore::sweep_all_sessions(base.clone(), CleanupTrigger::Shutdown).await;

    assert!(theirs.exists(), "nothing behind the link was removed");
    assert_eq!(report.artifacts_removed, 0);
    assert_eq!(
        failures(&report),
        vec![(
            text(&root),
            "not made by CretLink; left as it is".to_string()
        )]
    );
    let purge = FileStore::sweep_all_sessions(base, CleanupTrigger::All).await;
    assert!(purge.ok(), "{:?}", purge.failed);
    assert!(!root.exists());
    assert!(theirs.exists());
}

/// What a session can ask of its staging area, with a session folder that
/// holds the file `theirs.bin`.
fn every_staging_operation() -> Vec<Operation> {
    vec![
        Operation::FsStat {
            path: staging("theirs.bin"),
        },
        Operation::FsList {
            path: staging("."),
            continue_after: None,
        },
        Operation::FsRead {
            path: staging("theirs.bin"),
            offset: 0,
            len: 16,
        },
        Operation::FsDigest {
            path: staging("theirs.bin"),
            prefix_len: None,
        },
        Operation::FsDelete {
            path: staging("theirs.bin"),
        },
        Operation::FsAbort {
            dest: staging("theirs.bin"),
        },
        Operation::CreateDir {
            path: staging("made"),
        },
        Operation::RemoveDir {
            path: staging("made"),
        },
        Operation::FsMove {
            from: staging("theirs.bin"),
            to: staging("moved.bin"),
            overwrite: false,
        },
        Operation::RemoveTree {
            path: staging("."),
            stop: false,
            folder_id: None,
        },
        Operation::ListStaged {},
        Operation::StagingCleanup {
            trigger: CleanupTrigger::All,
        },
    ]
}

/// A private base with something else in the place of one of its folders is
/// not used at all: with `link` standing where the staging area or the data
/// folder belongs and pointing at a folder that holds the staged session
/// `session` with its file `artifact`, nothing behind it is read, removed or
/// made, by a census, by any sweep or by the session itself.
async fn assert_a_base_behind_a_link_is_not_used(
    base: OwnFolder,
    link: &Path,
    session: SessionId,
    artifact: &Path,
) {
    let behind = artifact.parent().expect("the session folder").to_path_buf();
    let before = std::fs::read_dir(&behind).expect("list").count();
    let named = vec![(
        text(base.path()),
        "the staging area could not be made private".to_string(),
    )];

    let census = FileStore::census_all_sessions(base.clone()).await;
    assert!(
        census.sessions.is_empty(),
        "a census reads nothing behind it"
    );
    assert_eq!(census.unread, vec![text(base.path())]);
    assert!(!census.is_empty(), "unknown is not clean");

    let startup = FileStore::sweep_all_sessions(base.clone(), CleanupTrigger::Shutdown).await;
    assert_eq!(startup.sessions_scanned, 0);
    assert_eq!(failures(&startup), named);
    // The purge does not go through it either, and does not remove it: only
    // an administrator can have put it there.
    let purge = FileStore::sweep_all_sessions(base.clone(), CleanupTrigger::All).await;
    assert_eq!(failures(&purge), named);
    assert_eq!(purge.foreign_removed, 0);

    // The session whose folder lies behind the link asks for everything a
    // session can ask of its staging area.
    let store = FileStore::with_base(session, base);
    let refused = transfer(
        &store,
        staging("tool.bin"),
        b"payload",
        4,
        false,
        Some("C:\\tech\\tool.bin"),
        CleanupPolicy::OnSessionEnd,
    )
    .await
    .expect_err("nothing is staged there");
    assert_eq!(refused.code, ErrorCode::AccessDenied);
    let mut refusals = Vec::new();
    for op in every_staging_operation() {
        refusals.push(store.perform(op).await.map(|_| ()).map_err(|e| e.code));
    }
    assert_eq!(
        refusals,
        vec![Err(ErrorCode::AccessDenied); 12],
        "no staging operation goes through the link"
    );
    assert_eq!(
        store
            .cleanup(CleanupTrigger::All)
            .await
            .expect_err("no cleanup either")
            .code,
        ErrorCode::AccessDenied
    );
    assert_eq!(store.census().await.staged.len(), 0);

    assert!(artifact.exists(), "nothing behind the link was removed");
    assert_eq!(
        std::fs::read_dir(&behind).expect("list").count(),
        before,
        "nothing was made behind it"
    );
    assert!(
        std::fs::symlink_metadata(link).is_ok(),
        "the link is left as it is"
    );
}

/// A link where the staging area of a private base belongs.
#[cfg(unix)]
#[tokio::test]
async fn a_private_base_that_is_a_link_is_not_used() {
    let sb = Sandbox::new();
    let data = sb.root.join("CretLink Data");
    std::fs::create_dir_all(&data).expect("create");
    let elsewhere = sb.root.join("elsewhere");
    let session = SessionId::new();
    let artifact = stage_artifact(
        &elsewhere,
        session,
        "theirs.bin",
        CleanupPolicy::OnSessionEnd,
    )
    .await;
    let link = data.join("staging");
    std::os::unix::fs::symlink(&elsewhere, &link).expect("link");
    let base = OwnFolder::administrators_only(&data, &link);

    assert_a_base_behind_a_link_is_not_used(base, &link, session, &artifact).await;
}

/// A link where the data folder of a private base belongs.
#[cfg(unix)]
#[tokio::test]
async fn a_private_base_behind_a_link_is_not_used() {
    let sb = Sandbox::new();
    let elsewhere = sb.root.join("elsewhere");
    let session = SessionId::new();
    let artifact = stage_artifact(
        &elsewhere.join("staging"),
        session,
        "theirs.bin",
        CleanupPolicy::OnSessionEnd,
    )
    .await;
    let data = sb.root.join("CretLink Data");
    std::os::unix::fs::symlink(&elsewhere, &data).expect("link");
    let base = OwnFolder::administrators_only(&data, data.join("staging"));

    assert_a_base_behind_a_link_is_not_used(base, &data, session, &artifact).await;
}

/// On Windows a link to a folder is a junction, which an account without
/// rights can make wherever it can make a folder. The same holds for it as
/// for a link elsewhere.
#[cfg(windows)]
mod junctions {
    use std::path::Path;
    use std::process::Command;

    use super::*;

    fn junction(link: &Path, target: &Path) {
        let done = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .expect("run mklink");
        assert!(
            done.status.success(),
            "mklink /J: {}",
            String::from_utf8_lossy(&done.stderr)
        );
    }

    /// A junction under the name of a session folder is not a session folder:
    /// nothing behind it is read or removed. Every sweep names it, the census
    /// names it, and the purge removes the junction and leaves what it points
    /// at.
    #[tokio::test]
    async fn a_junction_in_the_place_of_a_session_folder_is_named_and_never_read() {
        let sb = Sandbox::new();
        let base = sb.staging_base();
        std::fs::create_dir_all(&base).expect("create base");
        let elsewhere = sb.root.join("elsewhere");
        let session = SessionId::new();
        let artifact = stage_artifact(
            &elsewhere,
            session,
            "theirs.bin",
            CleanupPolicy::OnSessionEnd,
        )
        .await;
        let link = base.join(session.to_string());
        junction(&link, &elsewhere.join(session.to_string()));

        let report = FileStore::sweep_all_sessions(base.clone(), CleanupTrigger::Shutdown).await;

        assert!(artifact.exists(), "nothing behind the junction was removed");
        assert_eq!(report.sessions_scanned, 0);
        assert_eq!(
            failures(&report),
            vec![(
                text(&link),
                "not made by CretLink; left as it is".to_string()
            )]
        );
        let census = FileStore::census_all_sessions(base.clone()).await;
        assert!(census.sessions.is_empty(), "nothing behind it was read");
        assert_eq!(census.foreign, vec![text(&link)]);

        let purge = FileStore::sweep_all_sessions(base.clone(), CleanupTrigger::All).await;

        assert!(purge.ok(), "{:?}", purge.failed);
        assert_eq!((purge.foreign_removed, purge.sessions_removed), (1, 0));
        assert!(
            std::fs::symlink_metadata(&link).is_err(),
            "the junction is gone"
        );
        assert!(artifact.exists(), "what it pointed at is not");
        assert!(FileStore::census_all_sessions(base).await.is_empty());
    }

    /// A folder of parts that is a junction is not swept: what lies behind it
    /// stays.
    #[tokio::test]
    async fn a_folder_of_parts_that_is_a_junction_is_not_swept() {
        let sb = Sandbox::new();
        let base = sb.staging_base();
        let session = SessionId::new();
        let root = base.join(session.to_string());
        std::fs::create_dir_all(&root).expect("create");
        let elsewhere = sb.target();
        let theirs = elsewhere.join("theirs.txt");
        std::fs::write(&theirs, b"theirs").expect("write");
        junction(&root.join(".parts"), &elsewhere);

        let report = FileStore::sweep_all_sessions(base.clone(), CleanupTrigger::Shutdown).await;

        assert!(theirs.exists(), "nothing behind the junction was removed");
        assert_eq!(report.artifacts_removed, 0);
        assert_eq!(
            failures(&report),
            vec![(
                text(&root),
                "not made by CretLink; left as it is".to_string()
            )]
        );
        let store = FileStore::with_base(session, base.clone());
        assert_eq!(
            store
                .cleanup(CleanupTrigger::All)
                .await
                .expect_err("a cleanup does not enter it either")
                .code,
            ErrorCode::AccessDenied
        );
        let purge = FileStore::sweep_all_sessions(base, CleanupTrigger::All).await;
        assert!(purge.ok(), "{:?}", purge.failed);
        assert!(!root.exists());
        assert!(theirs.exists());
    }

    /// A junction where the staging area of a private base belongs.
    #[tokio::test]
    async fn a_private_base_that_is_a_junction_is_not_used() {
        if !crate::own::tests_can_name_the_administrators() {
            return;
        }
        let sb = Sandbox::new();
        let data = sb.root.join("CretLink Data");
        let elsewhere = sb.root.join("elsewhere");
        let session = SessionId::new();
        let artifact = stage_artifact(
            &elsewhere,
            session,
            "theirs.bin",
            CleanupPolicy::OnSessionEnd,
        )
        .await;
        // The data folder is a private one of the administrators; the
        // junction stands in it where the staging area belongs.
        OwnFolder::administrators_only(&data, &data)
            .prepare()
            .expect("prepare the data folder");
        let link = data.join("staging");
        junction(&link, &elsewhere);
        let base = OwnFolder::administrators_only(&data, &link);

        assert_a_base_behind_a_link_is_not_used(base, &link, session, &artifact).await;
    }

    /// A junction where the data folder of a private base belongs.
    #[tokio::test]
    async fn a_private_base_behind_a_junction_is_not_used() {
        if !crate::own::tests_can_name_the_administrators() {
            return;
        }
        let sb = Sandbox::new();
        let elsewhere = sb.root.join("elsewhere");
        let session = SessionId::new();
        let artifact = stage_artifact(
            &elsewhere.join("staging"),
            session,
            "theirs.bin",
            CleanupPolicy::OnSessionEnd,
        )
        .await;
        let data = sb.root.join("CretLink Data");
        junction(&data, &elsewhere);
        let base = OwnFolder::administrators_only(&data, data.join("staging"));

        assert_a_base_behind_a_link_is_not_used(base, &data, session, &artifact).await;
    }
}

/// A move keeps a file's permissions and the handles opened on it, so a
/// private staging area takes nothing by a move from elsewhere. In a staging
/// area of the account, where no boundary is crossed, the move works as
/// before.
#[tokio::test]
async fn nothing_is_moved_into_a_private_staging_area_from_elsewhere() {
    #[cfg(windows)]
    if !crate::own::tests_can_name_the_administrators() {
        return;
    }
    let sb = Sandbox::new();
    let data = sb.root.join("CretLink Data");
    let base = OwnFolder::administrators_only(&data, data.join("staging"));
    let store = FileStore::with_base(SessionId::new(), base);
    let target = sb.root.join("target");
    std::fs::create_dir_all(&target).expect("target");
    std::fs::write(target.join("theirs.bin"), b"theirs").expect("write");

    let refused = store
        .perform(Operation::FsMove {
            from: abs(&target, "theirs.bin"),
            to: staging("tool.bin"),
            overwrite: false,
        })
        .await
        .expect_err("a move into a private staging area");

    assert_eq!(refused.code, ErrorCode::Unsupported);
    assert!(
        target.join("theirs.bin").exists(),
        "the file stays where it was"
    );
    assert!(!store.staging_root().join("tool.bin").exists());

    let own = FileStore::with_base(
        SessionId::new(),
        OwnFolder::same_account(sb.root.join("own")),
    );
    own.perform(Operation::FsMove {
        from: abs(&target, "theirs.bin"),
        to: staging("tool.bin"),
        overwrite: false,
    })
    .await
    .expect("a move into a staging area of the account");
    assert!(own.staging_root().join("tool.bin").exists());
}

// --- what the review of the corrections asked for (ST-M010-001) --------------

/// What a manifest says is text that anybody may have written. A path in it
/// that is longer than an answer may carry, or that holds a line break, is
/// still answered: bounded and on one line, and never touched.
#[tokio::test]
async fn a_path_from_a_planted_manifest_fits_the_answer_whatever_it_holds() {
    use cretlink_protocol::limits::MAX_PATH_BYTES;

    let sb = Sandbox::new();
    let store = sb.store();
    let long = format!("{}\nsecond line", "x".repeat(MAX_PATH_BYTES + 1));
    write_manifest(store.staging_root(), &[record(Path::new(&long))]);

    let report = store
        .cleanup(CleanupTrigger::SessionEnd)
        .await
        .expect("a report");

    assert_on_the_wire(&report);
    let OpResult::CleanupReport { removed, failed } = report else {
        panic!("expected a cleanup report");
    };
    assert_eq!(removed, 0);
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert!(failed[0].canonical_path.len() <= MAX_PATH_BYTES);
    assert!(!failed[0].canonical_path.contains('\n'));
    assert_eq!(failed[0].reason, "outside the staging area; not touched");
}

/// A cleanup that cannot use its staging area still removes the parts of
/// unfinished transfers to a folder, which do not lie there, and the store
/// names the ones that are left.
#[tokio::test]
async fn a_cleanup_that_cannot_use_the_staging_area_still_removes_unfinished_parts() {
    let sb = Sandbox::new();
    let store = sb.store();
    let folder = sb.target();
    for name in ["gone.bin", "stuck.bin"] {
        store
            .perform(chunk(&abs(&folder, name), 0, b"half"))
            .await
            .expect("the chunk");
    }
    let stuck = store
        .part_path(&abs(&folder, "stuck.bin"))
        .expect("the part of this transfer");
    let block = block_removal(&stuck);
    // A manifest that cannot be read: the cleanup of the staging area fails.
    std::fs::create_dir_all(store.staging_root()).expect("the session folder");
    std::fs::write(
        store.staging_root().join("manifest.json"),
        b"not a manifest",
    )
    .expect("write");

    store
        .cleanup(CleanupTrigger::SessionEnd)
        .await
        .expect_err("the manifest cannot be read");

    assert_eq!(
        names_in(&folder).len(),
        1,
        "the part that could be removed is gone"
    );
    assert_eq!(store.unfinished_parts(), vec![text(&stuck)]);
    release_block(block, &stuck);
}

/// A file that lies under the name of a transfer's part before the transfer
/// wrote anything was put there by someone else, who would stay the owner of
/// the published file. It is not written to and not published; a discard
/// removes it, and the transfer then goes through.
#[tokio::test]
async fn a_file_that_lay_under_the_name_of_a_part_before_the_transfer_is_not_taken_for_it() {
    let sb = Sandbox::new();
    let store = sb.store();
    let folder = sb.target();
    let dest = abs(&folder, "report.pdf");
    let part = store.part_path(&dest).expect("the part of this transfer");
    std::fs::write(&part, b"").expect("someone else plants an empty file");

    let refused = store
        .perform(chunk(&dest, 0, b"content"))
        .await
        .expect_err("not written to");
    assert_eq!(refused.code, ErrorCode::WrongPathKind);
    assert_eq!(
        std::fs::read(&part).expect("read"),
        b"",
        "nothing was written"
    );
    let refused = store
        .perform(commit_of(&dest, b"", false))
        .await
        .expect_err("not published");
    assert_eq!(refused.code, ErrorCode::WrongPathKind);
    assert_eq!(names_in(&folder).len(), 1, "nothing was published");

    store
        .perform(Operation::FsAbort { dest: dest.clone() })
        .await
        .expect("the discard removes what lies there");
    assert!(names_in(&folder).is_empty());
    transfer(
        &store,
        dest,
        b"content",
        4,
        false,
        None,
        CleanupPolicy::Keep,
    )
    .await
    .expect("the transfer goes through");
    assert_eq!(names_in(&folder), vec!["report.pdf"]);
}

/// A folder that was moved away took the part of an unfinished transfer
/// along. The cleanup cannot know that the part is gone and does not say so:
/// it names the part, keeps it in mind, and removes it once the folder is
/// back in its place. A discard does not claim that it is gone either.
#[tokio::test]
async fn a_part_whose_folder_is_no_longer_in_its_place_is_named_and_tried_again() {
    let sb = Sandbox::new();
    let store = sb.store();
    let folder = sb.target();
    let dest = abs(&folder, "half.bin");
    store
        .perform(chunk(&dest, 0, b"half of it"))
        .await
        .expect("the chunk");
    let part = store.part_path(&dest).expect("the part of this transfer");
    let moved = folder.with_file_name("moved-away");
    std::fs::rename(&folder, &moved).expect("the folder is moved away");

    let report = store
        .cleanup(CleanupTrigger::SessionEnd)
        .await
        .expect("a report");
    let OpResult::CleanupReport { removed, failed } = report else {
        panic!("expected a cleanup report");
    };
    assert_eq!(removed, 0);
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0].canonical_path, text(&part));
    assert!(
        failed[0]
            .reason
            .starts_with("its folder is no longer in its place"),
        "{}",
        failed[0].reason
    );
    assert_eq!(
        names_in(&moved).len(),
        1,
        "the part lies where the folder is now"
    );
    let refused = store
        .perform(Operation::FsAbort { dest: dest.clone() })
        .await
        .expect_err("not known to be gone");
    assert_eq!(refused.code, ErrorCode::PathNotFound);

    std::fs::rename(&moved, &folder).expect("back in its place");
    assert_eq!(
        cleanup(&store, CleanupTrigger::SessionEnd).await,
        (1, Vec::new())
    );
    assert!(names_in(&folder).is_empty());
}

// --- a folder page always fits the channel (ST-M010-005) ---------------------

/// How many characters the names of the long-name tests have. Windows takes a
/// name of 255 UTF-16 units, so 250 characters of three bytes each; the other
/// systems the agent is built for take 255 bytes, which is 85 of them.
const LONG_NAME_CHARS: usize = if cfg!(windows) { 250 } else { 85 };

/// A name of [`LONG_NAME_CHARS`] characters of three bytes each, told apart by
/// `n`, which is written in digits of three bytes too.
fn long_name(n: usize) -> String {
    let digits: String = format!("{n:03}")
        .chars()
        .map(|digit| {
            let value = digit.to_digit(10).expect("a decimal digit");
            // The fullwidth digits U+FF10 to U+FF19.
            char::from_u32(0xFF10 + value).expect("a fullwidth digit")
        })
        .collect();
    let name = format!("{digits}{}", "\u{8A9E}".repeat(LONG_NAME_CHARS - 3));
    assert_eq!(name.len(), 3 * LONG_NAME_CHARS);
    name
}

/// Lists `path` page by page, the way a caller does: each request continues
/// after the last name of the page before. Every page must be an answer that
/// fits one record of the secure channel, with any request identifier, and a
/// page that says more follows must hold an entry. Returns the names of every
/// page.
async fn list_in_pages(store: &FileStore, path: &RemotePath) -> Vec<Vec<String>> {
    use cretlink_protocol::{Message, RequestId, limits};

    let mut pages: Vec<Vec<String>> = Vec::new();
    let mut continue_after = None;
    loop {
        let result = store
            .perform(Operation::FsList {
                path: path.clone(),
                continue_after: continue_after.clone(),
            })
            .await
            .expect("a page");
        let sent = Message::Response {
            id: RequestId(u64::MAX),
            session: SessionId::new(),
            result: result.clone(),
        }
        .encode()
        .expect("the page is within the bounds of the wire");
        assert!(
            sent.len() <= limits::MAX_RECORD_BYTES,
            "a page of {} bytes does not fit one record",
            sent.len()
        );
        let OpResult::DirEntries { entries, end } = result else {
            panic!("expected DirEntries");
        };
        let names: Vec<String> = entries.into_iter().map(|entry| entry.name).collect();
        if end {
            pages.push(names);
            return pages;
        }
        continue_after = Some(
            names
                .last()
                .expect("a page that says more follows holds an entry")
                .clone(),
        );
        pages.push(names);
        assert!(pages.len() < 1000, "the listing must end");
    }
}

/// A folder of 600 files with names as long as the system takes, in
/// characters of three bytes: a page of 256 of them is far larger than one
/// record of the secure channel. Every page fits a record, the pages end
/// before they hold 256 entries, and together they name every file exactly
/// once, in order.
#[tokio::test]
async fn a_folder_of_long_names_is_listed_in_pages_that_fit_one_record() {
    use cretlink_protocol::limits;

    let sb = Sandbox::new();
    let store = sb.store();
    store
        .perform(Operation::CreateDir {
            path: staging("long-names"),
        })
        .await
        .expect("create the staged folder");
    let folder = store.staging_root().join("long-names");
    let mut expected: Vec<String> = (0..600).map(long_name).collect();
    for name in &expected {
        std::fs::write(folder.join(name), b"x").expect("write a file with a long name");
    }
    expected.sort();

    let pages = list_in_pages(&store, &staging("long-names")).await;

    assert!(
        pages.len() > 600 / limits::MAX_DIR_PAGE + 1,
        "{}",
        pages.len()
    );
    for page in &pages {
        assert!(
            page.len() < limits::MAX_DIR_PAGE,
            "a page of {} long names",
            page.len()
        );
    }
    let listed: Vec<String> = pages.into_iter().flatten().collect();
    assert_eq!(listed.len(), expected.len());
    assert!(listed == expected, "every name exactly once, in order");
}

/// Short names are listed as before: a page ends at its number of entries,
/// the pages together name every file exactly once, and the last page says
/// that it is the last.
#[tokio::test]
async fn a_folder_of_short_names_is_listed_in_pages_of_the_full_number_of_entries() {
    use cretlink_protocol::limits;

    let sb = Sandbox::new();
    let store = sb.store();
    store
        .perform(Operation::CreateDir {
            path: staging("short-names"),
        })
        .await
        .expect("create the staged folder");
    let folder = store.staging_root().join("short-names");
    let count = 2 * limits::MAX_DIR_PAGE + 10;
    let expected: Vec<String> = (0..count).map(|n| format!("file-{n:04}.txt")).collect();
    for name in &expected {
        std::fs::write(folder.join(name), b"x").expect("write");
    }

    let pages = list_in_pages(&store, &staging("short-names")).await;

    let sizes: Vec<usize> = pages.iter().map(Vec::len).collect();
    assert_eq!(sizes, [limits::MAX_DIR_PAGE, limits::MAX_DIR_PAGE, 10]);
    let listed: Vec<String> = pages.into_iter().flatten().collect();
    assert_eq!(listed, expected);
}

/// Names that the encoding writes longer than they are — a quotation mark and
/// a backslash as two bytes, a control character as six — are counted as they
/// are written: the pages still fit a record and name every file once. Such
/// names exist off Windows only.
#[cfg(unix)]
#[tokio::test]
async fn names_that_the_encoding_writes_longer_are_counted_as_they_are_written() {
    use cretlink_protocol::limits;

    let sb = Sandbox::new();
    let store = sb.store();
    store
        .perform(Operation::CreateDir {
            path: staging("escaped"),
        })
        .await
        .expect("create the staged folder");
    let folder = store.staging_root().join("escaped");
    // 250 control characters are 250 bytes on the disk and 1500 in the answer.
    let mut expected: Vec<String> = (0..120)
        .map(|n| format!("{n:03}{}", "\u{1}".repeat(250)))
        .chain((0..120).map(|n| format!("q{n:03}{}", "\"\\".repeat(120))))
        .collect();
    for name in &expected {
        std::fs::write(folder.join(name), b"x").expect("write");
    }
    expected.sort();

    let pages = list_in_pages(&store, &staging("escaped")).await;

    assert!(pages.len() > 2, "{}", pages.len());
    assert!(pages.iter().all(|page| page.len() < limits::MAX_DIR_PAGE));
    let listed: Vec<String> = pages.into_iter().flatten().collect();
    assert!(listed == expected, "every name exactly once, in order");
}

/// An entry that not even an empty page takes fails the listing with the
/// reason: a page that says more follows is never empty. No file system the
/// agent runs on has such a name, so the entry is made up here.
#[test]
fn an_entry_that_fits_no_page_fails_the_listing_instead_of_ending_a_page_empty() {
    use cretlink_protocol::{DirPage, FileEntry, limits};

    let entry = |name: String| FileEntry {
        name,
        kind: FileKind::File,
        len: 1,
        modified_ms: None,
        readonly: false,
    };
    let mut page = DirPage::new();
    let refused = super::add_to_page(&mut page, entry("n".repeat(2 * limits::MAX_RECORD_BYTES)))
        .expect_err("larger than any answer");
    assert_eq!(refused.code, ErrorCode::LimitExceeded);
    assert!(page.is_empty());

    // An entry that fits is added, and a page that is full ends before the
    // next one without failing.
    assert_eq!(
        super::add_to_page(&mut page, entry("a".to_owned())),
        Ok(true)
    );
    let mut n = 0;
    while super::add_to_page(&mut page, entry(format!("{n:03}{}", "b".repeat(700))))
        .expect("a page that holds an entry ends, it does not fail")
    {
        n += 1;
    }
    assert!(n > 1 && page.len() == n + 1);
}
