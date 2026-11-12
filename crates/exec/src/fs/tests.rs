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
        } => {
            assert_eq!(state, TreeState::Removed);
            assert_eq!((removed_files, removed_dirs), (2, 2));
            assert!(failed.is_empty());
            assert_eq!(failed_more, 0);
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
    loop {
        requests += 1;
        let result = store
            .perform(Operation::RemoveTree {
                path: abs(&target, "big"),
                stop: false,
            })
            .await
            .expect("a step");
        assert_on_the_wire(&result);
        let OpResult::TreeRemoval {
            state,
            removed_files,
            removed_dirs,
            ..
        } = result
        else {
            panic!("expected TreeRemoval");
        };
        assert!(removed_files + removed_dirs <= limits::MAX_TREE_STEP_ENTRIES);
        removed += removed_files + removed_dirs;
        match state {
            TreeState::More => assert!(big.exists()),
            TreeState::Removed => break,
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

/// A link that someone planted under the name of a transfer's part is never
/// written through and never published: the write and the commit are refused
/// and the file the link points at keeps its content.
#[cfg(unix)]
#[tokio::test]
async fn a_link_planted_under_the_parts_name_is_never_written_through() {
    let sb = Sandbox::new();
    let store = sb.store();
    let folder = sb.target();
    let victim = sb.root.join("victim.txt");
    std::fs::write(&victim, b"untouched").expect("write the victim");
    let dest = abs(&folder, "tool.exe");
    let part = store.part_path(&dest).expect("the part of this transfer");
    std::os::unix::fs::symlink(&victim, &part).expect("plant the link");

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
            dest,
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

// --- Windows only: a held destination and another drive (ST-M010-001) -------

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
