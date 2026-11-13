//! Tests of the explorer commands (ST-M010-002): the contract of the window,
//! the words of a failure, the listing that is left for the next, and both
//! commands against a real loopback session with an in-process agent that
//! serves its real file store without administrator rights.
//!
//! The wire accepts a path only when it is rooted on a drive, so a folder can
//! be opened by its path only where such a path names a real folder: that
//! test runs on Windows. Everywhere, the commands are driven through the
//! session for the drive list, for a place that is refused, and for the end
//! of the session.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use cretlink_console::{AuditEntry, AuditReceiver, ChannelAudit, ConsoleSessionService};
use cretlink_protocol::{CAP_FS_V1, CAP_FS_V2, PROTOCOL_V1, TargetIdentity};
use cretlink_session::{
    ConsoleHints, HostHandle, JoinHandle, SessionEvent, SessionHost, SessionJoiner,
};
use cretlink_shared::{Actor, Outcome};
use serde_json::json;
use tokio::time::timeout;

use super::super::session_driver::lock_service;
use super::*;

const STEP: Duration = Duration::from_secs(10);

/// A session under full control, as the pump publishes it: the application
/// state with the session's service and audit sink, the agent's side of the
/// session, and the records the commands write.
struct Live {
    state: AppState,
    service: ConsoleSessionService,
    _agent: JoinHandle,
    records: AuditReceiver,
}

impl Live {
    /// Every audit record written until now, in order.
    fn recorded(&mut self) -> Vec<AuditEntry> {
        let mut all = Vec::new();
        while let Ok(entry) = self.records.try_recv() {
            all.push(entry);
        }
        all
    }
}

async fn next_event(host: &mut HostHandle) -> SessionEvent {
    timeout(STEP, host.next_event())
        .await
        .expect("timed out waiting for a session event")
        .expect("the session ended unexpectedly")
}

/// Pairs a console with a consenting in-process agent and publishes the
/// session the way the pump does at full control.
async fn live() -> Live {
    let hints = ConsoleHints {
        label: "This computer (technician)".to_string(),
        display_name: None,
    };
    let mut host = SessionHost::host(
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        hints,
        PROTOCOL_V1,
        Vec::new(),
    )
    .await
    .expect("bind the console");
    let addr = host.local_addr().expect("the listener has an address");
    let identity = TargetIdentity {
        hostname: "CUSTOMER-PC".to_string(),
        os_version: "Simulated Windows".to_string(),
        arch: "x86_64".to_string(),
        elevated: false,
        simulated: true,
        agent_version: "explorer-command-test-agent".to_string(),
        account: None,
    };
    let agent = SessionJoiner::join(
        addr,
        identity,
        PROTOCOL_V1,
        vec![CAP_FS_V1.to_string(), CAP_FS_V2.to_string()],
    )
    .await
    .expect("dial the console");
    assert_eq!(next_event(&mut host).await, SessionEvent::Connected);
    assert_eq!(
        next_event(&mut host).await,
        SessionEvent::AwaitingApprovalCompare
    );
    host.confirm_code().expect("confirm the approval code");
    assert_eq!(next_event(&mut host).await, SessionEvent::AwaitingConsent);
    agent.grant_consent().expect("grant consent");
    let identity = match next_event(&mut host).await {
        SessionEvent::ConsentGranted { identity } => identity,
        other => panic!("expected ConsentGranted, got {other:?}"),
    };
    assert_eq!(next_event(&mut host).await, SessionEvent::Active);
    let session = host.session().expect("a session id once active");
    let (service, _driver) = ConsoleSessionService::start(host, session, identity);

    let state = AppState::live_idle();
    let (audit, records) = ChannelAudit::bounded(1024);
    *lock_service(&state.service) = Some(service.clone());
    *state.audit.lock().unwrap() = Some(audit);
    Live {
        state,
        service,
        _agent: agent,
        records,
    }
}

/// Runs `explorer_open` and collects what it delivered.
async fn opened(
    state: &AppState,
    path: &str,
    listing: u64,
) -> (Result<ListingView, CommandError>, Vec<ExplorerEvent>) {
    let mut events = Vec::new();
    let result = {
        let mut emit = |event| events.push(event);
        open(state, path, listing, &mut emit).await
    };
    (result, events)
}

fn failure(result: Result<impl std::fmt::Debug, CommandError>) -> (String, String) {
    match result {
        Err(error) => (error.stage, error.message),
        Ok(other) => panic!("expected a failure, got {other:?}"),
    }
}

// --- No authority of its own ---

/// Outside a session under full control both commands refuse before anything
/// is asked, and nothing is delivered.
#[tokio::test]
async fn without_a_session_both_commands_fail_closed() {
    let state = AppState::live_idle();
    let (stage, message) = failure(drives(&state).await);
    assert_eq!(stage, "no_consented_session");
    assert_eq!(message, "no session is under full control");

    let (result, events) = opened(&state, "C:\\Users\\ann", 1).await;
    assert_eq!(failure(result).0, "no_consented_session");
    assert!(events.is_empty());
}

/// A session without an open audit trail is no session for a file action:
/// nothing the technician does may miss the trail (REQ-S-017).
#[tokio::test]
async fn without_an_audit_trail_both_commands_fail_closed() {
    let mut live = live().await;
    *live.state.audit.lock().unwrap() = None;
    assert_eq!(failure(drives(&live.state).await).0, "no_consented_session");
    let (result, events) = opened(&live.state, "C:\\Users\\ann", 1).await;
    assert_eq!(failure(result).0, "no_consented_session");
    assert!(events.is_empty());
    assert!(live.recorded().is_empty());
    live.service.stop();
}

/// Through a session: the drives are listed for the window and recorded as
/// the technician's action.
#[tokio::test]
async fn the_drives_are_listed_through_the_session_and_recorded() {
    let mut live = live().await;
    let listed = drives(&live.state).await.expect("the drives are listed");
    assert!(!listed.is_empty(), "a computer has at least one drive");
    // Off Windows the one drive is the root file system.
    #[cfg(unix)]
    assert_eq!(
        listed
            .iter()
            .map(|drive| drive.root.as_str())
            .collect::<Vec<_>>(),
        ["/"]
    );
    let records = live.recorded();
    let actions: Vec<&str> = records.iter().map(|entry| entry.action).collect();
    assert_eq!(actions, ["console_fs_drives_start", "console_fs_drives"]);
    assert!(records.iter().all(|entry| entry.actor == Actor::Console));
    assert_eq!(records[1].outcome, Outcome::Ok);
    live.service.stop();
}

/// Through a session: a place that is refused delivers nothing, so the
/// window stays where it was, and the answer says why in plain words. The
/// refusal is recorded.
#[tokio::test]
async fn a_place_that_is_refused_delivers_nothing_and_says_why() {
    let mut live = live().await;

    // A drive the customer computer does not have.
    let (result, events) = opened(&live.state, "q:\\backup\\2026", 1).await;
    let (stage, message) = failure(result);
    assert_eq!(stage, "path_not_found");
    assert_eq!(message, "the customer computer lists no drive Q:");
    assert!(events.is_empty(), "{events:?}");
    let records = live.recorded();
    let last = records.last().expect("the look was recorded");
    assert_eq!(last.action, "console_fs_list");
    assert_eq!(last.outcome, Outcome::Failed);
    assert_eq!(last.actor, Actor::Console);

    // A text that is no path on a drive: refused before anything is asked.
    for (text, why) in [
        ("..\\Windows", "not a drive-qualified absolute path"),
        ("C:\\Users\\..\\Windows", "path traversal component"),
        ("\\\\server\\share", "UNC or device-namespace path"),
        ("", "empty path"),
    ] {
        let (result, events) = opened(&live.state, text, 2).await;
        let (stage, message) = failure(result);
        assert_eq!(stage, "invalid_path", "{text:?}");
        assert_eq!(
            message,
            format!("this is not a path on a drive of the customer computer: {why}"),
            "{text:?}"
        );
        assert!(events.is_empty(), "{text:?}");
        let records = live.recorded();
        assert_eq!(records.len(), 1, "{text:?}: {records:?}");
        assert_eq!(records[0].action, "console_fs_list_start");
        assert_eq!(records[0].outcome, Outcome::Failed);
    }
    live.service.stop();
}

/// A stop ends the session for the explorer too: every later command fails
/// closed, and says that no session is under full control.
#[tokio::test]
async fn after_a_stop_both_commands_fail_closed() {
    let live = live().await;
    drives(&live.state).await.expect("the session works");
    live.service.stop();
    assert_eq!(failure(drives(&live.state).await).0, "no_consented_session");
    let (result, events) = opened(&live.state, "C:\\Users\\ann", 3).await;
    assert_eq!(failure(result).0, "no_consented_session");
    assert!(events.is_empty());
}

/// Through a session, on Windows: a folder typed in another letter case
/// arrives under its real path, its first page with the place and the rest
/// in further pages, and the command answers that the listing is complete.
#[cfg(windows)]
#[tokio::test]
async fn a_folder_arrives_in_pages_under_its_real_path() {
    let mut live = live().await;
    // The temporary folder by its real names: a runner may give it by a
    // short name, which no listing shows.
    let base = std::env::temp_dir().join(format!("cretlink-app-explorer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("Documents")).expect("plant");
    let base = std::fs::canonicalize(&base).expect("the real path");
    let base = base.display().to_string();
    let base = base.strip_prefix("\\\\?\\").unwrap_or(&base).to_string();
    let documents = format!("{base}\\Documents");
    for n in 0..600 {
        std::fs::write(format!("{documents}\\file-{n:04}.txt"), b"x").expect("plant");
    }

    let (result, events) = opened(&live.state, &documents.to_lowercase(), 7).await;
    let answer = result.expect("the folder opens");
    assert_eq!(
        serde_json::to_value(&answer).unwrap(),
        json!({ "listing": 7, "path": documents, "entries": 600, "state": "complete" })
    );
    match &events[0] {
        ExplorerEvent::Opened {
            listing,
            path,
            select,
            entries,
            end,
        } => {
            assert_eq!(*listing, 7);
            assert_eq!(path, &documents, "the place as the computer names it");
            assert_eq!(*select, None);
            assert!(!entries.is_empty() && !*end);
        }
        other => panic!("the folder opens first, got {other:?}"),
    }
    let mut names = Vec::new();
    let mut ended = false;
    for event in &events {
        let (listing, entries, end) = match event {
            ExplorerEvent::Opened {
                listing,
                entries,
                end,
                ..
            }
            | ExplorerEvent::Page {
                listing,
                entries,
                end,
            } => (*listing, entries, *end),
        };
        assert_eq!(listing, 7);
        assert!(!ended, "nothing follows the last page");
        names.extend(entries.iter().map(|entry| entry.name.clone()));
        ended = end;
    }
    assert!(ended);
    assert_eq!(names.len(), 600);
    assert!(events.len() > 1, "600 entries take more than one page");

    let records = live.recorded();
    let done = records.last().expect("recorded");
    assert_eq!(done.action, "console_fs_list");
    assert_eq!(done.outcome, Outcome::Ok);

    live.service.stop();
    let _ = std::fs::remove_dir_all(&base);
}

// --- One listing at a time ---

#[test]
fn a_new_listing_leaves_the_one_before_it() {
    let listings = Listings::default();
    let first = listings.begin(1);
    assert!(!first.is_cancelled());
    let second = listings.begin(2);
    assert!(first.is_cancelled(), "the first folder was left");
    assert!(!second.is_cancelled());

    // The first listing ends late: the second one is still the one that runs.
    listings.end(1);
    let third = listings.begin(3);
    assert!(second.is_cancelled());

    // A listing that ended is not left again, and the drive list leaves the
    // one that runs.
    listings.end(3);
    listings.leave();
    assert!(!third.is_cancelled(), "it had ended by itself");
    let fourth = listings.begin(4);
    listings.leave();
    assert!(fourth.is_cancelled());
    // Nothing runs: leaving again does nothing.
    listings.leave();
}

// --- The contract of the window ---

#[test]
fn a_drive_and_an_entry_are_the_contract_of_the_window() {
    let drive = DriveView::from(DriveEntry {
        root: "C:\\".to_string(),
        kind: DriveKind::Fixed,
        label: Some("Windows".to_string()),
        total_bytes: Some(512_000_000_000),
        free_bytes: Some(128_000_000_000),
    });
    assert_eq!(
        serde_json::to_value(&drive).unwrap(),
        json!({
            "root": "C:\\",
            "kind": "fixed",
            "label": "Windows",
            "totalBytes": 512_000_000_000_u64,
            "freeBytes": 128_000_000_000_u64,
        })
    );
    // What the customer computer could not read is absent, never zero.
    let reader = DriveView::from(DriveEntry {
        root: "E:\\".to_string(),
        kind: DriveKind::Removable,
        label: None,
        total_bytes: None,
        free_bytes: None,
    });
    assert_eq!(
        serde_json::to_value(&reader).unwrap(),
        json!({ "root": "E:\\", "kind": "removable" })
    );
    for (kind, word) in [
        (DriveKind::Network, "network"),
        (DriveKind::Optical, "optical"),
        (DriveKind::RamDisk, "ram_disk"),
        (DriveKind::Unknown, "unknown"),
    ] {
        let drive = DriveView::from(DriveEntry {
            root: "Z:\\".to_string(),
            kind,
            label: None,
            total_bytes: None,
            free_bytes: None,
        });
        assert_eq!(serde_json::to_value(&drive).unwrap()["kind"], json!(word));
    }

    let file = EntryView::from(FileEntry {
        name: "report.pdf".to_string(),
        kind: FileKind::File,
        len: 48_213,
        modified_ms: Some(1_800_000_000_000),
        readonly: true,
    });
    assert_eq!(
        serde_json::to_value(&file).unwrap(),
        json!({
            "name": "report.pdf",
            "kind": "file",
            "len": 48_213,
            "modifiedMs": 1_800_000_000_000_u64,
            "readonly": true,
        })
    );
    for (kind, word) in [
        (FileKind::Dir, "dir"),
        (FileKind::Symlink, "symlink"),
        (FileKind::Other, "other"),
    ] {
        let entry = EntryView::from(FileEntry {
            name: "x".to_string(),
            kind,
            len: 0,
            modified_ms: None,
            readonly: false,
        });
        assert_eq!(
            serde_json::to_value(&entry).unwrap(),
            json!({ "name": "x", "kind": word, "len": 0, "readonly": false })
        );
    }
}

#[test]
fn the_steps_and_the_end_of_a_listing_are_the_contract_of_the_window() {
    let page = vec![FileEntry {
        name: "notes.txt".to_string(),
        kind: FileKind::File,
        len: 12,
        modified_ms: None,
        readonly: false,
    }];
    let folder = RemotePath::Absolute {
        path: "C:\\Users\\ann\\Documents".to_string(),
    };
    let opened = ExplorerEvent::of(
        4,
        FolderEvent::Opened {
            folder: folder.clone(),
            select: Some("notes.txt".to_string()),
            entries: page.clone(),
            end: false,
        },
    );
    assert_eq!(
        serde_json::to_value(&opened).unwrap(),
        json!({
            "kind": "opened",
            "listing": 4,
            "path": "C:\\Users\\ann\\Documents",
            "select": "notes.txt",
            "entries": [{ "name": "notes.txt", "kind": "file", "len": 12, "readonly": false }],
            "end": false,
        })
    );
    let empty = ExplorerEvent::of(
        5,
        FolderEvent::Opened {
            folder: folder.clone(),
            select: None,
            entries: Vec::new(),
            end: true,
        },
    );
    assert_eq!(
        serde_json::to_value(&empty).unwrap(),
        json!({
            "kind": "opened",
            "listing": 5,
            "path": "C:\\Users\\ann\\Documents",
            "entries": [],
            "end": true,
        })
    );
    let further = ExplorerEvent::of(
        4,
        FolderEvent::Page {
            entries: page,
            end: true,
        },
    );
    assert_eq!(
        serde_json::to_value(&further).unwrap(),
        json!({
            "kind": "page",
            "listing": 4,
            "entries": [{ "name": "notes.txt", "kind": "file", "len": 12, "readonly": false }],
            "end": true,
        })
    );

    let report = |end, folder: Option<RemotePath>| FolderReport {
        folder,
        select: None,
        entries: 4321,
        end,
    };
    assert_eq!(
        serde_json::to_value(ListingView::of(
            4,
            &report(ListingEnd::Complete, Some(folder.clone()))
        ))
        .unwrap(),
        json!({
            "listing": 4,
            "path": "C:\\Users\\ann\\Documents",
            "entries": 4321,
            "state": "complete",
        })
    );
    assert_eq!(
        ListingView::of(4, &report(ListingEnd::Limit, Some(folder))).state,
        "limit"
    );
    // Left before the folder was found: no path to name.
    assert_eq!(
        serde_json::to_value(ListingView::of(4, &report(ListingEnd::Cancelled, None))).unwrap(),
        json!({ "listing": 4, "entries": 4321, "state": "cancelled" })
    );
}

// --- The words of a failure ---

/// The stage of a failure is its category as the wire spells it, for every
/// category there is.
#[test]
fn the_stage_of_a_failure_is_its_category_as_the_wire_spells_it() {
    for code in [
        ErrorCode::IncompatibleVersion,
        ErrorCode::MalformedMessage,
        ErrorCode::OversizedMessage,
        ErrorCode::LimitExceeded,
        ErrorCode::UnknownSession,
        ErrorCode::UnknownJob,
        ErrorCode::SessionRevoked,
        ErrorCode::Unsupported,
        ErrorCode::PathNotFound,
        ErrorCode::AccessDenied,
        ErrorCode::FileInUse,
        ErrorCode::InvalidPath,
        ErrorCode::AlreadyExists,
        ErrorCode::WrongPathKind,
        ErrorCode::DirNotEmpty,
        ErrorCode::WriteConflict,
        ErrorCode::IntegrityMismatch,
        ErrorCode::StorageExhausted,
        ErrorCode::OutsideStagingRoot,
        ErrorCode::Internal,
    ] {
        assert_eq!(
            json!(code_token(code)),
            serde_json::to_value(code).unwrap(),
            "{code:?}"
        );
    }
}

/// A refusal of the customer computer is put in the words the activity has
/// for it; a refusal the explorer made keeps its own, which name what stood
/// in the way; the end of the session and a lost connection are told apart.
#[test]
fn a_failure_is_put_in_the_words_of_the_application() {
    let from_target = |code| {
        explorer_error(TargetError::Remote {
            code,
            message: "failed to open the directory".to_string(),
            stage: ErrorStage::Execution,
        })
    };
    let denied = from_target(ErrorCode::AccessDenied);
    assert_eq!(denied.stage, "access_denied");
    assert_eq!(denied.message, "access was denied");
    let unsupported = from_target(ErrorCode::Unsupported);
    assert_eq!(unsupported.stage, "unsupported");
    assert_eq!(unsupported.message, "not supported on this computer");
    let gone = from_target(ErrorCode::PathNotFound);
    assert_eq!(
        (gone.stage.as_str(), gone.message.as_str()),
        ("path_not_found", "the path does not exist")
    );

    let own = explorer_error(TargetError::Remote {
        code: ErrorCode::InvalidPath,
        message: "C:\\Documents and Settings is a link, and a link is shown, not followed"
            .to_string(),
        stage: ErrorStage::Validation,
    });
    assert_eq!(own.stage, "invalid_path");
    assert_eq!(
        own.message,
        "C:\\Documents and Settings is a link, and a link is shown, not followed"
    );

    let ended = explorer_error(TargetError::NoConsentedSession);
    assert_eq!(ended.stage, "no_consented_session");
    let lost = explorer_error(TargetError::Disconnected);
    assert_eq!(lost.stage, "disconnected");
    assert!(lost.message.contains("was lost"), "{}", lost.message);
    let odd = explorer_error(TargetError::Unexpected);
    assert_eq!(odd.stage, "unexpected");
}

/// A staged folder, which the explorer does not open, would be named as the
/// activity names it.
#[test]
fn a_folder_is_named_by_its_path() {
    assert_eq!(
        shown(&RemotePath::Absolute {
            path: "D:\\Tools".to_string()
        }),
        "D:\\Tools"
    );
    assert_eq!(
        shown(&RemotePath::Staging {
            name: "unpacked".to_string()
        }),
        "unpacked (CretLink staging folder)"
    );
}
