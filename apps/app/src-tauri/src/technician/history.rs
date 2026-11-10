//! The technician's session history and audit files (ST-M009-012).
//!
//! [`list_sessions`] returns the summaries `cretlink_console::history` reads
//! from the audit folder, newest first within retention, and [`open_audit`]
//! opens the folder or one session's file with the platform opener.
//!
//! The session being written right now is not history yet: while its log is
//! open it is left out, and once its session has ended the list waits a
//! bounded moment for the seal, so the end report reads the sealed file
//! rather than a half-written one ([`OpenLog`]).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cretlink_console::{
    AuditEntryView, AuditServiceConfig, SessionAuditStart, SessionSummary,
    list_sessions as read_history, session_entries,
};
use tauri::State;
use tokio::sync::{oneshot, watch};

use super::session_driver::{is_terminal, now_unix_millis};
use super::{AUDIT_SEAL_TIMEOUT, AppState, CommandError};

/// The file name of the session log the drain is writing, if any, and of the
/// current or last session's log, which the Audit tab reads (ST-M009-008).
#[derive(Debug, Clone)]
pub(crate) struct OpenLog(
    Arc<watch::Sender<Option<String>>>,
    Arc<std::sync::Mutex<Option<String>>>,
);

impl Default for OpenLog {
    fn default() -> Self {
        Self(
            Arc::new(watch::Sender::new(None)),
            Arc::new(std::sync::Mutex::new(None)),
        )
    }
}

impl OpenLog {
    /// The log being written now.
    fn current(&self) -> Option<String> {
        self.0.borrow().clone()
    }

    /// The log of the current session, or of the last one until the next
    /// session opens its own.
    fn latest(&self) -> Option<String> {
        self.1
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_latest(latest: &std::sync::Mutex<Option<String>>, name: Option<String>) {
        *latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = name;
    }

    /// Records that the drain finished with the log (sealed, or given up).
    pub(crate) fn closed(&self) {
        self.0.send_replace(None);
    }

    /// Stands between the pump and the audit service: passes the opened log on
    /// unchanged and remembers its name. A pump that drops its sender without
    /// a log drops the service's as well, so the service discards honestly as
    /// before.
    pub(crate) fn track(
        &self,
    ) -> (
        oneshot::Sender<SessionAuditStart>,
        oneshot::Receiver<SessionAuditStart>,
    ) {
        let (pump_tx, pump_rx) = oneshot::channel::<SessionAuditStart>();
        let (service_tx, service_rx) = oneshot::channel();
        let open = self.0.clone();
        // A new session: the last one's entries are no longer this session's.
        Self::set_latest(&self.1, None);
        let latest = self.1.clone();
        tauri::async_runtime::spawn(async move {
            if let Ok(start) = pump_rx.await {
                open.send_replace(Some(start.file_name.clone()));
                Self::set_latest(&latest, Some(start.file_name.clone()));
                let _ = service_tx.send(start);
            }
        });
        (pump_tx, service_rx)
    }

    /// Waits up to `limit` for the log `name` to close.
    async fn wait_closed(&self, name: &str, limit: Duration) {
        let mut rx = self.0.subscribe();
        let _ =
            tokio::time::timeout(limit, rx.wait_for(|open| open.as_deref() != Some(name))).await;
    }
}

/// Drops the summary of the log still being written, which is not history yet.
fn without_open(summaries: Vec<SessionSummary>, open: Option<&str>) -> Vec<SessionSummary> {
    let Some(open) = open else {
        return summaries;
    };
    summaries
        .into_iter()
        .filter(|summary| {
            Path::new(&summary.file)
                .file_name()
                .is_none_or(|name| name != open)
        })
        .collect()
}

/// Reads the history of `dir`, leaving out the log still being written.
fn history(dir: &Path, open: Option<&str>) -> Vec<SessionSummary> {
    let config = AuditServiceConfig::default();
    without_open(
        read_history(dir, &config.retention, now_unix_millis()),
        open,
    )
}

/// The sessions within audit retention, newest first (ST-M009-012). Right
/// after a session ends, waits a bounded moment for its log to be sealed.
#[tauri::command]
pub(super) async fn list_sessions(
    state: State<'_, AppState>,
) -> Result<Vec<SessionSummary>, CommandError> {
    let open_log = state.audit_log.clone();
    if let Some(name) = open_log.current() {
        let ended = is_terminal(&state.guard());
        if ended {
            open_log.wait_closed(&name, AUDIT_SEAL_TIMEOUT).await;
        }
    }
    let open = open_log.current();
    let dir = AuditServiceConfig::default().dir;
    tauri::async_runtime::spawn_blocking(move || history(&dir, open.as_deref()))
        .await
        .map_err(|_| CommandError::new("history", "the session history could not be read"))
}

/// The audit file named `name` in `dir`, when `name` is a plain file name
/// (never a path out of the folder).
fn log_in(dir: &Path, name: &str) -> Option<PathBuf> {
    let plain = Path::new(name)
        .file_name()
        .is_some_and(|file| file == std::ffi::OsStr::new(name));
    plain.then(|| dir.join(name))
}

/// This session's audit entries, oldest first, for the Live tab **Audit**
/// (ST-M009-008): the current session's, or the last one's until the next
/// session starts; none before the first. Each is one secret-free line read
/// from the session's own audit file ([`cretlink_console::session_entries`]).
#[tauri::command]
pub(super) async fn get_session_audit(
    state: State<'_, AppState>,
) -> Result<Vec<AuditEntryView>, CommandError> {
    let Some(name) = state.audit_log.latest() else {
        return Ok(Vec::new());
    };
    let dir = AuditServiceConfig::default().dir;
    let Some(path) = log_in(&dir, &name) else {
        return Ok(Vec::new());
    };
    tauri::async_runtime::spawn_blocking(move || session_entries(&path))
        .await
        .map_err(|_| CommandError::new("audit", "the session's audit entries could not be read"))
}

/// The audit file of `session`, which must be one the history lists, inside
/// `dir` itself (not a link out of it).
fn audit_file(
    dir: &Path,
    summaries: &[SessionSummary],
    session: &str,
) -> Result<PathBuf, &'static str> {
    let file = cretlink_console::history::file_of(summaries, session)
        .ok_or("this session is not in the audit folder")?;
    let dir = dir
        .canonicalize()
        .map_err(|_| "the audit folder cannot be found")?;
    let file = file
        .canonicalize()
        .map_err(|_| "the audit file of this session cannot be found")?;
    if file.parent() != Some(dir.as_path()) || !file.is_file() {
        return Err("the audit file of this session is not inside the audit folder");
    }
    Ok(file)
}

/// The form of a canonical path that Explorer and the user read: Windows'
/// `canonicalize` yields verbatim paths (`\\?\C:\…`, `\\?\UNC\server\…`),
/// which Explorer may refuse. The prefix is dropped for a drive or UNC path
/// short enough not to need it; anything else is returned as it is. The
/// inside-the-audit-folder check is made on the canonical form before this.
fn displayable(path: PathBuf) -> PathBuf {
    /// Beyond this a path needs the verbatim form (`MAX_PATH`).
    const MAX_PATH: usize = 260;
    let Some(text) = path.to_str() else {
        return path;
    };
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        let plain = format!(r"\\{rest}");
        if plain.len() < MAX_PATH {
            return PathBuf::from(plain);
        }
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        let bytes = rest.as_bytes();
        let drive = bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && bytes[2] == b'\\';
        if drive && rest.len() < MAX_PATH {
            return PathBuf::from(rest);
        }
    }
    path
}

/// Opens the audit folder, or with `session` that session's audit file, with
/// the platform opener (ST-M009-012). The session must be one the history
/// lists.
#[tauri::command]
pub(super) async fn open_audit(
    state: State<'_, AppState>,
    session: Option<String>,
) -> Result<(), CommandError> {
    let dir = AuditServiceConfig::default().dir;
    let open = state.audit_log.current();
    tauri::async_runtime::spawn_blocking(move || {
        let target = match session {
            None => {
                // The technician's own data folder; created by every session
                // anyway, so opening it before the first is not an error.
                std::fs::create_dir_all(&dir).map_err(|error| {
                    CommandError::new(
                        "audit",
                        format!("the audit folder could not be created: {error}"),
                    )
                })?;
                dir.canonicalize()
                    .map_err(|_| CommandError::new("audit", "the audit folder cannot be found"))?
            }
            Some(session) => {
                let summaries = history(&dir, open.as_deref());
                audit_file(&dir, &summaries, &session)
                    .map_err(|message| CommandError::new("audit", message))?
            }
        };
        crate::opener::open(&displayable(target)).map_err(|error| {
            CommandError::new(
                "audit",
                format!("the audit log could not be opened: {error}"),
            )
        })
    })
    .await
    .map_err(|_| CommandError::new("audit", "the audit log could not be opened"))?
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use cretlink_console::{ActionCounts, Integrity};
    use cretlink_session::CleanupResult;

    use super::*;

    fn summary(id: &str, file: &Path) -> SessionSummary {
        SessionSummary {
            id: id.to_owned(),
            file: file.display().to_string(),
            integrity: Integrity::Verified,
            hostname: None,
            os: None,
            started_at: None,
            ended_at: None,
            duration_ms: None,
            ended_by: None,
            actions: ActionCounts::default(),
            files_sent: 0,
            files_received: 0,
            failures: 0,
            first_failure: None,
            cleanup: CleanupResult::unknown(None),
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cretlink-app-history-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_log_being_written_is_not_history() {
        let dir = Path::new("/audit");
        let all = vec![
            summary("new", &dir.join("2-new.jsonl")),
            summary("old", &dir.join("1-old.jsonl")),
        ];
        let shown = without_open(all.clone(), Some("2-new.jsonl"));
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].id, "old");
        assert_eq!(without_open(all, None).len(), 2);
    }

    #[test]
    fn only_a_listed_session_inside_the_folder_is_opened() {
        let dir = temp_dir("open");
        let file = dir.join("1-listed.jsonl");
        std::fs::write(&file, "{}\n").unwrap();
        let outside_dir = temp_dir("outside");
        let outside = outside_dir.join("1-outside.jsonl");
        std::fs::write(&outside, "{}\n").unwrap();
        let summaries = vec![
            summary("listed", &file),
            summary("outside", &outside),
            summary("traversal", &dir.join("..").join("x.jsonl")),
        ];

        assert_eq!(
            audit_file(&dir, &summaries, "listed").unwrap(),
            file.canonicalize().unwrap()
        );
        assert!(audit_file(&dir, &summaries, "unknown").is_err());
        assert!(audit_file(&dir, &summaries, "outside").is_err());
        assert!(audit_file(&dir, &summaries, "traversal").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&outside_dir).unwrap();
    }

    #[tokio::test]
    async fn tracking_passes_the_log_on_and_remembers_its_name() {
        let open = OpenLog::default();
        let (pump_tx, service_rx) = open.track();
        let dir = temp_dir("track");
        let file = std::fs::File::create(dir.join("x.jsonl")).unwrap();
        let session = cretlink_protocol::SessionId::new();
        pump_tx
            .send(SessionAuditStart {
                session,
                file,
                file_name: "x.jsonl".to_owned(),
            })
            .unwrap();
        let start = service_rx.await.unwrap();
        assert_eq!(start.session, session);
        assert_eq!(open.current().as_deref(), Some("x.jsonl"));
        open.closed();
        assert_eq!(open.current(), None);
        // A closed log ends the wait at once.
        open.wait_closed("x.jsonl", Duration::from_secs(5)).await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_pump_without_a_log_leaves_the_service_without_one() {
        let open = OpenLog::default();
        let (pump_tx, service_rx) = open.track();
        drop(pump_tx);
        assert!(service_rx.await.is_err());
        assert_eq!(open.current(), None);
    }

    #[test]
    fn verbatim_paths_are_opened_in_their_plain_form() {
        assert_eq!(
            displayable(PathBuf::from(
                r"\\?\C:\Users\ann\AppData\Roaming\CretLink\audit"
            )),
            PathBuf::from(r"C:\Users\ann\AppData\Roaming\CretLink\audit")
        );
        assert_eq!(
            displayable(PathBuf::from(r"\\?\UNC\server\share\audit\a.jsonl")),
            PathBuf::from(r"\\server\share\audit\a.jsonl")
        );
        // Already plain, a device path, or too long for the plain form: kept.
        for kept in [
            "/home/ann/.local/share/CretLink/audit".to_owned(),
            r"C:\audit".to_owned(),
            r"\\?\Volume{0000}\audit".to_owned(),
            format!(r"\\?\C:\{}", "a".repeat(300)),
        ] {
            assert_eq!(displayable(PathBuf::from(&kept)), PathBuf::from(&kept));
        }
    }

    #[test]
    fn the_audit_tab_reads_only_a_file_in_the_audit_folder() {
        let dir = Path::new("/audit");
        assert_eq!(
            log_in(dir, "2026-11-03T10-00-00Z-abc.jsonl"),
            Some(dir.join("2026-11-03T10-00-00Z-abc.jsonl"))
        );
        for bad in ["../x.jsonl", "/etc/passwd", "a/b.jsonl", "", ".."] {
            assert_eq!(log_in(dir, bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_latest_log_is_the_current_session_s_until_the_next_starts() {
        let log = OpenLog::default();
        assert_eq!(log.latest(), None);
        OpenLog::set_latest(&log.1, Some("one.jsonl".to_owned()));
        // Closing the log keeps it as this session's for the Audit tab.
        log.closed();
        assert_eq!(log.latest().as_deref(), Some("one.jsonl"));
        assert_eq!(log.current(), None);
    }
}
