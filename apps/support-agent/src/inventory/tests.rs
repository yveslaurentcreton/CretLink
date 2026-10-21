//! Tests for the owned-resource inventory and its recovery verb (ST-M006-003).
//!
//! Every test drives the parameterized [`scan_at`]/[`recover_at`] entry points
//! against a per-test sandbox, so the whole surface runs in-process on every CI
//! host (no elevation, no reboot, no real customer machine). Staged residue is
//! created through the *public* `FileStore` commit flow; reboot-resume residue
//! through the *public* `TicketStore` — the inventory never reaches inside
//! either owner.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cretlink_exec::FileStore;
use cretlink_protocol::{
    ArmedTicket, B64Bytes, CleanupPolicy, DigestAlgo, OpResult, Operation,
    REBOOT_RESUME_TICKET_ID_LEN, RemotePath, SessionId, TargetBinding, TargetIdentity, TicketId,
};
use sha2::{Digest, Sha256};

use super::{ResourceKind, recover_at, scan_at};
use crate::reboot_resume::TicketStore;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// A unique on-disk sandbox that is torn down when the test ends.
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("cretlink-inv-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create sandbox root");
        Self { root }
    }

    /// The staging base a `FileStore` writes session trees beneath.
    fn staging_base(&self) -> PathBuf {
        self.root.join("staging")
    }

    /// A ticket store at a real, writable file path.
    fn ticket_store(&self) -> TicketStore {
        TicketStore::new(self.root.join("reboot-resume").join("tickets.json"))
    }

    /// A ticket store whose path is an existing *directory*, so both `load` and
    /// `remove` fail with a non-`NotFound` I/O error — the cross-platform way to
    /// exercise the unreadable / removal-failure branches without OS-specific
    /// file locking.
    fn unreadable_ticket_store(&self) -> TicketStore {
        let dir = self.root.join("bad-store");
        std::fs::create_dir_all(&dir).expect("create dir for the unreadable store");
        TicketStore::new(dir)
    }

    /// A ticket store whose file holds malformed JSON: `load` fails (so a scan
    /// reports it unknown), but the file is an ordinary file, so `remove`
    /// succeeds and recovery resolves it cleanly.
    fn malformed_ticket_store(&self) -> TicketStore {
        let dir = self.root.join("malformed");
        std::fs::create_dir_all(&dir).expect("create dir for the malformed store");
        let path = dir.join("tickets.json");
        std::fs::write(&path, b"this is not json").expect("write malformed store");
        TicketStore::new(path)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn sha256(bytes: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().to_vec()
}

fn target_identity() -> TargetIdentity {
    TargetIdentity {
        hostname: "CUSTOMER-PC".to_string(),
        os_version: "Windows 11 Pro 10.0.26200".to_string(),
        arch: "x86_64".to_string(),
        elevated: true,
        simulated: false,
        agent_version: "0.0.0".to_string(),
    }
}

/// A plausible armed ticket at rest. The sealed payload is opaque filler — the
/// inventory never unseals it, so no real key material is needed.
fn ticket_fixture(fill: u8, not_after_unix_millis: i64) -> ArmedTicket {
    ArmedTicket {
        ticket_id: TicketId([fill; REBOOT_RESUME_TICKET_ID_LEN]),
        target: TargetBinding::of(&target_identity()),
        not_after_unix_millis,
        sealed_key: B64Bytes::new(vec![fill; 8]),
    }
}

/// Stages one artifact under `base` for `session` with the given cleanup policy,
/// through the public write+commit flow, and returns its committed path.
async fn stage_artifact(
    base: &Path,
    session: SessionId,
    name: &str,
    cleanup: CleanupPolicy,
) -> PathBuf {
    let store = FileStore::with_base(session, base.to_path_buf());
    let bytes = b"leftover-bytes";
    let dest = RemotePath::Staging {
        name: name.to_string(),
    };
    let write = store
        .perform(Operation::FsWrite {
            dest: dest.clone(),
            offset: 0,
            data: B64Bytes::new(bytes.to_vec()),
        })
        .await
        .expect("staging write");
    assert!(
        matches!(write, OpResult::WriteAccepted { .. }),
        "the write is accepted"
    );
    store
        .perform(Operation::FsCommit {
            dest,
            len: bytes.len() as u64,
            algo: DigestAlgo::Sha256,
            digest: B64Bytes::new(sha256(bytes)),
            overwrite: false,
            source: Some("C:\\tech\\tool.zip".to_string()),
            cleanup,
        })
        .await
        .expect("staging commit");
    let artifact = store.staging_root().join(name);
    assert!(artifact.exists(), "the staged artifact is on disk");
    artifact
}

#[tokio::test]
async fn clean_machine_scans_clean_and_asserts_every_kind_empty() {
    let sb = Sandbox::new();
    let inventory = scan_at(&sb.staging_base(), &sb.ticket_store()).await;

    assert!(inventory.is_clean(), "nothing found, nothing unreadable");
    assert!(inventory.found.is_empty());
    assert!(inventory.unreadable.is_empty());

    // Every kind is accounted for by an honest empty assertion — none omitted.
    for kind in ResourceKind::ALL {
        assert!(
            inventory.asserted_empty.contains(&kind),
            "kind {} is asserted empty on a clean machine",
            kind.label()
        );
    }
    assert_eq!(inventory.asserted_empty.len(), ResourceKind::ALL.len());
}

#[tokio::test]
async fn reboot_resume_ticket_is_found_machine_scoped() {
    let sb = Sandbox::new();
    let store = sb.ticket_store();
    store
        .save(&[ticket_fixture(0x11, 9_999)])
        .expect("persist a ticket");

    let inventory = scan_at(&sb.staging_base(), &store).await;

    assert!(!inventory.is_clean());
    let tickets: Vec<_> = inventory
        .found
        .iter()
        .filter(|r| r.kind == ResourceKind::RebootResumeTicket)
        .collect();
    assert_eq!(tickets.len(), 1, "the one armed ticket is found");
    assert_eq!(tickets[0].session, None, "a ticket is machine-scoped");
    // Staged files are still asserted empty alongside the ticket residue.
    assert!(
        inventory
            .asserted_empty
            .contains(&ResourceKind::StagedFiles)
    );
}

#[tokio::test]
async fn staged_files_and_ticket_residue_compose_with_owning_session() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    let session = SessionId::new();
    let artifact = stage_artifact(&base, session, "leftover.bin", CleanupPolicy::Keep).await;
    let store = sb.ticket_store();
    store
        .save(&[ticket_fixture(0x22, 12_345)])
        .expect("persist a ticket");

    let inventory = scan_at(&base, &store).await;

    let staged: Vec<_> = inventory
        .found
        .iter()
        .filter(|r| r.kind == ResourceKind::StagedFiles)
        .collect();
    assert_eq!(staged.len(), 1, "the staged artifact is enumerated");
    assert_eq!(
        staged[0].session,
        Some(session),
        "staged residue carries its owning session (REQ-S-017)"
    );
    assert_eq!(
        staged[0].description,
        artifact.to_string_lossy(),
        "the residue is named by its committed path"
    );
    assert!(
        inventory
            .found
            .iter()
            .any(|r| r.kind == ResourceKind::RebootResumeTicket),
        "the ticket is found alongside the staged file"
    );
}

#[tokio::test]
async fn recover_removes_all_residue_and_is_idempotent() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    let session = SessionId::new();
    // A `Keep` artifact proves recovery clears even keep-until-shutdown residue.
    let artifact = stage_artifact(&base, session, "keep.bin", CleanupPolicy::Keep).await;
    let store = sb.ticket_store();
    store
        .save(&[ticket_fixture(0x33, 42)])
        .expect("persist a ticket");

    let report = recover_at(&base, &store).await;

    assert!(report.ok(), "recovery left the machine clean");
    assert!(report.failed.is_empty());
    assert!(report.unreadable.is_empty());
    assert_eq!(report.found.len(), 2, "both residues recorded as evidence");
    assert_eq!(report.removed.len(), 2, "both residues removed");
    assert!(!artifact.exists(), "the staged artifact is gone from disk");
    assert!(
        store.load().expect("empty store loads clean").is_empty(),
        "the ticket store no longer holds tickets"
    );

    // A rescan is clean, and a second recovery is a clean no-op.
    let rescan = scan_at(&base, &store).await;
    assert!(rescan.is_clean(), "the machine is clean after recovery");
    let again = recover_at(&base, &store).await;
    assert!(again.ok());
    assert!(again.found.is_empty(), "nothing left to find");
    assert!(again.removed.is_empty(), "nothing left to remove");
}

#[tokio::test]
async fn unreadable_ticket_store_is_surfaced_not_assumed_empty() {
    let sb = Sandbox::new();
    let inventory = scan_at(&sb.staging_base(), &sb.unreadable_ticket_store()).await;

    assert!(!inventory.is_clean(), "an unreadable store is not clean");
    let unreadable: Vec<_> = inventory
        .unreadable
        .iter()
        .filter(|u| u.kind == ResourceKind::RebootResumeTicket)
        .collect();
    assert_eq!(unreadable.len(), 1, "the store is surfaced as unknown");
    assert!(!unreadable[0].reason.is_empty(), "with a reason");
    // The unreadable ticket store is never counted as an empty assertion.
    assert!(
        !inventory
            .asserted_empty
            .contains(&ResourceKind::RebootResumeTicket)
    );
}

#[tokio::test]
async fn recover_keeps_unremovable_ticket_store_and_names_remediation() {
    let sb = Sandbox::new();
    let store = sb.unreadable_ticket_store();
    let report = recover_at(&sb.staging_base(), &store).await;

    assert!(
        !report.ok(),
        "recovery cannot claim success it did not achieve"
    );
    let failed: Vec<_> = report
        .failed
        .iter()
        .filter(|f| f.kind == ResourceKind::RebootResumeTicket)
        .collect();
    assert_eq!(failed.len(), 1, "the residue it could not remove is kept");
    assert!(!failed[0].reason.is_empty(), "with an honest reason");
    assert!(
        failed[0]
            .remediation
            .contains(&store.path().display().to_string()),
        "and safe manual remediation naming the exact file"
    );
}

#[tokio::test]
async fn recover_resolves_a_malformed_but_removable_ticket_store() {
    let sb = Sandbox::new();
    let store = sb.malformed_ticket_store();

    // A scan cannot read it and says so honestly rather than assuming empty.
    let scan = scan_at(&sb.staging_base(), &store).await;
    assert!(
        scan.unreadable
            .iter()
            .any(|u| u.kind == ResourceKind::RebootResumeTicket),
        "the malformed store is surfaced as unknown before recovery"
    );

    // Recovery deletes the ordinary (if malformed) file, so it is truly gone:
    // the outcome is clean success, not a pessimistic failure.
    let report = recover_at(&sb.staging_base(), &store).await;
    assert!(
        report.ok(),
        "removing the malformed file leaves a clean result"
    );
    assert!(report.failed.is_empty());
    assert!(report.unreadable.is_empty());
    assert!(
        !store.path().exists(),
        "the malformed store file is deleted"
    );
}
