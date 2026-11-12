//! Mapping tests for the technician-side activity fold (ST-M009-011): every
//! audit actor, every MCP tool kind, read-only calls, in-place updates, the
//! client label, gaps, and the honest end of running items.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use cretlink_session::{ActivityEntry, ActivityItem, ActivityKind, ActivitySource, ActivityState};
use cretlink_shared::{Actor, AuditArgs, AuditValue, Outcome, format_rfc3339_millis};

use super::*;
use crate::ipc_server::AuditEntry;

const T0: i64 = 1_800_000_000_000;

fn record(
    actor: Actor,
    action: &'static str,
    outcome: Outcome,
    args: &[(&str, AuditValue)],
) -> AuditEntry {
    let mut entry = AuditEntry::event(actor, None, action, outcome);
    let mut map = AuditArgs::new();
    for (key, value) in args {
        map.insert(*key, value.clone());
    }
    entry.args = map;
    entry
}

fn text(value: &str) -> AuditValue {
    AuditValue::String(value.to_owned())
}

fn int(value: i64) -> AuditValue {
    AuditValue::Int(value)
}

fn item_of(entry: Option<ActivityEntry>) -> ActivityItem {
    match entry {
        Some(ActivityEntry::Item { item }) => item,
        other => panic!("expected an item, got {other:?}"),
    }
}

#[test]
fn client_labels_cover_the_known_clients_and_fall_back() {
    assert_eq!(client_label(Some("claude-code")), "Claude Code");
    assert_eq!(client_label(Some("Claude-Code")), "Claude Code");
    assert_eq!(client_label(Some("codex-mcp-client")), "Codex CLI");
    assert_eq!(client_label(Some("gemini-cli-mcp-client")), "Gemini CLI");
    assert_eq!(client_label(Some("claude-ai")), UNKNOWN_CLIENT_LABEL);
    assert_eq!(client_label(Some("my-own-agent")), "AI assistant");
    assert_eq!(client_label(None), "AI assistant");
}

/// One MCP tool case: its start and completion actions, its kind, its audit
/// arguments, and the operation text the item shows.
type ToolCase = (
    &'static str,
    &'static str,
    ActivityKind,
    Vec<(&'static str, AuditValue)>,
    &'static str,
);

/// Every MCP tool maps to its kind, with the AI as source, the client label,
/// and the stated purpose — read-only calls included.
#[test]
fn every_mcp_tool_maps_to_its_kind_including_read_only_calls() {
    let cases: &[ToolCase] = &[
        (
            "mcp_remote_system_info_start",
            "mcp_remote_system_info",
            ActivityKind::SystemInformation,
            vec![],
            "read system information",
        ),
        (
            "mcp_remote_exec_start",
            "mcp_remote_exec",
            ActivityKind::Command,
            vec![("command", text("Get-Date"))],
            "run a command: Get-Date",
        ),
        (
            "mcp_remote_write_stdin_start",
            "mcp_remote_write_stdin",
            ActivityKind::JobControl,
            vec![("job_id", int(3)), ("input_len", int(5))],
            "send 5 bytes of input to job 3",
        ),
        (
            "mcp_remote_cancel_start",
            "mcp_remote_cancel",
            ActivityKind::JobControl,
            vec![("job_id", int(3))],
            "stop job 3",
        ),
        (
            "mcp_remote_list_jobs_start",
            "mcp_remote_list_jobs",
            ActivityKind::JobControl,
            vec![],
            "list running jobs",
        ),
        (
            "mcp_remote_fs_stat_start",
            "mcp_remote_fs_stat",
            ActivityKind::FileOperation,
            vec![("path", text("C:\\a.txt"))],
            "look up C:\\a.txt",
        ),
        (
            "mcp_remote_fs_list_start",
            "mcp_remote_fs_list",
            ActivityKind::FileOperation,
            vec![("path", text("C:\\Logs"))],
            "list the folder C:\\Logs",
        ),
        (
            "mcp_remote_make_dir_start",
            "mcp_remote_make_dir",
            ActivityKind::FileOperation,
            vec![("path", text("C:\\New"))],
            "create the folder C:\\New",
        ),
        (
            "mcp_remote_move_start",
            "mcp_remote_move",
            ActivityKind::FileOperation,
            vec![("path", text("C:\\a")), ("to", text("C:\\b"))],
            "move C:\\a to C:\\b",
        ),
        (
            "mcp_remote_delete_start",
            "mcp_remote_delete",
            ActivityKind::FileOperation,
            vec![("path", text("C:\\a"))],
            "delete the file C:\\a",
        ),
        (
            "mcp_remote_remove_dir_start",
            "mcp_remote_remove_dir",
            ActivityKind::FileOperation,
            vec![("path", text("C:\\d"))],
            "remove the folder C:\\d",
        ),
        (
            "mcp_remote_upload_start",
            "mcp_remote_upload",
            ActivityKind::FileReceived,
            vec![("path", text("staging:wifi.zip"))],
            "copy a file to the customer computer at wifi.zip (CretLink staging folder)",
        ),
        (
            "mcp_remote_download_start",
            "mcp_remote_download",
            ActivityKind::FileSent,
            vec![("path", text("C:\\setup.log"))],
            "copy the file C:\\setup.log from the customer computer",
        ),
        (
            "mcp_remote_list_staged_start",
            "mcp_remote_list_staged",
            ActivityKind::FileOperation,
            vec![],
            "list the files CretLink staged",
        ),
        (
            "mcp_remote_cleanup_start",
            "mcp_remote_cleanup",
            ActivityKind::FileOperation,
            vec![("trigger", text("session_end"))],
            "remove files CretLink staged (session_end)",
        ),
        (
            "mcp_remote_capture_desktop_start",
            "mcp_remote_capture_desktop",
            ActivityKind::ScreenObserved,
            vec![("display", int(0))],
            "take a screenshot of display 0",
        ),
        (
            "mcp_remote_desktop_pointer_start",
            "mcp_remote_desktop_pointer",
            ActivityKind::ScreenControlled,
            vec![
                ("action", text("Click")),
                ("button", text("Left")),
                ("x", int(10)),
                ("y", int(20)),
            ],
            "pointer click with the left button at (10, 20)",
        ),
        (
            "mcp_remote_desktop_scroll_start",
            "mcp_remote_desktop_scroll",
            ActivityKind::ScreenControlled,
            vec![
                ("x", int(1)),
                ("y", int(2)),
                ("dx", int(0)),
                ("dy", int(-3)),
            ],
            "scroll by (0, -3) at (1, 2)",
        ),
        (
            "mcp_remote_desktop_key_start",
            "mcp_remote_desktop_key",
            ActivityKind::ScreenControlled,
            vec![("action", text("Press")), ("key", text("Enter"))],
            "key press Enter",
        ),
        (
            "mcp_remote_desktop_type_start",
            "mcp_remote_desktop_type",
            ActivityKind::ScreenControlled,
            vec![("text_len", int(8))],
            "type 8 bytes of text",
        ),
        (
            "mcp_remote_desktop_hotkey_start",
            "mcp_remote_desktop_hotkey",
            ActivityKind::ScreenControlled,
            vec![("keys", text("ctrl+c"))],
            "press ctrl+c",
        ),
        (
            "mcp_remote_desktop_wait_start",
            "mcp_remote_desktop_wait",
            ActivityKind::ScreenControlled,
            vec![("millis", int(250))],
            "wait 250 ms",
        ),
        (
            "mcp_remote_clipboard_get_start",
            "mcp_remote_clipboard_get",
            ActivityKind::ClipboardRead,
            vec![],
            "read the clipboard text",
        ),
        (
            "mcp_remote_clipboard_set_start",
            "mcp_remote_clipboard_set",
            ActivityKind::ClipboardWritten,
            vec![("text_len", int(4))],
            "replace the clipboard text (4 bytes)",
        ),
    ];
    for (n, (start, finish, kind, args, operation)) in cases.iter().enumerate() {
        let mut fold = TechnicianActivity::new();
        let call = i64::try_from(n).unwrap() + 1;
        let mut start_args = args.clone();
        start_args.push(("call_id", int(call)));
        start_args.push(("purpose", text("Checking the printer")));
        start_args.push(("client", text("claude-code")));
        let running = item_of(fold.apply(&record(Actor::Mcp, start, Outcome::Ok, &start_args), T0));
        assert_eq!(running.id, format!("call-{call}"), "{start}");
        assert_eq!(running.kind, *kind, "{start}");
        assert_eq!(running.source, ActivitySource::Ai);
        assert_eq!(running.operation, *operation, "{start}");
        assert_eq!(running.purpose.as_deref(), Some("Checking the printer"));
        assert_eq!(running.client_label.as_deref(), Some("Claude Code"));
        assert_eq!(running.state, ActivityState::Running);
        assert_eq!(running.started_at, format_rfc3339_millis(T0));

        let mut done = record(Actor::Mcp, finish, Outcome::Ok, &start_args);
        done.duration_ms = Some(40);
        let finished = item_of(fold.apply(&done, T0 + 40));
        assert_eq!(finished.id, running.id, "{finish} updates in place");
        assert_eq!(finished.state, ActivityState::Done { exit_code: None });
        assert_eq!(finished.duration_ms, Some(40));
        assert_eq!(finished.started_at, running.started_at);
        assert_eq!(fold.entries().len(), 1);
    }
}

#[test]
fn a_finished_download_carries_its_byte_count_and_a_failure_its_reason() {
    let mut fold = TechnicianActivity::new();
    let start = [("call_id", int(9)), ("path", text("C:\\setup.log"))];
    fold.apply(
        &record(Actor::Mcp, "mcp_remote_download_start", Outcome::Ok, &start),
        T0,
    );
    let mut done = record(
        Actor::Mcp,
        "mcp_remote_download",
        Outcome::Ok,
        &[
            ("call_id", int(9)),
            ("path", text("C:\\setup.log")),
            ("len", int(1200)),
        ],
    );
    done.duration_ms = Some(12);
    let item = item_of(fold.apply(&done, T0 + 12));
    assert_eq!(item.kind, ActivityKind::FileSent);
    assert_eq!(item.bytes, Some(1200));
    assert_eq!(item.path.as_deref(), Some("C:\\setup.log"));
    assert_eq!(item.description, "Copied a file from the customer computer");

    // A failed transfer keeps the file's known byte count and says why; its
    // state, not the count, says that it did not finish.
    fold.apply(
        &record(
            Actor::Mcp,
            "mcp_remote_upload_start",
            Outcome::Ok,
            &[("call_id", int(10))],
        ),
        T0,
    );
    let failed = item_of(fold.apply(
        &record(
            Actor::Mcp,
            "mcp_remote_upload",
            Outcome::Failed,
            &[
                ("call_id", int(10)),
                ("len", int(99)),
                ("reason", text("the disk is full")),
            ],
        ),
        T0 + 1,
    ));
    assert_eq!(failed.bytes, Some(99));
    assert_eq!(failed.state, ActivityState::failed("the disk is full"));

    // A failure before the length was known shows no count rather than 0.
    fold.apply(
        &record(
            Actor::Mcp,
            "mcp_remote_download_start",
            Outcome::Ok,
            &[("call_id", int(11))],
        ),
        T0,
    );
    let unknown = item_of(fold.apply(
        &record(
            Actor::Mcp,
            "mcp_remote_download",
            Outcome::Failed,
            &[
                ("call_id", int(11)),
                ("len", int(0)),
                ("reason", text("not found")),
            ],
        ),
        T0 + 1,
    ));
    assert_eq!(unknown.bytes, None);
}

#[test]
fn exec_outcomes_map_honestly() {
    let cases = [
        (
            Outcome::Ok,
            Some(0),
            ActivityState::Done { exit_code: Some(0) },
        ),
        (
            Outcome::Failed,
            Some(1),
            ActivityState::Failed {
                exit_code: Some(1),
                message: None,
            },
        ),
        (Outcome::Cancelled, None, ActivityState::Cancelled),
        (Outcome::Timeout, None, ActivityState::TimedOut),
        (
            Outcome::Unknown,
            None,
            ActivityState::failed(cretlink_session::activity::UNKNOWN_RESULT),
        ),
    ];
    for (outcome, exit_code, expected) in cases {
        let mut fold = TechnicianActivity::new();
        fold.apply(
            &record(
                Actor::Mcp,
                "mcp_remote_exec_start",
                Outcome::Ok,
                &[("call_id", int(1)), ("command", text("x"))],
            ),
            T0,
        );
        let mut args = vec![("call_id", int(1))];
        if let Some(code) = exit_code {
            args.push(("exit_code", int(code)));
        }
        let item = item_of(fold.apply(
            &record(Actor::Mcp, "mcp_remote_exec", outcome, &args),
            T0 + 5,
        ));
        assert_eq!(item.state, expected, "{outcome:?}");
        assert_eq!(
            item.operation, "run a command: x",
            "the start's operation is kept"
        );
    }
}

#[test]
fn the_terminal_actor_opens_and_closes_terminals() {
    let mut fold = TechnicianActivity::new();
    // Over IPC: a start and a completion sharing a call id.
    let start = item_of(fold.apply(
        &record(
            Actor::Terminal,
            "terminal_open_start",
            Outcome::Ok,
            &[("call_id", int(4)), ("size", text("80x24"))],
        ),
        T0,
    ));
    assert_eq!(start.source, ActivitySource::Terminal);
    assert_eq!(start.kind, ActivityKind::Command);
    assert_eq!(start.description, "Used the terminal");
    assert_eq!(start.operation, "open an interactive terminal (80x24)");
    assert_eq!(start.purpose, None, "the terminal carries no purpose");
    assert_eq!(start.client_label, None);
    let opened = item_of(fold.apply(
        &record(
            Actor::Terminal,
            "terminal_open",
            Outcome::Ok,
            &[("call_id", int(4)), ("job_id", int(7))],
        ),
        T0 + 3,
    ));
    assert_eq!(opened.id, start.id);
    assert_eq!(opened.state, ActivityState::Done { exit_code: None });

    // In the GUI: a start without a correlation id is not paired, and the
    // completion stands alone.
    assert!(
        fold.apply(
            &record(Actor::Terminal, "terminal_open_start", Outcome::Ok, &[]),
            T0
        )
        .is_none()
    );
    let gui = item_of(fold.apply(
        &record(
            Actor::Terminal,
            "terminal_open",
            Outcome::Ok,
            &[("job_id", int(8)), ("size", text("100x30"))],
        ),
        T0,
    ));
    assert!(gui.id.starts_with("local-"));
    let closed = item_of(fold.apply(
        &record(
            Actor::Terminal,
            "terminal_close",
            Outcome::Ok,
            &[("job_id", int(8))],
        ),
        T0,
    ));
    assert_eq!(closed.kind, ActivityKind::JobControl);
    assert_eq!(closed.operation, "close terminal job 8");
    assert_ne!(closed.id, gui.id);

    // Attaching a local terminal view reaches nothing on the customer side.
    for action in [
        "terminal_attach_start",
        "terminal_attach",
        "terminal_detach",
    ] {
        assert!(
            fold.apply(
                &record(Actor::Terminal, action, Outcome::Ok, &[("call_id", int(5))]),
                T0
            )
            .is_none(),
            "{action}"
        );
    }
}

#[test]
fn the_console_actor_runs_refuses_and_cancels_manual_commands() {
    let mut fold = TechnicianActivity::new();
    let started = item_of(fold.apply(
        &record(
            Actor::Console,
            "console_exec_start",
            Outcome::Ok,
            &[("job_id", int(2)), ("command", text("ipconfig"))],
        ),
        T0,
    ));
    assert_eq!(started.id, "job-2");
    assert_eq!(started.source, ActivitySource::Technician);
    assert_eq!(started.operation, "run a command: ipconfig");
    let mut finish = record(
        Actor::Console,
        "console_exec_finish",
        Outcome::Ok,
        &[("job_id", int(2)), ("exit_code", int(0))],
    );
    finish.duration_ms = Some(900);
    let finished = item_of(fold.apply(&finish, T0 + 900));
    assert_eq!(finished.id, "job-2");
    assert_eq!(finished.state, ActivityState::Done { exit_code: Some(0) });
    assert_eq!(finished.duration_ms, Some(900));

    // A refused start is shown as failed at once, never as running.
    let refused = item_of(fold.apply(
        &record(
            Actor::Console,
            "console_exec_start",
            Outcome::Failed,
            &[("command", text("bad")), ("reason", text("not active"))],
        ),
        T0,
    ));
    assert_eq!(refused.state, ActivityState::failed("not active"));

    // A cancellation is its own item, never overwriting the command's.
    let cancel = item_of(fold.apply(
        &record(
            Actor::Console,
            "console_cancel",
            Outcome::Ok,
            &[("job_id", int(2))],
        ),
        T0,
    ));
    assert_ne!(cancel.id, "job-2");
    assert_eq!(cancel.kind, ActivityKind::JobControl);
    assert_eq!(fold.entries().len(), 3);
}

#[test]
fn lifecycle_records_are_not_actions() {
    let mut fold = TechnicianActivity::new();
    for (actor, action) in [
        (Actor::Console, "pairing_completed"),
        (Actor::Console, "approval_compare_confirmed"),
        (Actor::Agent, "consent_granted"),
        (Actor::System, "fs_cleanup"),
        (Actor::Mcp, "mcp_bridge_connect"),
    ] {
        assert!(
            fold.apply(&record(actor, action, Outcome::Ok, &[]), T0)
                .is_none(),
            "{action}"
        );
    }
    assert!(fold.entries().is_empty());
}

#[test]
fn an_unknown_client_and_no_client_are_the_ai_assistant() {
    let mut fold = TechnicianActivity::new();
    let item = item_of(fold.apply(
        &record(
            Actor::Mcp,
            "mcp_remote_list_jobs_start",
            Outcome::Ok,
            &[("call_id", int(1))],
        ),
        T0,
    ));
    assert_eq!(item.client_label.as_deref(), Some("AI assistant"));
}

#[test]
fn shed_records_become_a_gap_and_running_items_end_honestly() {
    let mut fold = TechnicianActivity::new();
    fold.apply(
        &record(
            Actor::Mcp,
            "mcp_remote_exec_start",
            Outcome::Ok,
            &[("call_id", int(1)), ("command", text("ping -t x"))],
        ),
        T0,
    );
    assert_eq!(
        fold.shed(4, T0 + 1),
        ActivityEntry::Gap {
            count: 4,
            at: format_rfc3339_millis(T0 + 1)
        }
    );
    let ended = fold.session_ended(T0 + 50);
    assert_eq!(ended.len(), 1);
    let item = item_of(ended.into_iter().next());
    assert_eq!(
        item.state,
        ActivityState::failed(cretlink_session::activity::SESSION_ENDED)
    );
    assert_eq!(item.duration_ms, Some(50));
    assert_eq!(fold.entries().len(), 2);
}

#[test]
fn the_gui_terminal_records_map_to_terminal_items() {
    let session = cretlink_protocol::SessionId::new();
    let mut fold = TechnicianActivity::new();
    assert!(
        fold.apply(&crate::terminal_open_started(session, 80, 24), T0)
            .is_none(),
        "an uncorrelated start is not paired"
    );
    let opened = item_of(fold.apply(
        &crate::terminal_opened(session, 80, 24, Ok(cretlink_protocol::JobId(3)), 40),
        T0 + 40,
    ));
    assert_eq!(opened.source, ActivitySource::Terminal);
    assert_eq!(opened.operation, "open an interactive terminal (80x24)");
    assert_eq!(opened.state, ActivityState::Done { exit_code: None });
    assert_eq!(opened.duration_ms, Some(40));
    let refused = item_of(fold.apply(
        &crate::terminal_opened(session, 80, 24, Err("no session is under full control"), 1),
        T0,
    ));
    assert_eq!(
        refused.state,
        ActivityState::failed("no session is under full control")
    );
    let closed = item_of(fold.apply(
        &crate::terminal_closed(session, cretlink_protocol::JobId(3), Ok(()), 5),
        T0,
    ));
    assert_eq!(closed.operation, "close terminal job 3");
}

/// The technician's own file actions (ST-M010-001): each is one item of the
/// technician, opened by its start record and closed by its completion, in
/// the words the customer's feed uses.
#[test]
fn the_file_actions_of_the_technician_are_items_of_the_technician() {
    let mut fold = TechnicianActivity::new();

    let drives = item_of(fold.apply(
        &record(
            Actor::Console,
            "console_fs_drives_start",
            Outcome::Ok,
            &[("action_id", int(1))],
        ),
        T0,
    ));
    assert_eq!(drives.id, "action-1");
    assert_eq!(drives.source, ActivitySource::Technician);
    assert_eq!(drives.kind, ActivityKind::FileOperation);
    assert_eq!(drives.operation, "list the drives");
    assert_eq!(drives.description, "Worked with files and folders");
    assert_eq!(drives.state, ActivityState::Running);
    assert_eq!(drives.client_label, None);
    assert_eq!(drives.purpose, None);
    let mut listed = record(
        Actor::Console,
        "console_fs_drives",
        Outcome::Ok,
        &[("action_id", int(1)), ("drives", int(2))],
    );
    listed.duration_ms = Some(8);
    let drives = item_of(fold.apply(&listed, T0 + 8));
    assert_eq!(drives.id, "action-1");
    assert_eq!(drives.state, ActivityState::Done { exit_code: None });
    assert_eq!(drives.duration_ms, Some(8));

    let removing = item_of(fold.apply(
        &record(
            Actor::Console,
            "console_remove_tree_start",
            Outcome::Ok,
            &[
                ("action_id", int(2)),
                ("path", text("C:\\Users\\ann\\Documents\\old")),
            ],
        ),
        T0 + 10,
    ));
    assert_eq!(removing.id, "action-2");
    assert_eq!(removing.kind, ActivityKind::FileOperation);
    assert_eq!(
        removing.operation,
        "delete the folder C:\\Users\\ann\\Documents\\old and everything in it"
    );
    assert_eq!(
        removing.path.as_deref(),
        Some("C:\\Users\\ann\\Documents\\old")
    );
    assert_eq!(removing.state, ActivityState::Running);
    let removed = item_of(fold.apply(
        &record(
            Actor::Console,
            "console_remove_tree",
            Outcome::Ok,
            &[
                ("action_id", int(2)),
                ("path", text("C:\\Users\\ann\\Documents\\old")),
                ("removed_files", int(40)),
                ("removed_dirs", int(3)),
            ],
        ),
        T0 + 900,
    ));
    assert_eq!(removed.id, "action-2");
    assert_eq!(removed.state, ActivityState::Done { exit_code: None });
    assert_eq!(removed.bytes, None);

    let sending = item_of(fold.apply(
        &record(
            Actor::Console,
            "console_send_start",
            Outcome::Ok,
            &[
                ("action_id", int(3)),
                ("path", text("D:\\Tools\\setup.exe")),
            ],
        ),
        T0 + 1000,
    ));
    assert_eq!(sending.id, "action-3");
    assert_eq!(sending.kind, ActivityKind::FileReceived);
    assert_eq!(
        sending.description,
        "Copied a file to the customer computer"
    );
    assert_eq!(
        sending.operation,
        "copy a file to the customer computer at D:\\Tools\\setup.exe"
    );
    let sent = item_of(fold.apply(
        &record(
            Actor::Console,
            "console_send",
            Outcome::Ok,
            &[
                ("action_id", int(3)),
                ("path", text("D:\\Tools\\setup.exe")),
                ("len", int(4096)),
            ],
        ),
        T0 + 1500,
    ));
    assert_eq!(sent.id, "action-3");
    assert_eq!(sent.bytes, Some(4096));
    assert_eq!(sent.state, ActivityState::Done { exit_code: None });
    assert_eq!(fold.entries().len(), 3);
}

/// A removal that left something behind, a cancelled one, one whose session
/// was lost and a send refused before it began are never shown as done.
#[test]
fn a_file_action_of_the_technician_ends_honestly() {
    let mut fold = TechnicianActivity::new();
    let start = |fold: &mut TechnicianActivity, id: i64| {
        fold.apply(
            &record(
                Actor::Console,
                "console_remove_tree_start",
                Outcome::Ok,
                &[("action_id", int(id)), ("path", text("staging:old"))],
            ),
            T0,
        )
    };
    let end = |fold: &mut TechnicianActivity, id: i64, outcome, reason: Option<&str>| {
        let mut args = vec![("action_id", int(id)), ("path", text("staging:old"))];
        if let Some(reason) = reason {
            args.push(("reason", text(reason)));
        }
        item_of(fold.apply(
            &record(Actor::Console, "console_remove_tree", outcome, &args),
            T0 + 5,
        ))
    };

    let opened = item_of(start(&mut fold, 1));
    assert_eq!(
        opened.operation,
        "delete the folder old (CretLink staging folder) and everything in it"
    );
    let left = end(
        &mut fold,
        1,
        Outcome::Failed,
        Some("2 entries could not be removed, the first: held (AccessDenied)"),
    );
    assert_eq!(
        left.state,
        ActivityState::failed("2 entries could not be removed, the first: held (AccessDenied)")
    );

    start(&mut fold, 2);
    assert_eq!(
        end(&mut fold, 2, Outcome::Cancelled, None).state,
        ActivityState::Cancelled
    );

    start(&mut fold, 3);
    assert_eq!(
        end(&mut fold, 3, Outcome::Unknown, None).state,
        ActivityState::failed(UNKNOWN_RESULT)
    );

    // A removal still running when the session ends is failed, not done.
    start(&mut fold, 4);
    let ended = fold.session_ended(T0 + 50);
    assert_eq!(ended.len(), 1);
    assert_eq!(
        item_of(ended.into_iter().next()).state,
        ActivityState::failed(SESSION_ENDED)
    );

    // A send refused before it began is one failed item; nothing was running.
    let refused = item_of(fold.apply(
        &record(
            Actor::Console,
            "console_send_start",
            Outcome::Failed,
            &[
                ("path", text("C:\\Users\\ann\\..\\x.txt")),
                ("reason", text("invalid path")),
            ],
        ),
        T0 + 60,
    ));
    assert_eq!(refused.kind, ActivityKind::FileReceived);
    assert_eq!(refused.state, ActivityState::failed("invalid path"));
    assert_eq!(fold.entries().len(), 5);
}

/// A file action of the technician and a routed AI call that happen to carry
/// the same number are two items: the two are numbered apart.
#[test]
fn a_technician_action_never_shares_an_item_with_a_routed_call() {
    let mut fold = TechnicianActivity::new();
    let ai = item_of(fold.apply(
        &record(
            Actor::Mcp,
            "mcp_remote_remove_dir_start",
            Outcome::Ok,
            &[("call_id", int(1)), ("path", text("C:\\a"))],
        ),
        T0,
    ));
    let technician = item_of(fold.apply(
        &record(
            Actor::Console,
            "console_remove_tree_start",
            Outcome::Ok,
            &[("action_id", int(1)), ("path", text("C:\\b"))],
        ),
        T0,
    ));
    assert_ne!(ai.id, technician.id);
    assert_eq!(ai.source, ActivitySource::Ai);
    assert_eq!(technician.source, ActivitySource::Technician);
    assert_eq!(fold.entries().len(), 2);

    // An `action_id` on a record of another actor is not a technician action.
    assert!(
        fold.apply(
            &record(
                Actor::Agent,
                "console_remove_tree_start",
                Outcome::Ok,
                &[("action_id", int(2)), ("path", text("C:\\c"))],
            ),
            T0,
        )
        .is_none()
    );
}
