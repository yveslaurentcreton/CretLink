//! The customer's end report and its saved copy (ST-M009-012, REQ-F-027).
//!
//! The report is built **in memory** when a live session ends, from what the
//! customer saw: the activity feed of this window, the agent's own teardown
//! report, who ended the session, this computer's hostname and what the
//! technician's computer said its user is called. It holds no session secret
//! — no approval code, no key, no connection code (REQ-S-017). Nothing is
//! written to disk unless the customer chooses **Save a copy**: then a save
//! dialog opens on `Documents\CretLink\` (created only at that moment, and
//! removed again if it stays empty because the customer cancels) with
//! `YYYY-MM-DD <hostname>.html` as the name, and the report is written as one
//! self-contained HTML file ([`cretlink_session::report::html`]). **Open**
//! opens only a file this process saved.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use cretlink_session::report::html;
use cretlink_session::{CustomerReport, CustomerReportInput, SessionEvent};
use tauri::{AppHandle, Manager, State, WebviewWindow};

use tokio::sync::mpsc::UnboundedReceiver;

use super::session_driver::{Intent, ViewSink, Wire, drain_intents, lock_current, now_unix_millis};
use super::{AppState, CommandError, ConsentSession};

/// How long the window waits, after the session's terminal event, for the
/// session task to finish its teardown — stop the jobs, sweep the staging
/// area, send its report — before reading the teardown report. Bounded, so a
/// stuck teardown ends in an honest "not confirmed" cleanup.
pub(crate) const TEARDOWN_WAIT: Duration = Duration::from_secs(15);

/// What the window gathers during one session for its end report.
#[derive(Debug, Clone, Default)]
pub(crate) struct ReportState {
    /// When pairing completed (the first `Connected`).
    started_unix_millis: Option<i64>,
    /// Whether full control was granted.
    control_granted: bool,
    /// The finished report, once the session has ended.
    report: Option<CustomerReport>,
}

impl ReportState {
    /// Notes what a lifecycle event tells the report.
    pub(crate) fn observe(&mut self, event: &SessionEvent, now_unix_millis: i64) {
        match event {
            SessionEvent::Connected => {
                self.started_unix_millis.get_or_insert(now_unix_millis);
            }
            SessionEvent::ConsentGranted { .. } => self.control_granted = true,
            _ => {}
        }
    }

    /// The finished report, if the session has ended.
    pub(crate) fn report(&self) -> Option<&CustomerReport> {
        self.report.as_ref()
    }
}

/// Builds the end report of a session that reached pairing. A dial or pairing
/// failure has nothing to report and yields none.
pub(crate) fn finish(
    session: &mut ConsentSession,
    handle: &impl Wire,
    hostname: String,
    now_unix_millis: i64,
) {
    let Some(started) = session.report.started_unix_millis else {
        return;
    };
    let input = CustomerReportInput {
        ended_by: handle.ended_by(),
        technician_name: session.technician_name.clone(),
        hostname,
        control_granted: session.report.control_granted,
        started_unix_millis: started,
        ended_unix_millis: now_unix_millis,
        cleanup: handle.cleanup_report(),
        activity: session.activity.entries(),
        kept_program: super::keep::kept_program(),
    };
    session.report.report = Some(CustomerReport::build(input));
}

/// Lets the session task finish its teardown after the window reached a
/// terminal state, folding the activity it still reports (the honest end of
/// stopped jobs), then builds the end report and renders the final view.
///
/// A Stop or decline still queued is sent first, so a local stop always
/// revokes on the wire before this waits (REQ-F-018); one pressed during the
/// wait is sent too. Only the state of this dial's `generation` is written: a
/// later dial's session is left alone.
pub(crate) async fn settle(
    handle: &mut impl Wire,
    shared: &Arc<Mutex<ConsentSession>>,
    generation: u64,
    sink: &dyn ViewSink,
    intents: &mut UnboundedReceiver<Intent>,
) {
    drain_intents(handle, intents);
    let deadline = tokio::time::Instant::now() + TEARDOWN_WAIT;
    while let Ok(Some(event)) = tokio::time::timeout_at(deadline, handle.next_event()).await {
        drain_intents(handle, intents);
        if let Some(mut session) = lock_current(shared, generation) {
            for entry in session.activity.apply(&event, now_unix_millis()) {
                sink.activity(&entry);
            }
        }
    }
    let now = now_unix_millis();
    let Some(mut session) = lock_current(shared, generation) else {
        return;
    };
    // Anything still running ended with the session, never as done.
    for entry in session.activity.session_ended(now) {
        sink.activity(&entry);
    }
    finish(&mut session, handle, super::detect_hostname(), now);
    sink.emit(&session.view());
}

/// The paths this process saved, the only ones [`open_saved_report`] opens.
#[derive(Debug, Clone, Default)]
pub(crate) struct SavedReports(Arc<Mutex<Vec<PathBuf>>>);

impl SavedReports {
    fn remember(&self, path: PathBuf) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(path);
    }

    fn find(&self, path: &str) -> Option<PathBuf> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|saved| saved.as_os_str() == std::ffi::OsStr::new(path))
            .cloned()
    }
}

/// The end report of the session that just ended, or `None` while a session
/// runs or before any session (ST-M009-012).
#[tauri::command]
pub(super) fn get_end_report(state: State<'_, AppState>) -> Option<CustomerReport> {
    state.guard().report.report().cloned()
}

/// The folder the save dialog opens on: `Documents\CretLink`.
fn reports_folder(documents: &Path) -> PathBuf {
    documents.join("CretLink")
}

/// Writes the report as one self-contained HTML file.
fn write_copy(path: &Path, report: &CustomerReport) -> std::io::Result<()> {
    std::fs::write(path, html::render(report))
}

/// **Save a copy**: asks where to save the end report, starting in
/// `Documents\CretLink\` with `YYYY-MM-DD <hostname>.html`, writes it as one
/// self-contained HTML file, and returns the saved path; `None` when the
/// customer cancels (ST-M009-012).
#[tauri::command]
pub(super) async fn save_report(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, AppState>,
) -> Result<Option<String>, CommandError> {
    let Some(report) = state.guard().report.report().cloned() else {
        return Err(CommandError::new(
            "report",
            "there is no session report to save yet",
        ));
    };
    let documents = app.path().document_dir().map_err(|_| {
        CommandError::new(
            "report",
            "the Documents folder of this account was not found",
        )
    })?;
    let folder = reports_folder(&documents);
    // Created only now that the customer asked for a copy.
    let created = !folder.exists();
    if created {
        std::fs::create_dir_all(&folder).map_err(|error| {
            CommandError::new(
                "report",
                format!(
                    "the folder {} could not be created: {error}",
                    folder.display()
                ),
            )
        })?;
    }
    let chosen = rfd::AsyncFileDialog::new()
        .set_title("Save a copy of the session report")
        .set_directory(&folder)
        .set_file_name(report.file_name())
        .add_filter("Web page", &["html"])
        .set_parent(&window)
        .save_file()
        .await;
    let Some(chosen) = chosen else {
        if created {
            // Leave nothing behind: an empty folder made for a cancelled save
            // goes again (`remove_dir` refuses a folder that is not empty).
            let _ = std::fs::remove_dir(&folder);
        }
        return Ok(None);
    };
    let path = chosen.path().to_path_buf();
    write_copy(&path, &report).map_err(|error| {
        CommandError::new(
            "report",
            format!(
                "the report could not be saved to {}: {error}",
                path.display()
            ),
        )
    })?;
    state.saved_reports.remember(path.clone());
    Ok(Some(path.display().to_string()))
}

/// **Open**: opens a report this process saved, with the platform opener.
#[tauri::command]
pub(super) fn open_saved_report(
    state: State<'_, AppState>,
    path: String,
) -> Result<(), CommandError> {
    let saved = state
        .saved_reports
        .find(&path)
        .ok_or_else(|| CommandError::new("report", "only a report saved here can be opened"))?;
    crate::opener::open(&saved).map_err(|error| {
        CommandError::new("report", format!("the report could not be opened: {error}"))
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use cretlink_protocol::TargetIdentity;
    use cretlink_session::CleanupState;

    use super::*;

    fn identity() -> TargetIdentity {
        TargetIdentity {
            hostname: "ACME-PC-07".to_owned(),
            os_version: "Windows 11".to_owned(),
            arch: "x86_64".to_owned(),
            elevated: true,
            simulated: false,
            agent_version: "0.0.0".to_owned(),
            account: None,
        }
    }

    #[test]
    fn the_report_state_notes_the_start_and_the_grant() {
        let mut state = ReportState::default();
        state.observe(&SessionEvent::Connected, 1_000);
        state.observe(&SessionEvent::Connected, 9_000);
        assert_eq!(state.started_unix_millis, Some(1_000));
        assert!(!state.control_granted);
        state.observe(
            &SessionEvent::ConsentGranted {
                identity: identity(),
            },
            2_000,
        );
        assert!(state.control_granted);
        assert!(state.report().is_none(), "no report before the end");
    }

    #[test]
    fn only_saved_paths_can_be_opened() {
        let saved = SavedReports::default();
        let path = std::env::temp_dir().join("2026-11-03 ACME-PC-07.html");
        saved.remember(path.clone());
        assert_eq!(saved.find(&path.display().to_string()), Some(path));
        assert_eq!(saved.find("/etc/passwd"), None);
        assert_eq!(saved.find(""), None);
    }

    #[test]
    fn the_dialog_starts_in_documents_cretlink() {
        assert_eq!(
            reports_folder(Path::new("/home/c/Documents")),
            Path::new("/home/c/Documents/CretLink")
        );
    }

    #[test]
    fn a_saved_copy_is_the_rendered_report() {
        let report = CustomerReport::build(CustomerReportInput {
            ended_by: None,
            technician_name: None,
            hostname: "ACME-PC-07".to_owned(),
            control_granted: false,
            started_unix_millis: 0,
            ended_unix_millis: 1_000,
            cleanup: None,
            activity: Vec::new(),
            kept_program: None,
        });
        assert_eq!(report.cleanup.state, CleanupState::Unknown);
        let dir = std::env::temp_dir().join(format!("cretlink-report-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(report.file_name());
        write_copy(&path, &report).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            html::render(&report)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
