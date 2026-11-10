//! The startup sweep of the customer role (ST-M009-010, ST-M003-005,
//! REQ-F-019).
//!
//! Before the customer role connects, it removes the staging trees an earlier
//! session left behind (a crash, or a removal that failed), as the support
//! agent did. The sweep runs once per process, off the window's thread, as soon
//! as the window is past the permission step: at window start for an elevated
//! (or relaunched) process, or when the customer goes on without admin rights.
//! An unelevated window that is still explaining the UAC prompt does not sweep:
//! it either starts the elevated child, which sweeps, or goes on itself.
//!
//! The sweep is skipped while another customer-side process runs, and this
//! process then holds its share of the staging base until it ends
//! ([`StagingInUse`]), so a second **Get help** window never removes the
//! staging of a session live in the first.
//!
//! A dial waits for the sweep to finish ([`StartupSweep::finished`]). When the
//! sweep kept files it could not remove, the view carries a
//! [`StartupCleanupView`] and the customer screen says so in plain words with
//! the recovery command ([`recovery_command`]), written so that it runs as
//! shown in an administrator Windows PowerShell: never a silent success
//! (REQ-N-008).

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use cretlink_support_agent::recovery::{StagingInUse, StartupSweepReport};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::watch;

use super::{AGENT_VIEW_EVENT, AppState};

/// Files an earlier session left that the startup sweep could not remove.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StartupCleanupView {
    /// How many files were kept.
    pub failed: usize,
    /// The command that removes them, runnable as written in an
    /// administrator Windows PowerShell: the full path of `cretlink-cli.exe`
    /// when it sits next to this program, else of this program itself
    /// (`cretlink.exe` or the `CretLink-Support*` copy), see
    /// [`recovery_command`].
    pub command: String,
}

/// Maps a sweep report to the notice the customer sees: `None` when nothing
/// was kept, whatever was removed.
#[must_use]
pub(crate) fn notice(report: &StartupSweepReport, command: String) -> Option<StartupCleanupView> {
    if report.ok() {
        return None;
    }
    Some(StartupCleanupView {
        failed: report.failed.len(),
        command,
    })
}

/// A PowerShell single-quoted literal: every quote character PowerShell
/// accepts as a single quote is doubled, so nothing in the path is
/// interpreted.
fn powershell_literal(path: &Path) -> String {
    let mut quoted = String::from("'");
    for c in path.to_string_lossy().chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}') {
            quoted.push(c);
        }
        quoted.push(c);
    }
    quoted.push('\'');
    quoted
}

/// The recovery command for the program at `exe`, runnable as written in an
/// administrator Windows PowerShell (ST-M009-010):
///
/// - `& '<folder>\cretlink-cli.exe' recover --remove` when `cretlink-cli.exe`
///   sits next to it: a console program, so PowerShell waits for it and shows
///   its report;
/// - otherwise `& '<exe>' recover --remove | Out-Host`: `cretlink.exe` (and a
///   `CretLink-Support*` copy) is a Windows GUI program, which PowerShell would
///   not wait for; piping its output makes PowerShell wait, show the report
///   and set `$LASTEXITCODE`.
///
/// The full path is given because the program is rarely on `PATH` (a
/// downloaded copy runs from `Downloads`). Off Windows (development builds
/// only) the path is single-quoted for a POSIX shell.
#[must_use]
pub(crate) fn recovery_command(exe: &Path, cli_beside: bool, windows: bool) -> String {
    if !windows {
        let quoted = exe.to_string_lossy().replace('\'', r"'\''");
        return format!("'{quoted}' recover --remove");
    }
    if cli_beside {
        let cli = exe.with_file_name(CLI_EXE);
        format!("& {} recover --remove", powershell_literal(&cli))
    } else {
        format!("& {} recover --remove | Out-Host", powershell_literal(exe))
    }
}

/// The console build's file name on Windows.
const CLI_EXE: &str = "cretlink-cli.exe";

/// The recovery command of this process ([`recovery_command`]).
fn command() -> String {
    let Ok(exe) = std::env::current_exe() else {
        return cretlink_support_agent::recovery::recover_command("cretlink");
    };
    let cli_beside = exe.with_file_name(CLI_EXE).is_file();
    recovery_command(&exe, cli_beside, cfg!(windows))
}

/// This process's one startup sweep: started at most once, awaited by a dial.
#[derive(Debug, Clone)]
pub(crate) struct StartupSweep {
    started: Arc<Mutex<bool>>,
    /// This process's share of the staging base, held until the process ends
    /// so another **Get help** window's sweep never removes this one's live
    /// staging.
    in_use: Arc<Mutex<Option<StagingInUse>>>,
    done_tx: Arc<watch::Sender<bool>>,
    done_rx: watch::Receiver<bool>,
}

impl Default for StartupSweep {
    fn default() -> Self {
        let (done_tx, done_rx) = watch::channel(false);
        Self {
            started: Arc::new(Mutex::new(false)),
            in_use: Arc::new(Mutex::new(None)),
            done_tx: Arc::new(done_tx),
            done_rx,
        }
    }
}

impl StartupSweep {
    /// Claims the sweep: `true` exactly once per process.
    fn claim(&self) -> bool {
        let mut started = self.started.lock().unwrap_or_else(PoisonError::into_inner);
        !std::mem::replace(&mut *started, true)
    }

    /// Starts the sweep unless it already ran or runs. It runs on the async
    /// runtime, never on the window's thread; when it kept files, the notice
    /// goes into the view and the view is emitted again.
    pub(crate) fn start(&self, app: &AppHandle, state: &AppState) {
        if !self.claim() {
            return;
        }
        let app = app.clone();
        let shared = state.inner.clone();
        let done = self.done_tx.clone();
        let in_use = self.in_use.clone();
        tauri::async_runtime::spawn(async move {
            let (report, share) = cretlink_support_agent::sweep_stale_staging().await;
            *in_use.lock().unwrap_or_else(PoisonError::into_inner) = share;
            if let Some(view) = notice(&report, command()) {
                let mut session = super::lock(&shared);
                session.startup_cleanup = Some(view);
                let _ = app.emit(AGENT_VIEW_EVENT, session.view());
            }
            let _ = done.send(true);
        });
    }

    /// Resolves once the sweep has finished, so a dial never starts before it
    /// (ST-M009-010).
    pub(crate) async fn finished(&self) {
        let mut done = self.done_rx.clone();
        let _ = done.wait_for(|finished| *finished).await;
    }
}

#[cfg(test)]
mod tests {
    use cretlink_protocol::CleanupFailure;

    use super::*;

    fn failure(path: &str) -> CleanupFailure {
        CleanupFailure {
            canonical_path: path.to_owned(),
            reason: "in use".to_owned(),
        }
    }

    #[test]
    fn a_clean_sweep_shows_nothing_even_after_removing() {
        assert_eq!(notice(&StartupSweepReport::default(), "x".to_owned()), None);
        let removed = StartupSweepReport {
            sessions_scanned: 2,
            sessions_removed: 2,
            artifacts_removed: 5,
            failed: vec![],
        };
        assert_eq!(notice(&removed, "x".to_owned()), None);
    }

    #[test]
    fn kept_files_carry_the_recovery_command() {
        let report = StartupSweepReport {
            failed: vec![failure("C:/a"), failure("C:/b")],
            ..StartupSweepReport::default()
        };
        let command = "& 'C:\\x\\cretlink-cli.exe' recover --remove".to_owned();
        assert_eq!(
            notice(&report, command.clone()),
            Some(StartupCleanupView {
                failed: 2,
                command: command.clone(),
            })
        );
        let json = serde_json::to_value(notice(&report, command.clone())).unwrap_or_default();
        assert_eq!(json["failed"], 2);
        assert_eq!(json["command"], command.as_str());
    }

    #[test]
    fn the_recovery_command_runs_as_written_in_powershell() {
        // A downloaded copy, alone in Downloads: the GUI program, piped so
        // PowerShell waits for it and shows its report.
        let copy = Path::new(r"C:\Users\Ann O'Neil\Downloads\CretLink-Support (1).exe");
        assert_eq!(
            recovery_command(copy, false, true),
            r"& 'C:\Users\Ann O''Neil\Downloads\CretLink-Support (1).exe' recover --remove | Out-Host"
        );
        // Next to cretlink-cli.exe: the console program, which PowerShell
        // waits for anyway.
        let installed = Path::new(r"C:\Users\ann\AppData\Local\Programs\CretLink\cretlink.exe");
        let with_cli = recovery_command(installed, true, true);
        assert!(with_cli.starts_with("& '"), "{with_cli}");
        assert!(
            with_cli.ends_with(r"cretlink-cli.exe' recover --remove"),
            "{with_cli}"
        );
        assert!(!with_cli.contains("Out-Host"));
        if cfg!(windows) {
            assert_eq!(
                with_cli,
                r"& 'C:\Users\ann\AppData\Local\Programs\CretLink\cretlink-cli.exe' recover --remove"
            );
        }
        // Every PowerShell single-quote character is doubled.
        assert_eq!(
            powershell_literal(Path::new("a\u{2019}b")),
            "'a\u{2019}\u{2019}b'"
        );
        // A development build off Windows: quoted for a POSIX shell.
        assert_eq!(
            recovery_command(Path::new("/opt/it's/cretlink"), false, false),
            r"'/opt/it'\''s/cretlink' recover --remove"
        );
    }

    #[test]
    fn the_sweep_is_claimed_once() {
        let sweep = StartupSweep::default();
        assert!(sweep.claim());
        assert!(!sweep.clone().claim());
    }
}
