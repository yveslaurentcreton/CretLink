#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::activity::{ActivityItem, ActivitySource};

fn report(ok: bool, notes: &[&str]) -> CleanupReport {
    CleanupReport {
        ok,
        notes: notes.iter().map(|note| (*note).to_owned()).collect(),
    }
}

const TEARDOWN: &str = "session revoked by the peer; secure channel and session keys dropped";

// --- the cleanup rule ---

#[test]
fn a_clean_report_is_cleaned_up() {
    let result = CleanupResult::of_report(&report(true, &[TEARDOWN]));
    assert_eq!(result.state, CleanupState::CleanedUp);
    assert_eq!(result.residuals, 0);
    assert_eq!(
        result.notes,
        vec![CleanupNote {
            ok: true,
            text: TEARDOWN.to_owned()
        }]
    );
}

#[test]
fn residual_notes_are_counted_and_come_first() {
    let result = CleanupResult::of_report(&report(
        false,
        &[
            TEARDOWN,
            "staged artifact not removed: C:\\a.bin (access denied)",
            "staged artifact not removed: C:\\b.bin (in use)",
        ],
    ));
    assert_eq!(result.state, CleanupState::Residuals);
    assert_eq!(result.residuals, 2);
    assert!(!result.notes[0].ok && !result.notes[1].ok);
    assert!(result.notes[2].ok);
    assert_eq!(result.notes[2].text, TEARDOWN);
}

#[test]
fn a_counted_note_adds_its_residuals() {
    let result = CleanupResult::of_report(&report(
        false,
        &[
            "staged artifact not removed: C:\\a.bin (in use)",
            "staged artifacts not removed and not listed: 7 (and 0 other notes not sent)",
        ],
    ));
    assert_eq!(result.state, CleanupState::Residuals);
    assert_eq!(result.residuals, 8);
}

#[test]
fn a_failed_sweep_is_failed_and_listed_before_residuals() {
    let result = CleanupResult::of_report(&report(
        false,
        &[
            TEARDOWN,
            "staged artifact not removed: C:\\a.bin (in use)",
            "staging cleanup failed: the manifest could not be read",
        ],
    ));
    assert_eq!(result.state, CleanupState::Failed);
    assert_eq!(result.residuals, 1);
    assert!(
        result.notes[0]
            .text
            .starts_with(TEARDOWN_FAILED_NOTE_PREFIX)
    );
    assert!(result.notes[1].text.starts_with(RESIDUAL_NOTE_PREFIX));
}

#[test]
fn a_not_ok_report_without_a_named_residual_is_failed() {
    let result = CleanupResult::of_report(&report(false, &[TEARDOWN]));
    assert_eq!(result.state, CleanupState::Failed);
}

#[test]
fn an_ok_report_naming_a_residual_is_never_cleaned_up() {
    let result = CleanupResult::of_report(&report(
        true,
        &["staged artifact not removed: C:\\a.bin (in use)"],
    ));
    assert_eq!(result.state, CleanupState::Residuals);
}

#[test]
fn no_report_is_unknown_never_success() {
    let result = CleanupResult::unknown(Some("no report".to_owned()));
    assert_eq!(result.state, CleanupState::Unknown);
    assert!(!result.notes[0].ok);
    assert_eq!(
        serde_json::to_value(&result).unwrap(),
        serde_json::json!({
            "state": "unknown",
            "residuals": 0,
            "notes": [{ "ok": false, "text": "no report" }],
        })
    );
}

// --- fitting the report to the wire ---

#[test]
fn a_small_report_fits_unchanged_but_ordered() {
    let original = report(
        false,
        &[TEARDOWN, "staged artifact not removed: C:\\a.bin (in use)"],
    );
    let fitted = fit_for_wire(&original);
    assert_eq!(
        fitted.notes,
        vec![
            "staged artifact not removed: C:\\a.bin (in use)".to_owned(),
            TEARDOWN.to_owned()
        ]
    );
    assert!(!fitted.ok);
}

#[test]
fn an_oversized_report_keeps_every_residual_counted() {
    use cretlink_protocol::limits::{MAX_CLEANUP_NOTE_BYTES, MAX_CLEANUP_NOTES};

    let mut notes = vec![TEARDOWN.to_owned()];
    for n in 0..100 {
        notes.push(format!(
            "staged artifact not removed: C:\\f{n}.bin (in use)"
        ));
    }
    notes.push(format!(
        "staged artifact not removed: C:\\{}\\end.bin (in use)",
        "x".repeat(2000)
    ));
    notes.push("line\nbreak".to_owned());
    let original = CleanupReport { ok: false, notes };
    let fitted = fit_for_wire(&original);

    assert_eq!(fitted.notes.len(), MAX_CLEANUP_NOTES);
    assert!(
        fitted
            .notes
            .iter()
            .all(|note| note.len() <= MAX_CLEANUP_NOTE_BYTES && !note.contains('\n'))
    );
    // Every residual is still counted: the listed ones plus the counted note.
    let before = CleanupResult::of_report(&original);
    let after = CleanupResult::of_report(&fitted);
    assert_eq!(before.residuals, 101);
    assert_eq!(after.residuals, 101);
    assert_eq!(after.state, CleanupState::Residuals);
    // The message itself passes the wire validation.
    let message = cretlink_protocol::Message::Control {
        session: crate::SessionId::new(),
        control: cretlink_protocol::ControlMessage::CleanupReported {
            ok: fitted.ok,
            notes: fitted.notes,
        },
    };
    message.encode().expect("a fitted report is in contract");
}

#[test]
fn a_long_note_keeps_its_start_and_end() {
    let long = format!(
        "staged artifact not removed: C:\\{}\\end.bin (in use)",
        "é".repeat(600)
    );
    let fitted = fit_for_wire(&report(false, &[&long]));
    let note = &fitted.notes[0];
    assert!(note.len() <= cretlink_protocol::limits::MAX_CLEANUP_NOTE_BYTES);
    assert!(note.starts_with(RESIDUAL_NOTE_PREFIX));
    assert!(note.ends_with("end.bin (in use)"));
}

// --- who ended it ---

#[test]
fn ended_by_names_the_requester_of_a_revocation() {
    assert_eq!(
        EndedBy::of_revoke(RevokeReason::TechnicianRequested),
        EndedBy::Technician
    );
    assert_eq!(
        EndedBy::of_revoke(RevokeReason::CustomerRequested),
        EndedBy::Customer
    );
    assert_eq!(EndedBy::of_revoke(RevokeReason::Error), EndedBy::Failure);
    assert_eq!(EndedBy::of_revoke(RevokeReason::Expired), EndedBy::Failure);
    for ended_by in [
        EndedBy::Technician,
        EndedBy::Customer,
        EndedBy::ConnectionLost,
        EndedBy::Failure,
    ] {
        assert_eq!(EndedBy::parse(ended_by.as_str()), Some(ended_by));
        assert_eq!(
            serde_json::to_value(ended_by).unwrap(),
            serde_json::json!(ended_by.as_str())
        );
    }
}

// --- the customer report ---

const START: i64 = 1_761_141_600_000; // 2025-10-22T14:00:00.000Z

fn item(id: &str, kind: ActivityKind, state: ActivityState, operation: &str) -> ActivityEntry {
    ActivityEntry::Item {
        item: ActivityItem {
            id: id.to_owned(),
            source: ActivitySource::Ai,
            client_label: None,
            purpose: None,
            description: crate::activity::describe(kind, ActivitySource::Ai).to_owned(),
            kind,
            operation: operation.to_owned(),
            path: None,
            bytes: None,
            state,
            started_at: cretlink_shared::format_rfc3339_millis(START + 60_000),
            duration_ms: Some(1200),
        },
    }
}

const XSS: &str = "<script>alert('x')</script>&\"";

fn sample_input() -> CustomerReportInput {
    let mut hostile = item(
        "a",
        ActivityKind::Command,
        ActivityState::Failed {
            exit_code: Some(1),
            message: None,
        },
        &format!("run a command: echo {XSS}"),
    );
    if let ActivityEntry::Item { item } = &mut hostile {
        item.purpose = Some(format!("Check the {XSS} driver"));
    }
    CustomerReportInput {
        ended_by: Some(EndedBy::Customer),
        technician_name: Some(format!("Sam {XSS}")),
        hostname: "ACME-PC-07".to_owned(),
        control_granted: true,
        started_unix_millis: START,
        ended_unix_millis: START + 754_000,
        cleanup: Some(report(
            false,
            &[
                TEARDOWN,
                &format!("staged artifact not removed: C:\\Temp\\{XSS}.bin (in use)"),
            ],
        )),
        activity: vec![
            hostile,
            item(
                "b",
                ActivityKind::Command,
                ActivityState::Done { exit_code: Some(0) },
                "run a command: ipconfig",
            ),
            item(
                "c",
                ActivityKind::FileSent,
                ActivityState::Done { exit_code: None },
                "copy the file C:\\log.txt from the customer computer",
            ),
            item(
                "d",
                ActivityKind::FileReceived,
                ActivityState::Failed {
                    exit_code: None,
                    message: Some("the result is not known".to_owned()),
                },
                "copy a file to the customer computer at C:\\x.msi",
            ),
            item(
                "e",
                ActivityKind::ScreenObserved,
                ActivityState::Done { exit_code: None },
                "take a screenshot of display 0",
            ),
            ActivityEntry::Gap {
                count: 2,
                at: cretlink_shared::format_rfc3339_millis(START + 120_000),
            },
        ],
        kept_program: None,
    }
}

#[test]
fn the_customer_report_counts_what_happened() {
    let report = CustomerReport::build(sample_input());
    assert_eq!(
        report.counts,
        CustomerCounts {
            commands: 2,
            failed_commands: 1,
            files_sent: 1,
            // A transfer whose result is not known did not arrive.
            files_received: 0,
            screenshots: 1,
        }
    );
    assert_eq!(report.duration_ms, 754_000);
    assert_eq!(report.started_at, "2025-10-22T14:00:00.000Z");
    assert_eq!(report.cleanup.state, CleanupState::Residuals);
    assert_eq!(report.file_name(), "2025-10-22 ACME-PC-07.html");
}

#[test]
fn the_customer_report_serializes_to_the_contract() {
    let value = serde_json::to_value(CustomerReport::build(sample_input())).unwrap();
    for key in [
        "endedBy",
        "technicianName",
        "hostname",
        "controlGranted",
        "startedAt",
        "endedAt",
        "durationMs",
        "counts",
        "cleanup",
        "activity",
    ] {
        assert!(value.get(key).is_some(), "missing {key}");
    }
    assert_eq!(value["endedBy"], "customer");
    assert_eq!(
        value["counts"],
        serde_json::json!({
            "commands": 2,
            "failedCommands": 1,
            "filesSent": 1,
            "filesReceived": 0,
            "screenshots": 1,
        })
    );
}

#[test]
fn no_cleanup_report_is_unknown_in_the_customer_report() {
    let mut input = sample_input();
    input.cleanup = None;
    let report = CustomerReport::build(input);
    assert_eq!(report.cleanup.state, CleanupState::Unknown);
}

#[test]
fn the_file_name_is_safe_on_windows() {
    let mut input = sample_input();
    input.hostname = "a<b>:c/d\\e|f?g*h\"".to_owned();
    assert_eq!(
        CustomerReport::build(input).file_name(),
        "2025-10-22 a-b--c-d-e-f-g-h-.html"
    );
    let mut input = sample_input();
    input.hostname = "  ".to_owned();
    assert_eq!(
        CustomerReport::build(input).file_name(),
        "2025-10-22 this computer.html"
    );
}

/// A support program that the customer kept is on the computer on purpose:
/// the report says so in words, and never counts it among what a session
/// left behind (ST-M009-017, REQ-F-019).
#[test]
fn a_kept_program_is_said_to_be_there_on_purpose_and_is_no_residual() {
    let without = CustomerReport::build(sample_input());
    assert!(without.kept_program.is_none());
    assert!(
        serde_json::to_value(&without)
            .unwrap()
            .get("keptProgram")
            .is_none()
    );
    assert!(!html::render(&without).contains("kept there on purpose"));

    let kept = KeptProgram {
        location: format!("C:\\Program Files\\CretLink Support {XSS}"),
        version: Some("1.2.0".to_owned()),
    };
    let with = CustomerReport::build(CustomerReportInput {
        kept_program: Some(kept.clone()),
        ..sample_input()
    });
    // The cleanup result is what the session reported, with or without it.
    assert_eq!(with.cleanup, without.cleanup);
    assert_eq!(
        serde_json::to_value(&with).unwrap()["keptProgram"],
        serde_json::json!({ "location": kept.location, "version": "1.2.0" })
    );
    let page = html::render(&with);
    assert!(
        page.contains(
            ", version 1.2.0) because it was kept there on purpose. It was not left behind by \
             this session. To remove it, use Installed apps in the settings of Windows, or \
             start it and choose Remove from this computer.</p>"
        ),
        "{page}"
    );
    // The folder is text like every other string of the report.
    assert!(!page.contains(XSS));
    // Without a version that the record names, none is said.
    let unnamed = KeptProgram {
        location: "C:\\Program Files\\CretLink Support".to_owned(),
        version: None,
    };
    assert!(
        unnamed
            .words()
            .starts_with("CretLink Support is installed on this computer (C:\\Program Files\\CretLink Support) because")
    );
}

// --- the saved HTML copy ---

/// The committed snapshot of the saved copy. Regenerate it with
/// `CRETLINK_UPDATE_SNAPSHOTS=1 cargo test -p cretlink-session report` after a
/// deliberate change, and review the diff.
const SNAPSHOT: &str = include_str!("snapshots/customer_report.html");

#[test]
fn the_saved_copy_matches_its_snapshot() {
    let html = html::render(&CustomerReport::build(sample_input()));
    if std::env::var_os("CRETLINK_UPDATE_SNAPSHOTS").is_some() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/report/snapshots/customer_report.html");
        std::fs::write(path, &html).unwrap();
        return;
    }
    assert_eq!(html, SNAPSHOT);
}

#[test]
fn the_saved_copy_escapes_every_untrusted_string() {
    let html = html::render(&CustomerReport::build(sample_input()));
    assert!(!html.contains(XSS), "a hostile string must not survive raw");
    assert!(!html.contains("<script"), "no script element");
    assert!(html.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;&amp;&quot;"));
}

#[test]
fn the_saved_copy_is_self_contained() {
    let html = html::render(&CustomerReport::build(sample_input()));
    for forbidden in [
        "<script", "<link", "<img", "<iframe", "src=", "href=", "url(", "@import", "http:",
        "https:", " on",
    ] {
        // `on` attributes would be ` onclick=` and the like.
        let hit = if forbidden == " on" {
            html.contains(" onclick=") || html.contains(" onload=") || html.contains(" onerror=")
        } else {
            html.contains(forbidden)
        };
        assert!(!hit, "the saved copy must not contain {forbidden:?}");
    }
    assert!(html.contains("<style>"));
}

#[test]
fn the_saved_copy_states_problems_in_words() {
    let html = html::render(&CustomerReport::build(sample_input()));
    assert!(html.contains("<h1>You stopped the session</h1>"));
    assert!(html.contains("Cleanup: 1 item left behind"));
    assert!(html.contains("<span class=\"problem\">Problem:</span> staged artifact not removed"));
    assert!(html.contains("Stated purpose:"));
    assert!(html.contains("2 actions not shown here"));
}
