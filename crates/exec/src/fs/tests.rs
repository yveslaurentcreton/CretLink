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
        vec![Err(ErrorCode::AccessDenied); 8],
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

    // Nor by the full path of the staging folder: where a file would lie
    // decides, not how the request spells it.
    transfer(
        &store,
        staging("kept.bin"),
        b"kept",
        2,
        false,
        Some("kept.bin"),
        CleanupPolicy::Keep,
    )
    .await
    .expect("an upload makes the staging folder");
    let refused = store
        .perform(Operation::FsMove {
            from: abs(&target, "theirs.bin"),
            to: abs(store.staging_root(), "tool.bin"),
            overwrite: false,
        })
        .await
        .expect_err("a move into a private staging area by its full path");
    assert_eq!(refused.code, ErrorCode::Unsupported);
    assert!(target.join("theirs.bin").exists());
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

/// What leaves a private staging area does not keep the permissions it had
/// there, which admit only administrators: it takes the ones of the folder it
/// arrives in, like a file that was made there.
#[cfg(windows)]
#[tokio::test]
async fn a_file_moved_out_of_a_private_staging_area_takes_the_permissions_of_its_new_folder() {
    if !crate::own::tests_can_name_the_administrators() {
        return;
    }
    let sb = Sandbox::new();
    let data = sb.root.join("CretLink Data");
    let base = OwnFolder::administrators_only(&data, data.join("staging"));
    let store = FileStore::with_base(SessionId::new(), base);
    let target = sb.root.join("target");
    std::fs::create_dir_all(&target).expect("target");
    std::fs::write(target.join("made-here.bin"), b"made here").expect("write");
    for name in ["tool.bin", "named.bin"] {
        transfer(
            &store,
            staging(name),
            b"tool",
            2,
            false,
            Some(name),
            CleanupPolicy::Keep,
        )
        .await
        .expect("the upload");
    }
    let made_here = crate::own::tests_permission_entries(&target.join("made-here.bin"));
    let staged = crate::own::tests_permission_entries(&store.staging_root().join("tool.bin"));
    assert!(!made_here.is_empty() && !staged.is_empty());
    assert_ne!(
        staged, made_here,
        "the staging area admits fewer than the folder"
    );

    store
        .perform(Operation::FsMove {
            from: staging("tool.bin"),
            to: abs(&target, "tool.bin"),
            overwrite: false,
        })
        .await
        .expect("the move");

    assert_eq!(
        crate::own::tests_permission_entries(&target.join("tool.bin")),
        made_here,
        "the moved file has the permissions of a file made in that folder"
    );

    // The same for a staged file that the request names by its full path, as
    // the answer of an upload gives it: where the file lies decides.
    store
        .perform(Operation::FsMove {
            from: abs(store.staging_root(), "named.bin"),
            to: abs(&target, "named.bin"),
            overwrite: false,
        })
        .await
        .expect("the move by the full path");
    assert_eq!(
        crate::own::tests_permission_entries(&target.join("named.bin")),
        made_here,
        "and so has the one that was named by its full path"
    );
}
