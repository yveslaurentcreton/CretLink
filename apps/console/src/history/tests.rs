//! Audit-reader tests (ST-M009-012): several sessions newest first, a broken
//! chain, an unsealed file of an interrupted session, and the end and cleanup
//! records.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::Cell;
use std::fs::File;
use std::rc::Rc;

use cretlink_protocol::{JobId, JobOutcome, SessionId, TargetIdentity};
use cretlink_session::{CleanupReport, SessionEnd};
use cretlink_shared::{
    Actor, AuditArgs, AuditValue, AuditWriter, Clock, Outcome, session_file_name,
};

use super::*;
use crate::audit::{
    console_exec_finished, console_exec_started, end_entries, target_identity_entry,
    terminal_open_started, terminal_opened,
};
use crate::ipc_server::AuditEntry;

/// 2026-11-03T10:00:00.000Z
const T0: i64 = 1_793_700_000_000;

/// A clock that advances one second per record, so durations are real.
#[derive(Debug, Clone)]
struct StepClock(Rc<Cell<i64>>);

impl Clock for StepClock {
    fn now_unix_millis(&self) -> i64 {
        let now = self.0.get();
        self.0.set(now + 1000);
        now
    }
}

fn target() -> TargetIdentity {
    TargetIdentity {
        hostname: "ACME-PC-07".to_owned(),
        os_version: "Windows 11 Pro 24H2".to_owned(),
        arch: "x86_64".to_owned(),
        elevated: true,
        simulated: false,
        agent_version: "0.9.0".to_owned(),
        account: None,
    }
}

fn mcp(action: &'static str, outcome: Outcome, args: &[(&str, AuditValue)]) -> AuditEntry {
    let mut entry = AuditEntry::event(Actor::Mcp, None, action, outcome);
    let mut map = AuditArgs::new();
    for (key, value) in args {
        map.insert(*key, value.clone());
    }
    entry.args = map;
    entry
}

fn s(value: &str) -> AuditValue {
    AuditValue::String(value.to_owned())
}

fn i(value: i64) -> AuditValue {
    AuditValue::Int(value)
}

/// Writes one session file into `dir` from `entries`, sealed or not.
fn write_session(
    dir: &Path,
    start: i64,
    session: SessionId,
    entries: Vec<AuditEntry>,
    seal: bool,
) -> PathBuf {
    let path = dir.join(session_file_name(start, session));
    let file = File::create(&path).unwrap();
    let clock = StepClock(Rc::new(Cell::new(start)));
    let mut writer = AuditWriter::new_session(file, session, clock).unwrap();
    for entry in entries {
        writer
            .append_with(
                entry.actor,
                entry.action,
                entry.args,
                entry.outcome,
                entry.digests,
                entry.duration_ms,
            )
            .unwrap();
    }
    if seal {
        writer.close().unwrap();
    }
    path
}

/// A full session: consent, an AI command that failed, a download, an
/// upload, a terminal, a manual command, then the technician's stop and the
/// agent's report with one residual.
fn full_session(session: SessionId) -> Vec<AuditEntry> {
    let mut entries = vec![
        AuditEntry::event(Actor::Agent, None, "consent_granted", Outcome::Ok),
        target_identity_entry(&target()),
        mcp(
            "mcp_remote_exec_start",
            Outcome::Ok,
            &[("call_id", i(1)), ("command", s("sfc /scannow"))],
        ),
        mcp(
            "mcp_remote_exec",
            Outcome::Failed,
            &[("call_id", i(1)), ("exit_code", i(2))],
        ),
        mcp(
            "mcp_remote_download_start",
            Outcome::Ok,
            &[("call_id", i(2)), ("path", s("C:\\logs\\cbs.log"))],
        ),
        mcp(
            "mcp_remote_download",
            Outcome::Ok,
            &[("call_id", i(2)), ("len", i(4096))],
        ),
        mcp(
            "mcp_remote_upload_start",
            Outcome::Ok,
            &[("call_id", i(3)), ("path", s("C:\\Temp\\fix.msi"))],
        ),
        mcp(
            "mcp_remote_upload",
            Outcome::Ok,
            &[("call_id", i(3)), ("len", i(1024))],
        ),
        terminal_open_started(session, 80, 24),
        terminal_opened(session, 80, 24, Ok(JobId(9)), 15),
        console_exec_started(session, JobId(4), "ipconfig", false),
        console_exec_finished(session, JobId(4), Some(0), JobOutcome::Completed, Some(20)),
        AuditEntry::event(Actor::Console, None, "revoked", Outcome::Ok),
        AuditEntry::event(Actor::System, None, "cleanup", Outcome::Ok),
    ];
    entries.extend(end_entries(&SessionEnd {
        ended_by: EndedBy::Technician,
        action: "revoked",
        control_granted: true,
        peer_cleanup: Some(CleanupReport {
            ok: false,
            notes: vec![
                "session revoked by the peer; secure channel and session keys dropped".to_owned(),
                "staged artifact not removed: C:\\Temp\\fix.msi (in use)".to_owned(),
            ],
        }),
    }));
    entries
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cretlink-history-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn keep_forever() -> RetentionPolicy {
    RetentionPolicy {
        max_age_days: 0,
        max_total_bytes: 0,
    }
}

#[test]
fn a_full_session_is_summarized_from_its_records() {
    let dir = temp_dir("full");
    let session = SessionId::new();
    let path = write_session(&dir, T0, session, full_session(session), true);
    let summary = read_session(&path);

    assert_eq!(summary.id, session.to_string());
    assert_eq!(summary.file, path.display().to_string());
    assert_eq!(summary.integrity, Integrity::Verified);
    assert_eq!(summary.hostname.as_deref(), Some("ACME-PC-07"));
    assert_eq!(summary.os.as_deref(), Some("Windows 11 Pro 24H2"));
    assert_eq!(
        summary.started_at.as_deref(),
        Some("2026-11-03T10:00:00.000Z")
    );
    assert_eq!(summary.ended_by, Some(EndedBy::Technician));
    // The header is at T0 and every record one second later; `session_ended`
    // is the 15th record after the header.
    assert_eq!(
        summary.ended_at.as_deref(),
        Some("2026-11-03T10:00:15.000Z")
    );
    assert_eq!(summary.duration_ms, Some(15_000));
    assert_eq!(
        summary.actions,
        ActionCounts {
            ai: 3,
            terminal: 1,
            technician: 1,
        }
    );
    assert_eq!(summary.files_sent, 1);
    assert_eq!(summary.files_received, 1);
    assert_eq!(summary.failures, 1);
    assert_eq!(
        summary.first_failure.as_deref(),
        Some("run a command: sfc /scannow")
    );
    assert_eq!(summary.cleanup.state, CleanupState::Residuals);
    assert_eq!(summary.cleanup.residuals, 1);
    // Residuals first, with every note the agent sent.
    assert!(!summary.cleanup.notes[0].ok);
    assert!(summary.cleanup.notes[0].text.contains("fix.msi"));
    assert_eq!(summary.cleanup.notes.len(), 2);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_session_through_a_relay_names_the_relay_and_one_on_a_network_names_none() {
    let dir = temp_dir("relay");
    // On one network there is no route record, and the summary names no relay.
    let lan = SessionId::new();
    let on_a_network = read_session(&write_session(&dir, T0, lan, full_session(lan), true));
    assert_eq!(on_a_network.relay, None);
    assert!(
        serde_json::to_value(&on_a_network)
            .unwrap()
            .get("relay")
            .is_none()
    );

    // Through a relay the record stands before pairing and consent.
    let relayed = SessionId::new();
    let mut entries = vec![crate::audit::relay_route_entry("relay.example.org")];
    entries.extend(full_session(relayed));
    let through = read_session(&write_session(&dir, T0 + 60_000, relayed, entries, true));
    assert_eq!(through.relay.as_deref(), Some("relay.example.org"));
    assert_eq!(
        serde_json::to_value(&through).unwrap()["relay"],
        "relay.example.org"
    );
    // It is where the session travelled, and no action on the customer
    // computer: everything else of the summary is what it is on one network.
    assert_eq!(through.actions, on_a_network.actions);
    assert_eq!(through.failures, on_a_network.failures);
    assert_eq!(through.ended_by, on_a_network.ended_by);
    assert_eq!(through.cleanup, on_a_network.cleanup);
    assert_eq!(through.hostname, on_a_network.hostname);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_route_record_holds_the_transport_and_the_host_and_nothing_else() {
    let entry = crate::audit::relay_route_entry("relay.example.org");
    assert_eq!(entry.actor, Actor::Console);
    assert_eq!(entry.action, crate::audit::SESSION_ROUTE_ACTION);
    assert_eq!(entry.outcome, Outcome::Ok);
    assert_eq!(
        serde_json::to_value(&entry.args).unwrap(),
        serde_json::json!({
            "transport": { "string": "relay" },
            "relay_host": { "string": "relay.example.org" },
        })
    );
    assert!(entry.digests.is_empty());
}

#[test]
fn several_sessions_are_listed_newest_first() {
    let dir = temp_dir("several");
    let ids: Vec<SessionId> = (0..3).map(|_| SessionId::new()).collect();
    for (n, id) in ids.iter().enumerate() {
        let start = T0 + i64::try_from(n).unwrap() * 3_600_000;
        write_session(&dir, start, *id, full_session(*id), true);
    }
    // Something that is not an audit file is not a session.
    std::fs::write(dir.join("notes.txt"), "not a session").unwrap();

    let listed = list_sessions(&dir, &keep_forever(), T0);
    let listed_ids: Vec<&str> = listed.iter().map(|summary| summary.id.as_str()).collect();
    let expected: Vec<String> = ids.iter().rev().map(ToString::to_string).collect();
    assert_eq!(
        listed_ids,
        expected.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert!(
        listed
            .iter()
            .all(|summary| summary.integrity == Integrity::Verified)
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_broken_chain_is_listed_as_broken_from_its_verified_prefix() {
    let dir = temp_dir("broken");
    let good = SessionId::new();
    write_session(&dir, T0, good, full_session(good), true);
    let bad = SessionId::new();
    let path = write_session(&dir, T0 + 60_000, bad, full_session(bad), true);
    // A casual edit of the AI command's exit code breaks the chain there.
    let text = std::fs::read_to_string(&path).unwrap();
    let edited = text.replacen("{\"int\":2}", "{\"int\":0}", 1);
    assert_ne!(text, edited);
    std::fs::write(&path, edited).unwrap();

    let listed = list_sessions(&dir, &keep_forever(), T0);
    assert_eq!(listed.len(), 2, "a broken file is listed, never skipped");
    let broken = &listed[0];
    assert_eq!(broken.id, bad.to_string());
    assert_eq!(broken.integrity, Integrity::Broken);
    // Nothing after the break is trusted: no end, no cleanup, no success.
    assert_eq!(broken.ended_by, None);
    assert_eq!(broken.cleanup.state, CleanupState::Unknown);
    assert_eq!(broken.hostname.as_deref(), Some("ACME-PC-07"));
    assert_eq!(listed[1].integrity, Integrity::Verified);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_unreadable_file_is_listed_as_broken_with_the_id_from_its_name() {
    let dir = temp_dir("garbage");
    let session = SessionId::new();
    std::fs::write(dir.join(session_file_name(T0, session)), "not json\n").unwrap();
    let listed = list_sessions(&dir, &keep_forever(), T0);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, session.to_string());
    assert_eq!(listed[0].integrity, Integrity::Broken);
    assert_eq!(listed[0].cleanup.state, CleanupState::Unknown);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_interrupted_session_is_unsealed_and_never_cleaned_up() {
    let dir = temp_dir("unsealed");
    let session = SessionId::new();
    // The console stopped mid-call: no completion, no end records, no seal.
    let entries = vec![
        AuditEntry::event(Actor::Agent, None, "consent_granted", Outcome::Ok),
        target_identity_entry(&target()),
        mcp(
            "mcp_remote_exec_start",
            Outcome::Ok,
            &[("call_id", i(1)), ("command", s("chkdsk"))],
        ),
    ];
    let path = write_session(&dir, T0, session, entries, false);
    let summary = read_session(&path);
    assert_eq!(summary.integrity, Integrity::Unsealed);
    assert_eq!(summary.ended_by, None);
    assert_eq!(summary.cleanup.state, CleanupState::Unknown);
    assert_eq!(summary.actions.ai, 1);
    // The command never finished: a failure, never done.
    assert_eq!(summary.failures, 1);
    assert_eq!(
        summary.first_failure.as_deref(),
        Some("run a command: chkdsk")
    );
    // The last record stands for the end.
    assert_eq!(
        summary.ended_at.as_deref(),
        Some("2026-11-03T10:00:03.000Z")
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_session_without_control_needed_no_cleanup() {
    let dir = temp_dir("declined");
    let session = SessionId::new();
    let mut entries = vec![AuditEntry::event(
        Actor::Agent,
        None,
        "consent_denied",
        Outcome::Ok,
    )];
    entries.extend(end_entries(&SessionEnd {
        ended_by: EndedBy::Customer,
        action: "consent_denied",
        control_granted: false,
        peer_cleanup: None,
    }));
    let summary = read_session(&write_session(&dir, T0, session, entries, true));
    assert_eq!(summary.ended_by, Some(EndedBy::Customer));
    assert_eq!(summary.cleanup.state, CleanupState::CleanedUp);
    assert!(summary.cleanup.notes[0].text.contains("never granted"));
    assert_eq!(summary.hostname, None);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_lost_connection_without_a_report_is_unknown() {
    let dir = temp_dir("lost");
    let session = SessionId::new();
    let mut entries = vec![AuditEntry::event(
        Actor::Console,
        None,
        "disconnected",
        Outcome::Unknown,
    )];
    entries.extend(end_entries(&SessionEnd {
        ended_by: EndedBy::ConnectionLost,
        action: "disconnected",
        control_granted: true,
        peer_cleanup: None,
    }));
    let summary = read_session(&write_session(&dir, T0, session, entries, true));
    assert_eq!(summary.ended_by, Some(EndedBy::ConnectionLost));
    assert_eq!(summary.cleanup.state, CleanupState::Unknown);
    assert!(!summary.cleanup.notes[0].ok);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_end_records_are_written_as_documented() {
    let [ended, cleanup] = end_entries(&SessionEnd {
        ended_by: EndedBy::Customer,
        action: "revoked",
        control_granted: true,
        peer_cleanup: Some(CleanupReport {
            ok: true,
            notes: vec!["done".to_owned()],
        }),
    });
    assert_eq!(ended.action, "session_ended");
    assert_eq!(ended.args.0.get("ended_by"), Some(&s("customer")));
    assert_eq!(ended.args.0.get("how"), Some(&s("revoked")));
    assert_eq!(cleanup.action, "cleanup_report");
    assert_eq!(cleanup.actor, Actor::Agent);
    assert_eq!(cleanup.outcome, Outcome::Ok);
    assert_eq!(cleanup.args.0.get("source"), Some(&s("agent")));
    assert_eq!(
        cleanup.args.0.get("notes"),
        Some(&AuditValue::List(vec![s("done")]))
    );
}

#[test]
fn expired_files_are_outside_the_history() {
    let dir = temp_dir("retention");
    let session = SessionId::new();
    write_session(&dir, T0, session, full_session(session), true);
    let policy = RetentionPolicy {
        max_age_days: 1,
        max_total_bytes: 0,
    };
    // Judged by the file's modification time, like the prune.
    let now = cretlink_shared::SystemClock.now_unix_millis();
    assert_eq!(list_sessions(&dir, &policy, now).len(), 1);
    assert!(list_sessions(&dir, &policy, now + 2 * 86_400_000).is_empty());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_missing_folder_is_an_empty_history() {
    let dir = std::env::temp_dir().join("cretlink-history-does-not-exist");
    assert!(list_sessions(&dir, &keep_forever(), T0).is_empty());
}

#[test]
fn the_summary_serializes_to_the_contract() {
    let dir = temp_dir("contract");
    let session = SessionId::new();
    let summary = read_session(&write_session(
        &dir,
        T0,
        session,
        full_session(session),
        true,
    ));
    let value = serde_json::to_value(&summary).unwrap();
    for key in [
        "id",
        "file",
        "integrity",
        "hostname",
        "os",
        "startedAt",
        "endedAt",
        "durationMs",
        "endedBy",
        "actions",
        "filesSent",
        "filesReceived",
        "failures",
        "firstFailure",
        "cleanup",
    ] {
        assert!(value.get(key).is_some(), "missing {key}");
    }
    assert_eq!(value["integrity"], "verified");
    assert_eq!(value["endedBy"], "technician");
    assert_eq!(
        value["actions"],
        serde_json::json!({ "ai": 3, "terminal": 1, "technician": 1 })
    );
    assert_eq!(value["cleanup"]["state"], "residuals");
    assert_eq!(
        file_of(std::slice::from_ref(&summary), &summary.id),
        Some(PathBuf::from(&summary.file))
    );
    assert_eq!(file_of(&[summary], "someone-else"), None);
    std::fs::remove_dir_all(&dir).unwrap();
}
