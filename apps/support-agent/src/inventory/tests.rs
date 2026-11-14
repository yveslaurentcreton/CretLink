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

use cretlink_exec::{FileStore, OwnFolder};
use cretlink_protocol::{
    ArmedTicket, B64Bytes, CleanupPolicy, DigestAlgo, OpResult, Operation,
    REBOOT_RESUME_TICKET_ID_LEN, RemotePath, SessionId, TargetBinding, TargetIdentity, TicketId,
};
use sha2::{Digest, Sha256};

use super::{Places, ResourceKind, recover_at, recover_in, scan_at, scan_in};
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
        account: None,
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

    // Every kind this crate scans is accounted for by an honest empty
    // assertion — none omitted.
    let scanned: Vec<ResourceKind> = ResourceKind::ALL
        .into_iter()
        .filter(|kind| kind.scanned_here())
        .collect();
    for kind in &scanned {
        assert!(
            inventory.asserted_empty.contains(kind),
            "kind {} is asserted empty on a clean machine",
            kind.label()
        );
    }
    assert_eq!(inventory.asserted_empty.len(), scanned.len());
    // The webview profile is the application's (ST-M009-014), and so is an
    // unfinished keep of the support program (ST-M009-017): this scan cannot
    // see their folders, so it says nothing about them rather than "none".
    // The program that runs the verb adds the kinds.
    assert_eq!(
        ResourceKind::ALL.len() - scanned.len(),
        2,
        "only the webview profile and an unfinished keep are left to the caller"
    );
    assert!(!ResourceKind::WebviewProfile.scanned_here());
    assert!(!ResourceKind::UnfinishedKeep.scanned_here());
    assert!(
        !inventory
            .asserted_empty
            .contains(&ResourceKind::UnfinishedKeep)
    );
    assert!(
        !inventory
            .asserted_empty
            .contains(&ResourceKind::WebviewProfile)
    );
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

// --- what CretLink did not make (ST-M006-020, REQ-S-024) ---------------------

/// A link under the name of a session folder is residue that the inventory
/// names and never reads. The recovery removes the link and leaves what it
/// points at.
#[cfg(unix)]
#[tokio::test]
async fn what_cretlink_did_not_make_is_named_and_the_recovery_removes_it_whole() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    std::fs::create_dir_all(&base).expect("create the staging base");
    // A staged session lies elsewhere; a link in the staging base leads to it.
    let elsewhere = sb.root.join("elsewhere");
    let session = SessionId::new();
    let theirs = stage_artifact(&elsewhere, session, "theirs.bin", CleanupPolicy::Keep).await;
    let link = base.join(session.to_string());
    std::os::unix::fs::symlink(elsewhere.join(session.to_string()), &link).expect("link");
    let store = sb.ticket_store();

    let inventory = scan_at(&base, &store).await;

    assert!(!inventory.is_clean());
    assert_eq!(inventory.found.len(), 1, "{:?}", inventory.found);
    assert_eq!(inventory.found[0].kind, ResourceKind::NotCretLinks);
    assert_eq!(inventory.found[0].description, link.to_string_lossy());
    assert_eq!(inventory.found[0].session, None);
    assert!(
        !inventory
            .asserted_empty
            .contains(&ResourceKind::NotCretLinks)
    );
    assert!(
        inventory
            .asserted_empty
            .contains(&ResourceKind::StagedFiles),
        "nothing behind the link was read as staged"
    );
    let text = crate::recovery::inventory_text("cretlink", &inventory);
    assert!(
        text.contains(&format!(
            "  - entry that CretLink did not make: {}\n",
            link.display()
        )),
        "{text}"
    );

    let report = recover_at(&base, &store).await;

    assert!(report.ok(), "{:?}", report.failed);
    assert_eq!(report.removed.len(), 1);
    assert_eq!(report.removed[0].kind, ResourceKind::NotCretLinks);
    assert!(
        std::fs::symlink_metadata(&link).is_err(),
        "the link is gone"
    );
    assert!(theirs.exists(), "what it pointed at is not");
    assert!(scan_at(&base, &store).await.is_clean());
}

/// A manifest entry that names a file outside the staging area is never
/// removed, not even by the recovery. The report says that nothing is left to
/// do by hand: the file is not CretLink's.
#[tokio::test]
async fn a_manifest_entry_outside_the_staging_area_is_reported_and_never_removed() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    let victim = sb.root.join("important.txt");
    std::fs::write(&victim, b"keep me").expect("write");
    let planted = base.join(SessionId::new().to_string());
    std::fs::create_dir_all(&planted).expect("create");
    let entry = cretlink_protocol::StagedEntry {
        canonical_path: victim.to_string_lossy().into_owned(),
        source: "x".to_string(),
        len: 0,
        algo: DigestAlgo::Sha256,
        digest: B64Bytes::new(vec![0u8; 32]),
        cleanup: CleanupPolicy::OnSessionEnd,
        created_ms: 0,
    };
    let manifest = serde_json::json!({ "version": 1, "staged": [entry] });
    std::fs::write(
        planted.join("manifest.json"),
        serde_json::to_vec(&manifest).expect("json"),
    )
    .expect("write the manifest");
    let store = sb.ticket_store();

    let report = recover_at(&base, &store).await;

    assert_eq!(std::fs::read(&victim).expect("the file stays"), b"keep me");
    assert!(!report.ok(), "a recovery that left something out says so");
    assert!(report.removed.is_empty());
    assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
    assert_eq!(report.failed[0].description, victim.to_string_lossy());
    assert!(
        report.failed[0]
            .reason
            .starts_with("outside the staging area")
    );
    assert!(
        report.failed[0].remediation.starts_with("none"),
        "nobody is told to delete a file that is not CretLink's: {}",
        report.failed[0].remediation
    );

    // The entry was dropped, so the next recovery finds a clean machine.
    let again = recover_at(&base, &store).await;
    assert!(again.ok(), "{:?}", again.failed);
    assert!(victim.exists());
    assert!(scan_at(&base, &store).await.is_clean());
}

/// A ticket store in a folder that must be private is read and removed only
/// by a path of folders: with a link in the place of its folder, or of the
/// data folder above it, nothing behind the link is read or removed, and the
/// recovery says that it could not look.
#[cfg(unix)]
#[tokio::test]
async fn a_ticket_store_behind_a_link_is_not_read_and_nothing_behind_it_is_removed() {
    for link_is_the_data_folder in [false, true] {
        let sb = Sandbox::new();
        let data = sb.root.join("CretLink Data");
        let store = TicketStore::in_folder(OwnFolder::administrators_only(
            &data,
            data.join("reboot-resume"),
        ));
        // A real store lies elsewhere, where the path of the store leads
        // through the link.
        let elsewhere = sb.root.join("elsewhere");
        let (theirs, link) = if link_is_the_data_folder {
            let theirs = TicketStore::new(elsewhere.join("reboot-resume").join("tickets.json"));
            std::os::unix::fs::symlink(&elsewhere, &data).expect("link");
            (theirs, data.clone())
        } else {
            let theirs = TicketStore::new(elsewhere.join("tickets.json"));
            std::fs::create_dir_all(&data).expect("create");
            std::os::unix::fs::symlink(&elsewhere, data.join("reboot-resume")).expect("link");
            (theirs, data.join("reboot-resume"))
        };
        theirs.save(&[ticket_fixture(0x44, 7)]).expect("save");
        assert!(store.path().exists(), "the path leads to their store");

        assert!(store.load().is_err(), "nothing behind the link is read");
        assert!(
            store.remove().is_err(),
            "nothing is removed through the link"
        );
        assert!(
            store.save(&[ticket_fixture(0x55, 9)]).is_err(),
            "nothing is written through the link"
        );
        let scan = scan_at(&sb.staging_base(), &store).await;
        assert!(
            scan.unreadable
                .iter()
                .any(|unknown| unknown.kind == ResourceKind::RebootResumeTicket),
            "{:?}",
            scan.unreadable
        );

        let report = recover_at(&sb.staging_base(), &store).await;

        assert!(!report.ok(), "a recovery that could not look says so");
        let kept = theirs.load().expect("their store is untouched");
        assert_eq!(kept.len(), 1, "{:?}", report.failed);
        assert_eq!(kept[0].not_after_unix_millis, 7);
        assert!(
            std::fs::symlink_metadata(&link)
                .expect("the link is left as it is")
                .file_type()
                .is_symlink()
        );
    }
}

/// A link under the name of the ticket file is not read, and saving replaces
/// it instead of writing through it.
#[cfg(unix)]
#[tokio::test]
async fn a_ticket_file_that_is_a_link_is_not_read_and_not_written_through() {
    let sb = Sandbox::new();
    let data = sb.root.join("CretLink Data");
    let folder = OwnFolder::administrators_only(&data, data.join("reboot-resume"));
    let store = TicketStore::in_folder(folder);
    let elsewhere = sb.root.join("elsewhere.json");
    TicketStore::new(elsewhere.clone())
        .save(&[ticket_fixture(0x66, 3), ticket_fixture(0x67, 4)])
        .expect("save");
    let before = std::fs::read(&elsewhere).expect("read");
    std::fs::create_dir_all(data.join("reboot-resume")).expect("create");
    std::os::unix::fs::symlink(&elsewhere, store.path()).expect("link");

    assert!(store.load().is_err(), "nothing behind the link is read");

    store.save(&[ticket_fixture(0x68, 5)]).expect("save");

    assert_eq!(store.load().expect("load").len(), 1);
    assert!(
        !std::fs::symlink_metadata(store.path())
            .expect("the store")
            .file_type()
            .is_symlink(),
        "the store is a file of its own now"
    );
    assert_eq!(
        std::fs::read(&elsewhere).expect("read"),
        before,
        "the file the link pointed at was not written"
    );
}

/// A session whose manifest is damaged is unknown to the inventory, never
/// clean, and the recovery says what it kept.
#[tokio::test]
async fn a_session_with_a_damaged_manifest_is_unknown_and_kept_by_the_recovery() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    let session = SessionId::new();
    let artifact = stage_artifact(&base, session, "ours.bin", CleanupPolicy::OnSessionEnd).await;
    let root = base.join(session.to_string());
    std::fs::write(root.join("manifest.json"), b"this is not a manifest").expect("damage");
    let store = sb.ticket_store();

    let inventory = scan_at(&base, &store).await;

    assert!(!inventory.is_clean(), "unknown is not clean");
    assert!(
        !inventory
            .asserted_empty
            .contains(&ResourceKind::StagedFiles)
    );
    assert!(
        inventory
            .unreadable
            .iter()
            .any(|unknown| unknown.kind == ResourceKind::StagedFiles
                && unknown
                    .reason
                    .contains(&root.to_string_lossy().into_owned())),
        "{:?}",
        inventory.unreadable
    );

    let report = recover_at(&base, &store).await;

    assert!(!report.ok());
    assert!(report.removed.is_empty(), "{:?}", report.removed);
    assert!(artifact.exists());
    assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
    assert_eq!(report.failed[0].description, root.to_string_lossy());
    assert_eq!(report.failed[0].reason, "the manifest could not be read");
}

// --- the folder of an earlier release, and the two keepers (ST-M006-020) ------

/// Whether a folder that must be private can be made here: off Windows
/// always, on Windows only by an administrator, as continuous verification
/// is.
fn can_make_private() -> bool {
    !cfg!(windows) || std::env::var_os("CI").is_some()
}

/// The places of a sandbox: its staging area, and the folder of an earlier
/// release beside it.
fn places_with_an_earlier_release(sb: &Sandbox) -> (Places, PathBuf) {
    let earlier = sb.root.join("earlier").join("CretLink");
    let places = Places {
        staging: OwnFolder::same_account(sb.staging_base()),
        earlier_release: Some(earlier.clone()),
        out_of_reach: None,
        earlier_release_unknown: false,
    };
    (places, earlier)
}

/// What a release up to 0.24.0 left, with what an account without rights
/// could have put into it: a session with a manifest that names `victim`.
fn fill_earlier_release(earlier: &Path, victim: &Path) {
    let session = earlier.join("staging").join(SessionId::new().to_string());
    std::fs::create_dir_all(session.join(".parts")).expect("create");
    std::fs::write(session.join("tool.bin"), b"staged").expect("write");
    std::fs::write(session.join(".parts").join("half.part"), b"half").expect("write");
    let entry = cretlink_protocol::StagedEntry {
        canonical_path: victim.to_string_lossy().into_owned(),
        source: "x".to_string(),
        len: 0,
        algo: DigestAlgo::Sha256,
        digest: B64Bytes::new(vec![0u8; 32]),
        cleanup: CleanupPolicy::OnSessionEnd,
        created_ms: 0,
    };
    let manifest = serde_json::json!({ "version": 1, "staged": [entry] });
    std::fs::write(
        session.join("manifest.json"),
        serde_json::to_vec(&manifest).expect("json"),
    )
    .expect("write the manifest");
    std::fs::write(earlier.join("staging").join(".in-use.lock"), b"").expect("lock");
    std::fs::create_dir_all(earlier.join("reboot-resume")).expect("create");
    std::fs::write(
        earlier.join("reboot-resume").join("tickets.json"),
        b"not read",
    )
    .expect("write");
}

/// The folder of an earlier release is one resource, named by its path.
/// Nothing in it is read, and the recovery removes it whole.
#[tokio::test]
async fn the_folder_of_an_earlier_release_is_named_and_the_recovery_removes_it_whole() {
    let sb = Sandbox::new();
    let (places, earlier) = places_with_an_earlier_release(&sb);
    let victim = sb.root.join("important.txt");
    std::fs::write(&victim, b"keep me").expect("write");
    let store = sb.ticket_store();

    // Where no earlier release ran, the kind is checked and empty.
    let clean = scan_in(&places, &store).await;
    assert!(clean.is_clean());
    assert!(clean.asserted_empty.contains(&ResourceKind::EarlierRelease));

    fill_earlier_release(&earlier, &victim);
    let inventory = scan_in(&places, &store).await;

    assert!(!inventory.is_clean());
    assert_eq!(inventory.found.len(), 1, "{:?}", inventory.found);
    assert_eq!(inventory.found[0].kind, ResourceKind::EarlierRelease);
    assert_eq!(inventory.found[0].description, earlier.to_string_lossy());
    assert!(
        inventory
            .asserted_empty
            .contains(&ResourceKind::StagedFiles),
        "nothing in it was read as staged"
    );
    assert!(
        inventory
            .asserted_empty
            .contains(&ResourceKind::RebootResumeTicket),
        "nothing in it was read as a ticket"
    );
    let text = crate::recovery::inventory_text("cretlink", &inventory);
    assert!(
        text.contains(&format!(
            "  - folder of an earlier release: {}\n",
            earlier.display()
        )),
        "{text}"
    );

    let report = recover_in(&places, &store).await;

    assert!(report.ok(), "{:?}", report.failed);
    assert_eq!(report.removed.len(), 1);
    assert_eq!(report.removed[0].kind, ResourceKind::EarlierRelease);
    assert!(!earlier.exists(), "the folder is gone, with all it held");
    assert_eq!(
        std::fs::read(&victim).expect("what its manifest named stays"),
        b"keep me"
    );
    assert!(scan_in(&places, &store).await.is_clean());
}

/// A link in the folder of an earlier release is removed as a link, and a
/// link in the place of the folder too: what they point at stays.
#[cfg(unix)]
#[tokio::test]
async fn links_in_the_folder_of_an_earlier_release_are_never_followed() {
    let sb = Sandbox::new();
    let (places, earlier) = places_with_an_earlier_release(&sb);
    let theirs = sb.root.join("theirs");
    std::fs::create_dir_all(&theirs).expect("create");
    std::fs::write(theirs.join("important.txt"), b"keep me").expect("write");
    let store = sb.ticket_store();

    // A link inside it, in the place of a session folder.
    std::fs::create_dir_all(earlier.join("staging")).expect("create");
    std::os::unix::fs::symlink(
        &theirs,
        earlier.join("staging").join(SessionId::new().to_string()),
    )
    .expect("link");
    let report = recover_in(&places, &store).await;
    assert!(report.ok(), "{:?}", report.failed);
    assert!(!earlier.exists());
    assert!(theirs.join("important.txt").exists());

    // A link in its own place.
    std::os::unix::fs::symlink(&theirs, &earlier).expect("link");
    let inventory = scan_in(&places, &store).await;
    assert_eq!(inventory.found.len(), 1, "a link there is named too");
    let report = recover_in(&places, &store).await;
    assert!(report.ok(), "{:?}", report.failed);
    assert!(std::fs::symlink_metadata(&earlier).is_err());
    assert_eq!(
        std::fs::read(theirs.join("important.txt")).expect("still there"),
        b"keep me"
    );
}

/// A folder of an earlier release that this account may not remove is kept,
/// and the report says what removes it.
#[cfg(unix)]
#[tokio::test]
async fn a_folder_of_an_earlier_release_that_cannot_be_removed_is_kept_and_named() {
    use std::os::unix::fs::PermissionsExt;

    let sb = Sandbox::new();
    let (places, earlier) = places_with_an_earlier_release(&sb);
    let closed = earlier.join("staging");
    std::fs::create_dir_all(&closed).expect("create");
    std::fs::write(closed.join("tool.bin"), b"staged").expect("write");
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o555)).expect("close");
    let store = sb.ticket_store();

    let report = recover_in(&places, &store).await;

    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755)).expect("open");
    if report.ok() {
        // An account that the permissions do not hold for (root) removed it.
        assert!(!earlier.exists());
        return;
    }
    assert!(closed.join("tool.bin").exists());
    assert!(report.removed.is_empty(), "{:?}", report.removed);
    assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
    assert_eq!(report.failed[0].kind, ResourceKind::EarlierRelease);
    assert_eq!(report.failed[0].description, earlier.to_string_lossy());
    assert_eq!(
        report.failed[0].reason,
        "removing it needs administrator rights"
    );
    assert!(
        report.failed[0].remediation.contains("as an administrator"),
        "{}",
        report.failed[0].remediation
    );
    let text = crate::recovery::recovery_report_text(&report);
    assert!(text.contains("did NOT fully complete"), "{text}");
}

/// A program without administrator rights cannot read what a program with
/// them kept. Where that folder exists, the scan says that there is something
/// it cannot see, and neither the scan nor the recovery calls the machine
/// clean.
#[tokio::test]
async fn what_a_program_with_administrator_rights_kept_is_said_to_be_out_of_reach() {
    let sb = Sandbox::new();
    let theirs = sb.root.join("CretLink Data");
    let places = Places {
        staging: OwnFolder::same_account(sb.staging_base()),
        earlier_release: None,
        out_of_reach: Some(theirs.clone()),
        earlier_release_unknown: false,
    };
    let store = sb.ticket_store();

    // No such folder: no program with administrator rights keeps anything.
    let clean = scan_in(&places, &store).await;
    assert!(clean.is_clean());
    assert!(clean.asserted_empty.contains(&ResourceKind::StagedFiles));

    std::fs::create_dir_all(theirs.join("staging")).expect("create");
    std::fs::write(theirs.join("staging").join("kept.bin"), b"kept").expect("write");
    let inventory = scan_in(&places, &store).await;

    assert!(!inventory.is_clean(), "unknown is not clean");
    assert!(inventory.found.is_empty());
    assert!(
        !inventory
            .asserted_empty
            .contains(&ResourceKind::StagedFiles)
    );
    assert_eq!(inventory.unreadable.len(), 1, "{:?}", inventory.unreadable);
    assert_eq!(inventory.unreadable[0].kind, ResourceKind::StagedFiles);
    assert!(
        inventory.unreadable[0]
            .reason
            .contains(&theirs.to_string_lossy().into_owned())
            && inventory.unreadable[0]
                .reason
                .contains("as an administrator"),
        "{}",
        inventory.unreadable[0].reason
    );
    let text = crate::recovery::inventory_text("cretlink", &inventory);
    assert!(!text.contains("is clean"), "{text}");

    let report = recover_in(&places, &store).await;

    assert!(!report.ok(), "a recovery that could not look says so");
    assert_eq!(report.unreadable.len(), 1);
    assert!(
        theirs.join("staging").join("kept.bin").exists(),
        "what is out of reach is not touched"
    );
    let text = crate::recovery::recovery_report_text(&report);
    assert!(text.contains("did NOT fully complete"), "{text}");
}

/// A recovery that left nothing removes CretLink's own folders too, as far as
/// they are empty: the lock of the staging area, the staging area, the folder
/// of the ticket store and the data folder above them.
#[tokio::test]
async fn a_recovery_that_left_nothing_leaves_no_empty_folder_behind() {
    if !can_make_private() {
        return;
    }
    let sb = Sandbox::new();
    let data = sb.root.join("CretLink Data");
    let staging = OwnFolder::administrators_only(&data, data.join("staging"));
    let store = TicketStore::in_folder(OwnFolder::administrators_only(
        &data,
        data.join("reboot-resume"),
    ));
    let places = Places::only(staging.clone());
    // What a session and a start leave: a staged file, a ticket, the lock.
    let session = SessionId::new();
    let file_store = FileStore::with_base(session, staging.clone());
    let write = file_store
        .perform(Operation::FsWrite {
            dest: RemotePath::Staging {
                name: "tool.bin".to_string(),
            },
            offset: 0,
            data: B64Bytes::new(b"staged".to_vec()),
        })
        .await
        .expect("staging write");
    assert!(matches!(write, OpResult::WriteAccepted { .. }));
    store.save(&[ticket_fixture(0x21, 5)]).expect("save");
    let lock = staging.path().join(crate::recovery::IN_USE_LOCK);
    std::fs::write(&lock, b"").expect("lock");

    // While a program holds its share of the lock, its folders stay.
    let running = std::fs::File::open(&lock).expect("open");
    running.lock_shared().expect("share");
    let report = recover_in(&places, &store).await;
    assert!(report.ok(), "{:?}", report.failed);
    assert!(lock.exists(), "the lock of a running program stays");
    assert!(staging.path().is_dir());
    drop(running);

    // Nothing is left to remove; no program runs.
    let report = recover_in(&places, &store).await;

    assert!(report.ok(), "{:?}", report.failed);
    assert!(
        !data.exists(),
        "not even an empty folder of CretLink is left"
    );
    assert!(sb.root.is_dir(), "the folder above is not CretLink's");
    assert!(scan_in(&places, &store).await.is_clean());
}

/// What a recovery could not remove stays, with its folders.
#[tokio::test]
async fn a_recovery_that_kept_something_keeps_its_folders() {
    let sb = Sandbox::new();
    let base = sb.staging_base();
    let session = SessionId::new();
    stage_artifact(&base, session, "ours.bin", CleanupPolicy::OnSessionEnd).await;
    let root = base.join(session.to_string());
    std::fs::write(root.join("manifest.json"), b"this is not a manifest").expect("damage");
    let store = sb.ticket_store();

    let report = recover_at(&base, &store).await;

    assert!(!report.ok());
    assert!(root.join("ours.bin").exists());
}

/// Where the place of an earlier release cannot be told, nothing is known
/// about it: neither the scan nor the recovery calls the machine clean.
#[tokio::test]
async fn an_earlier_release_whose_place_cannot_be_told_is_unknown_not_absent() {
    let sb = Sandbox::new();
    let places = Places {
        earlier_release_unknown: true,
        ..Places::only(OwnFolder::same_account(sb.staging_base()))
    };
    let store = sb.ticket_store();

    let inventory = scan_in(&places, &store).await;

    assert!(!inventory.is_clean(), "unknown is not clean");
    assert!(
        !inventory
            .asserted_empty
            .contains(&ResourceKind::EarlierRelease)
    );
    assert!(
        inventory
            .unreadable
            .iter()
            .any(|unknown| unknown.kind == ResourceKind::EarlierRelease),
        "{:?}",
        inventory.unreadable
    );
    let report = recover_in(&places, &store).await;
    assert!(!report.ok(), "a recovery that could not look says so");
}

/// What lies out of reach is said also when the recovery kept something of
/// its own: the one says nothing about the other.
#[tokio::test]
async fn what_is_out_of_reach_is_said_also_when_something_else_was_kept() {
    let sb = Sandbox::new();
    let theirs = sb.root.join("CretLink Data");
    std::fs::create_dir_all(theirs.join("staging")).expect("create");
    let base = sb.staging_base();
    let session = SessionId::new();
    stage_artifact(&base, session, "ours.bin", CleanupPolicy::OnSessionEnd).await;
    let manifest = base.join(session.to_string()).join("manifest.json");
    std::fs::write(&manifest, b"this is not a manifest").expect("damage");
    let places = Places {
        out_of_reach: Some(theirs.clone()),
        ..Places::only(OwnFolder::same_account(base))
    };
    let store = sb.ticket_store();

    let report = recover_in(&places, &store).await;

    assert!(
        report
            .failed
            .iter()
            .any(|failure| failure.kind == ResourceKind::StagedFiles),
        "the recovery kept staged files of its own: {:?}",
        report.failed
    );
    assert!(
        report
            .unreadable
            .iter()
            .any(|unknown| unknown.reason.contains("as an administrator")),
        "failed: {:?}; unknown: {:?}",
        report.failed,
        report.unreadable
    );
}
