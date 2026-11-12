//! Unit tests for the shared activity item and the customer-side fold
//! (ST-M009-011): the exact frontend serialization, the kind of every
//! operation, the bounded feed, and the fold of every activity event.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use cretlink_protocol::{
    B64Bytes, CleanupPolicy, CleanupTrigger, DigestAlgo, ErrorCode, ImageFormat, JobId, JobOutcome,
    KeyAction, Operation, Origin, PointerAction, PointerButton, Purpose, RemotePath, Shell,
};
use serde_json::json;

use super::*;
use crate::{ActionOutcome, RevokeReason, SessionEvent};

const T0: i64 = 1_800_000_000_000;

fn abs(path: &str) -> RemotePath {
    RemotePath::Absolute {
        path: path.to_owned(),
    }
}

fn item(id: &str, state: ActivityState) -> ActivityItem {
    ActivityItem {
        id: id.to_owned(),
        source: ActivitySource::Technician,
        client_label: None,
        purpose: None,
        description: "Ran a command".to_owned(),
        kind: ActivityKind::Command,
        operation: "run a command in Command Prompt: dir".to_owned(),
        path: None,
        bytes: None,
        state,
        started_at: format_rfc3339_millis(T0),
        duration_ms: None,
    }
}

// --- Serialization: exactly the frontend contract (contract.ts) ---

#[test]
fn an_item_serializes_to_the_contract_in_camel_case() {
    let full = ActivityItem {
        id: "req-7".to_owned(),
        source: ActivitySource::Ai,
        client_label: Some("Claude Code".to_owned()),
        purpose: Some("Reading the setup log".to_owned()),
        description: "Copied a file from the customer computer".to_owned(),
        kind: ActivityKind::FileSent,
        operation: "read the file C:\\setup.log".to_owned(),
        path: Some("C:\\setup.log".to_owned()),
        bytes: Some(1200),
        state: ActivityState::Done { exit_code: None },
        started_at: "2027-01-15T08:00:00.000Z".to_owned(),
        duration_ms: Some(42),
    };
    assert_eq!(
        serde_json::to_value(&full).unwrap(),
        json!({
            "id": "req-7",
            "source": "ai",
            "clientLabel": "Claude Code",
            "purpose": "Reading the setup log",
            "description": "Copied a file from the customer computer",
            "kind": "file_sent",
            "operation": "read the file C:\\setup.log",
            "path": "C:\\setup.log",
            "bytes": 1200,
            "state": { "state": "done" },
            "startedAt": "2027-01-15T08:00:00.000Z",
            "durationMs": 42,
        })
    );
    // The optional fields are omitted, never `null`.
    let minimal = serde_json::to_value(item("job-1", ActivityState::Running)).unwrap();
    for key in ["clientLabel", "purpose", "path", "bytes", "durationMs"] {
        assert!(minimal.get(key).is_none(), "{key} in {minimal}");
    }
    assert_eq!(minimal["state"], json!({ "state": "running" }));
}

#[test]
fn every_state_serializes_as_a_tagged_union() {
    let cases = [
        (ActivityState::Running, json!({ "state": "running" })),
        (
            ActivityState::Done { exit_code: Some(0) },
            json!({ "state": "done", "exitCode": 0 }),
        ),
        (
            ActivityState::Failed {
                exit_code: Some(2),
                message: Some("access was denied".to_owned()),
            },
            json!({ "state": "failed", "exitCode": 2, "message": "access was denied" }),
        ),
        (
            ActivityState::Failed {
                exit_code: None,
                message: None,
            },
            json!({ "state": "failed" }),
        ),
        (ActivityState::Cancelled, json!({ "state": "cancelled" })),
        (ActivityState::TimedOut, json!({ "state": "timed_out" })),
    ];
    for (state, expected) in cases {
        assert_eq!(serde_json::to_value(&state).unwrap(), expected);
        let back: ActivityState = serde_json::from_value(expected).unwrap();
        assert_eq!(back, state);
    }
}

#[test]
fn every_kind_and_source_serializes_in_snake_case() {
    let kinds = [
        (ActivityKind::Command, "command"),
        (ActivityKind::FileSent, "file_sent"),
        (ActivityKind::FileReceived, "file_received"),
        (ActivityKind::FileOperation, "file_operation"),
        (ActivityKind::ScreenObserved, "screen_observed"),
        (ActivityKind::ScreenControlled, "screen_controlled"),
        (ActivityKind::ClipboardRead, "clipboard_read"),
        (ActivityKind::ClipboardWritten, "clipboard_written"),
        (ActivityKind::SystemInformation, "system_information"),
        (ActivityKind::JobControl, "job_control"),
    ];
    for (kind, name) in kinds {
        assert_eq!(serde_json::to_value(kind).unwrap(), json!(name));
    }
    for (source, name) in [
        (ActivitySource::Ai, "ai"),
        (ActivitySource::Terminal, "terminal"),
        (ActivitySource::Technician, "technician"),
    ] {
        assert_eq!(serde_json::to_value(source).unwrap(), json!(name));
    }
}

#[test]
fn entries_are_tagged_item_or_gap() {
    let entry = ActivityEntry::Item {
        item: item("job-1", ActivityState::Running),
    };
    let value = serde_json::to_value(&entry).unwrap();
    assert_eq!(value["entry"], json!("item"));
    assert_eq!(value["item"]["id"], json!("job-1"));
    let gap = ActivityEntry::Gap {
        count: 3,
        at: "2027-01-15T08:00:00.000Z".to_owned(),
    };
    assert_eq!(
        serde_json::to_value(&gap).unwrap(),
        json!({ "entry": "gap", "count": 3, "at": "2027-01-15T08:00:00.000Z" })
    );
}

// --- The kind of every operation ---

#[test]
fn every_operation_has_its_kind() {
    let digest = B64Bytes::new(vec![0; 32]);
    let cases = [
        (Operation::SystemInfo {}, ActivityKind::SystemInformation),
        (
            Operation::Exec {
                shell: Shell::Cmd,
                command: "dir".to_owned(),
                working_dir: None,
                env: BTreeMap::new(),
                timeout_ms: None,
            },
            ActivityKind::Command,
        ),
        (
            Operation::TermOpen {
                shell: Shell::PowerShell,
                cols: 80,
                rows: 24,
                working_dir: None,
                env: BTreeMap::new(),
            },
            ActivityKind::Command,
        ),
        (
            Operation::WriteStdin {
                job: JobId(1),
                data: B64Bytes::new(vec![1]),
            },
            ActivityKind::JobControl,
        ),
        (Operation::ListJobs {}, ActivityKind::JobControl),
        (
            Operation::TermResize {
                job: JobId(1),
                cols: 80,
                rows: 24,
            },
            ActivityKind::JobControl,
        ),
        (
            Operation::TermClose { job: JobId(1) },
            ActivityKind::JobControl,
        ),
        (
            Operation::FsRead {
                path: abs("C:\\a"),
                offset: 0,
                len: 1,
            },
            ActivityKind::FileSent,
        ),
        (
            Operation::FsCommit {
                dest: abs("C:\\a"),
                len: 1,
                algo: DigestAlgo::Sha256,
                digest: digest.clone(),
                overwrite: false,
                source: None,
                cleanup: CleanupPolicy::Keep,
            },
            ActivityKind::FileReceived,
        ),
        (
            Operation::FsStat { path: abs("C:\\a") },
            ActivityKind::FileOperation,
        ),
        (
            Operation::FsList {
                path: abs("C:\\"),
                continue_after: None,
            },
            ActivityKind::FileOperation,
        ),
        (
            Operation::CreateDir { path: abs("C:\\d") },
            ActivityKind::FileOperation,
        ),
        (
            Operation::FsWrite {
                dest: abs("C:\\a"),
                offset: 0,
                data: B64Bytes::new(vec![1]),
            },
            ActivityKind::FileOperation,
        ),
        (
            Operation::FsAbort { dest: abs("C:\\a") },
            ActivityKind::FileOperation,
        ),
        (
            Operation::FsDigest {
                path: abs("C:\\a"),
                prefix_len: None,
            },
            ActivityKind::FileOperation,
        ),
        (
            Operation::FsMove {
                from: abs("C:\\a"),
                to: abs("C:\\b"),
                overwrite: false,
            },
            ActivityKind::FileOperation,
        ),
        (
            Operation::FsDelete { path: abs("C:\\a") },
            ActivityKind::FileOperation,
        ),
        (
            Operation::RemoveDir { path: abs("C:\\d") },
            ActivityKind::FileOperation,
        ),
        (Operation::ListStaged {}, ActivityKind::FileOperation),
        (
            Operation::StagingCleanup {
                trigger: CleanupTrigger::SessionEnd,
            },
            ActivityKind::FileOperation,
        ),
        (Operation::FsDrives {}, ActivityKind::FileOperation),
        (
            Operation::RemoveTree {
                path: abs("C:\\d"),
                stop: false,
            },
            ActivityKind::FileOperation,
        ),
        (
            Operation::RemoveTree {
                path: abs("C:\\d"),
                stop: true,
            },
            ActivityKind::FileOperation,
        ),
        (
            Operation::DesktopCapture {
                display: 0,
                format: ImageFormat::Png,
                max_width: None,
                max_height: None,
                quality: None,
            },
            ActivityKind::ScreenObserved,
        ),
        (
            Operation::DesktopPointer {
                action: PointerAction::Click,
                button: PointerButton::Left,
                x: 1,
                y: 2,
            },
            ActivityKind::ScreenControlled,
        ),
        (
            Operation::DesktopScroll {
                x: 1,
                y: 2,
                dx: 0,
                dy: 1,
            },
            ActivityKind::ScreenControlled,
        ),
        (
            Operation::DesktopKey {
                action: KeyAction::Press,
                key: "Enter".to_owned(),
            },
            ActivityKind::ScreenControlled,
        ),
        (
            Operation::DesktopType {
                text: "x".to_owned(),
            },
            ActivityKind::ScreenControlled,
        ),
        (
            Operation::DesktopHotkey {
                keys: vec!["ctrl".to_owned(), "c".to_owned()],
            },
            ActivityKind::ScreenControlled,
        ),
        (
            Operation::DesktopWait { millis: 10 },
            ActivityKind::ScreenControlled,
        ),
        (Operation::ClipboardGet {}, ActivityKind::ClipboardRead),
        (
            Operation::ClipboardSet {
                text: "x".to_owned(),
            },
            ActivityKind::ClipboardWritten,
        ),
    ];
    for (op, kind) in cases {
        assert_eq!(ActivityKind::of(&op), kind, "{op:?}");
        // Every kind has a plain description for every source.
        for source in [
            ActivitySource::Ai,
            ActivitySource::Terminal,
            ActivitySource::Technician,
        ] {
            assert!(!describe(kind, source).is_empty());
        }
    }
}

#[test]
fn path_and_bytes_come_from_the_request() {
    let commit = Operation::FsCommit {
        dest: abs("C:\\Drivers\\wifi.zip"),
        len: 18_400_000,
        algo: DigestAlgo::Sha256,
        digest: B64Bytes::new(vec![0; 32]),
        overwrite: false,
        source: None,
        cleanup: CleanupPolicy::Keep,
    };
    assert_eq!(path_of(&commit).as_deref(), Some("C:\\Drivers\\wifi.zip"));
    assert_eq!(bytes_of(&commit), Some(18_400_000));
    let moved = Operation::FsMove {
        from: abs("C:\\a"),
        to: abs("C:\\b"),
        overwrite: false,
    };
    assert_eq!(path_of(&moved).as_deref(), Some("C:\\a"));
    let staged = Operation::FsDelete {
        path: RemotePath::Staging {
            name: "frame.png".to_owned(),
        },
    };
    assert_eq!(
        path_of(&staged).as_deref(),
        Some("frame.png (CretLink staging folder)")
    );
    assert_eq!(path_of(&Operation::SystemInfo {}), None);
    assert_eq!(bytes_of(&Operation::SystemInfo {}), None);
    // A folder removal acts on its folder, with or without the stop; the
    // drive list acts on no path (ST-M010-001).
    for stop in [false, true] {
        let removal = Operation::RemoveTree {
            path: abs("C:\\Users\\ann\\Downloads\\old-setup"),
            stop,
        };
        assert_eq!(
            path_of(&removal).as_deref(),
            Some("C:\\Users\\ann\\Downloads\\old-setup")
        );
        assert_eq!(bytes_of(&removal), None);
    }
    assert_eq!(path_of(&Operation::FsDrives {}), None);
}

#[test]
fn source_follows_the_wire_origin() {
    assert_eq!(ActivitySource::from(Origin::Mcp), ActivitySource::Ai);
    assert_eq!(
        ActivitySource::from(Origin::Terminal),
        ActivitySource::Terminal
    );
    assert_eq!(
        ActivitySource::from(Origin::Console),
        ActivitySource::Technician
    );
}

#[test]
fn states_never_turn_an_unknown_outcome_into_success() {
    assert_eq!(
        ActivityState::of_outcome(ActionOutcome::Unknown),
        ActivityState::failed(UNKNOWN_RESULT)
    );
    assert_eq!(
        ActivityState::of_job(JobOutcome::Unknown, None),
        ActivityState::failed(UNKNOWN_RESULT)
    );
    assert_eq!(
        ActivityState::of_outcome(ActionOutcome::Failed {
            code: ErrorCode::AccessDenied
        }),
        ActivityState::failed("access was denied")
    );
    // A removal the technician cancelled is cancelled, not done and not failed.
    assert_eq!(
        ActivityState::of_outcome(ActionOutcome::Cancelled),
        ActivityState::Cancelled
    );
    assert_eq!(
        ActivityState::of_outcome(ActionOutcome::InProgress),
        ActivityState::Running
    );
    assert_eq!(
        ActivityState::of_job(JobOutcome::Completed, Some(1)),
        ActivityState::Done { exit_code: Some(1) }
    );
    assert_eq!(
        ActivityState::of_job(JobOutcome::TimedOut, None),
        ActivityState::TimedOut
    );
    assert_eq!(
        ActivityState::of_job(JobOutcome::Cancelled, None),
        ActivityState::Cancelled
    );
}

// --- The bounded feed ---

#[test]
fn the_feed_updates_an_item_in_place_by_id() {
    let mut feed = ActivityFeed::new();
    feed.upsert(item("job-1", ActivityState::Running));
    feed.upsert(item("job-2", ActivityState::Running));
    feed.upsert(item("job-1", ActivityState::Done { exit_code: Some(0) }));
    let entries = feed.entries();
    assert_eq!(entries.len(), 2);
    match &entries[0] {
        ActivityEntry::Item { item } => {
            assert_eq!(item.id, "job-1");
            assert_eq!(item.state, ActivityState::Done { exit_code: Some(0) });
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn a_full_feed_counts_what_it_evicts_into_a_leading_gap() {
    let mut feed = ActivityFeed::with_capacity(2);
    for n in 0..5 {
        feed.upsert(item(&format!("job-{n}"), ActivityState::Running));
    }
    let entries = feed.entries();
    assert_eq!(feed.len(), 2);
    assert_eq!(entries.len(), 3);
    assert!(matches!(&entries[0], ActivityEntry::Gap { count: 3, .. }));
    assert!(matches!(&entries[2], ActivityEntry::Item { item } if item.id == "job-4"));
}

// --- The customer-side fold ---

fn handled(
    id: u64,
    kind: ActivityKind,
    outcome: ActionOutcome,
    bytes: Option<u64>,
) -> SessionEvent {
    SessionEvent::RequestHandled {
        id,
        origin: Origin::Mcp,
        purpose: Purpose::new("Reading the setup log").ok(),
        kind,
        operation: "read the file C:\\setup.log (a copy goes to the technician's computer)"
            .to_owned(),
        path: Some("C:\\setup.log".to_owned()),
        bytes,
        outcome,
        duration_ms: (outcome != ActionOutcome::InProgress).then_some(250),
    }
}

fn only_item(entries: Vec<ActivityEntry>) -> ActivityItem {
    assert_eq!(entries.len(), 1, "{entries:?}");
    match entries.into_iter().next() {
        Some(ActivityEntry::Item { item }) => item,
        other => panic!("expected an item, got {other:?}"),
    }
}

#[test]
fn a_command_runs_then_finishes_in_place() {
    let mut fold = CustomerActivity::new();
    let started = only_item(fold.apply(
        &SessionEvent::CommandStarted {
            job: JobId(4),
            shell: Shell::PowerShell,
            command: "Get-Process".to_owned(),
            origin: Origin::Mcp,
            purpose: Purpose::new("Listing programs").ok(),
        },
        T0,
    ));
    assert_eq!(started.id, "job-4");
    assert_eq!(started.source, ActivitySource::Ai);
    assert_eq!(started.purpose.as_deref(), Some("Listing programs"));
    assert_eq!(
        started.operation,
        "run a command in PowerShell: Get-Process"
    );
    assert_eq!(started.state, ActivityState::Running);
    // The client label never exists on the customer side.
    assert_eq!(started.client_label, None);

    let finished = only_item(fold.apply(
        &SessionEvent::ExecFinished {
            job: JobId(4),
            exit_code: Some(0),
            outcome: JobOutcome::Completed,
        },
        T0 + 1500,
    ));
    assert_eq!(finished.id, "job-4");
    assert_eq!(finished.state, ActivityState::Done { exit_code: Some(0) });
    assert_eq!(finished.duration_ms, Some(1500));
    assert_eq!(finished.started_at, started.started_at);
    assert_eq!(fold.entries().len(), 1);

    // A job never seen starting has no item to finish.
    assert!(
        fold.apply(
            &SessionEvent::ExecFinished {
                job: JobId(99),
                exit_code: None,
                outcome: JobOutcome::Completed,
            },
            T0,
        )
        .is_empty()
    );
}

#[test]
fn a_technician_request_carries_no_purpose_and_its_source() {
    let mut fold = CustomerActivity::new();
    let item = only_item(fold.apply(
        &SessionEvent::RequestHandled {
            id: 1,
            origin: Origin::Terminal,
            purpose: None,
            kind: ActivityKind::Command,
            operation: "open an interactive PowerShell terminal".to_owned(),
            path: None,
            bytes: None,
            outcome: ActionOutcome::Succeeded,
            duration_ms: Some(30),
        },
        T0,
    ));
    assert_eq!(item.source, ActivitySource::Terminal);
    assert_eq!(item.description, "Used the terminal");
    assert_eq!(item.purpose, None);
    assert_eq!(item.started_at, format_rfc3339_millis(T0 - 30));
}

#[test]
fn a_file_sent_is_shown_when_it_starts_and_updated_when_it_ends() {
    let mut fold = CustomerActivity::new();
    let start = only_item(fold.apply(
        &handled(
            3,
            ActivityKind::FileSent,
            ActionOutcome::InProgress,
            Some(1200),
        ),
        T0,
    ));
    assert_eq!(start.id, "req-3");
    assert_eq!(start.kind, ActivityKind::FileSent);
    assert_eq!(start.state, ActivityState::Running);
    assert_eq!(start.bytes, Some(1200));
    assert_eq!(
        start.description,
        "Copied a file from the customer computer"
    );

    let end = only_item(fold.apply(
        &handled(
            3,
            ActivityKind::FileSent,
            ActionOutcome::Succeeded,
            Some(1200),
        ),
        T0 + 250,
    ));
    assert_eq!(end.id, "req-3");
    assert_eq!(end.state, ActivityState::Done { exit_code: None });
    assert_eq!(end.bytes, Some(1200));
    assert_eq!(end.started_at, start.started_at);
    assert_eq!(end.duration_ms, Some(250));
    assert_eq!(fold.entries().len(), 1, "updated in place");
}

#[test]
fn shed_events_become_a_gap_at_the_place_they_were_dropped() {
    let mut fold = CustomerActivity::new();
    fold.apply(
        &handled(
            1,
            ActivityKind::SystemInformation,
            ActionOutcome::Succeeded,
            None,
        ),
        T0,
    );
    let gap = fold.apply(&SessionEvent::ActivityShed { count: 7 }, T0 + 5);
    assert_eq!(
        gap,
        vec![ActivityEntry::Gap {
            count: 7,
            at: format_rfc3339_millis(T0 + 5)
        }]
    );
    fold.apply(
        &handled(
            2,
            ActivityKind::SystemInformation,
            ActionOutcome::Succeeded,
            None,
        ),
        T0 + 10,
    );
    let entries = fold.entries();
    assert_eq!(entries.len(), 3);
    assert!(matches!(entries[1], ActivityEntry::Gap { count: 7, .. }));
}

#[test]
fn running_items_fail_honestly_when_the_session_ends() {
    let mut fold = CustomerActivity::new();
    fold.apply(
        &SessionEvent::CommandStarted {
            job: JobId(1),
            shell: Shell::Cmd,
            command: "ping -t host".to_owned(),
            origin: Origin::Console,
            purpose: None,
        },
        T0,
    );
    fold.apply(
        &handled(
            5,
            ActivityKind::FileSent,
            ActionOutcome::InProgress,
            Some(10),
        ),
        T0,
    );
    let ended = fold.apply(
        &SessionEvent::Revoked {
            reason: RevokeReason::CustomerRequested,
        },
        T0 + 100,
    );
    assert_eq!(ended.len(), 2);
    for entry in ended {
        match entry {
            ActivityEntry::Item { item } => {
                assert_eq!(item.state, ActivityState::failed(SESSION_ENDED));
                assert_eq!(item.duration_ms, Some(100));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

// --- A folder removal on the customer's side (ST-M010-001) ---

fn removal(id: u64, outcome: ActionOutcome) -> SessionEvent {
    SessionEvent::RequestHandled {
        id,
        origin: Origin::Console,
        purpose: None,
        kind: ActivityKind::FileOperation,
        operation: "delete the folder C:\\Users\\ann\\Downloads\\old-setup and everything in it"
            .to_owned(),
        path: Some("C:\\Users\\ann\\Downloads\\old-setup".to_owned()),
        bytes: None,
        outcome,
        duration_ms: (outcome != ActionOutcome::InProgress).then_some(2400),
    }
}

/// A folder removal is one item with its kind and path: running from its
/// first step, and updated in place when it ends, with the state the folder
/// is really in.
#[test]
fn a_folder_removal_is_one_item_from_its_first_step_to_its_end() {
    for (ended, state) in [
        (
            ActionOutcome::Succeeded,
            ActivityState::Done { exit_code: None },
        ),
        (ActionOutcome::Cancelled, ActivityState::Cancelled),
        (
            ActionOutcome::Failed {
                code: ErrorCode::FileInUse,
            },
            ActivityState::failed("the file is in use"),
        ),
    ] {
        let mut fold = CustomerActivity::new();
        let running = only_item(fold.apply(&removal(5, ActionOutcome::InProgress), T0));
        assert_eq!(running.id, "req-5");
        assert_eq!(running.source, ActivitySource::Technician);
        assert_eq!(running.kind, ActivityKind::FileOperation);
        assert_eq!(running.description, "Worked with files and folders");
        assert_eq!(
            running.path.as_deref(),
            Some("C:\\Users\\ann\\Downloads\\old-setup")
        );
        assert_eq!(running.state, ActivityState::Running);
        assert_eq!(running.duration_ms, None);

        let done = only_item(fold.apply(&removal(5, ended), T0 + 2400));
        assert_eq!(done.id, "req-5");
        assert_eq!(done.state, state, "{ended:?}");
        assert_eq!(done.started_at, running.started_at);
        assert_eq!(done.duration_ms, Some(2400));
        assert_eq!(fold.entries().len(), 1, "one item, updated in place");
    }
}

/// A removal that the session ends before it finished is shown as failed
/// because the session ended, never as done (REQ-N-008).
#[test]
fn a_folder_removal_cut_short_by_the_end_of_the_session_is_not_done() {
    let mut fold = CustomerActivity::new();
    fold.apply(&removal(5, ActionOutcome::InProgress), T0);
    let ended = only_item(fold.apply(
        &SessionEvent::Revoked {
            reason: RevokeReason::CustomerRequested,
        },
        T0 + 900,
    ));
    assert_eq!(ended.id, "req-5");
    assert_eq!(ended.state, ActivityState::failed(SESSION_ENDED));
}
