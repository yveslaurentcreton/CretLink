//! Before the UAC prompt (`#c-admin`, ST-M009-009, REQ-F-002).
//!
//! An unelevated customer process — Get help from the start screen, or a
//! `CretLink-Support*` copy — first explains in four steps what the UAC prompt
//! is for, and that nothing happens before step 4 and nothing is installed.
//! Only **Continue** ([`relaunch_elevated`]) starts this program again
//! elevated, with `--role customer`; that child never explains or relaunches
//! again ([`crate::launch::customer_admin_step`]). **Back** ([`go_back`])
//! returns to the start screen, or closes a `CretLink-Support*` copy. When the
//! customer declines the prompt, the screen says so honestly and offers **Try
//! again** and **Continue without admin rights** ([`continue_without_admin`]),
//! which goes on in this unelevated process: its identity says "Admin rights:
//! No" (REQ-S-011).
//!
//! No dial happens before the customer leaves this step: the connect commands
//! refuse while it is shown ([`AdminState::ready`]).
//!
//! On macOS and Linux (a `dev-simulate` build only) there is no UAC: the step
//! is shown so the flow can be exercised, and **Continue** starts the child
//! as a normal, unelevated process. The view says so (`uac: false`).

use std::sync::{Mutex, PoisonError};

use serde::Serialize;
use tauri::{AppHandle, State};

use super::{AppState, CommandError, session_exists};
use crate::launch::{
    AdminStep, BackTo, Entry, Role, back_target, customer_admin_step, may_relaunch,
};

/// Where the customer is in the admin step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// `#c-admin` before the prompt.
    Explain,
    /// The customer declined the prompt.
    Declined,
    /// Past the step: elevated, or going on without admin rights.
    Ready,
}

/// The admin step of this process, as managed state.
#[derive(Debug)]
pub(crate) struct AdminState {
    entry: Entry,
    elevated: bool,
    step: Mutex<Step>,
}

impl AdminState {
    /// The step a process started as `entry` begins with.
    pub(crate) fn new(entry: Entry, elevated: bool) -> Self {
        let step = match customer_admin_step(entry, elevated) {
            AdminStep::Explain => Step::Explain,
            AdminStep::NotNeeded => Step::Ready,
        };
        Self {
            entry,
            elevated,
            step: Mutex::new(step),
        }
    }

    fn step(&self) -> Step {
        *self.step.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, step: Step) {
        *self.step.lock().unwrap_or_else(PoisonError::into_inner) = step;
    }

    /// Whether the customer is past the admin step, so a dial may start.
    pub(crate) fn ready(&self) -> bool {
        self.step() == Step::Ready
    }

    fn view(&self) -> LaunchView {
        LaunchView {
            step: match self.step() {
                Step::Explain => "admin",
                Step::Declined => "declined",
                Step::Ready => "ready",
            },
            back: match back_target(self.entry) {
                BackTo::StartScreen => "start",
                BackTo::Close => "close",
            },
            elevated: self.elevated,
            uac: cfg!(windows),
            demo: cfg!(feature = "dev-simulate"),
        }
    }

    /// Goes on without admin rights, allowed only after a declined prompt.
    fn continue_unelevated(&self) -> Result<LaunchView, CommandError> {
        if self.step() != Step::Declined {
            return Err(CommandError::new(
                "admin",
                "continuing without admin rights is offered only after Windows was not given permission",
            ));
        }
        self.set(Step::Ready);
        Ok(self.view())
    }

    /// Refuses unless the admin step is on screen and no session exists.
    fn ensure_explaining(&self, state: &AppState) -> Result<(), CommandError> {
        if self.ready() {
            return Err(CommandError::new(
                "admin",
                "this window is already past the permission step",
            ));
        }
        if session_exists(&state.guard()) {
            return Err(CommandError::new(
                "admin",
                "a session is in progress; stop it first",
            ));
        }
        Ok(())
    }
}

/// What the frontend shows before choosing the technician.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LaunchView {
    /// `admin` (`#c-admin`), `declined` (the prompt was declined) or `ready`.
    pub step: &'static str,
    /// Where **Back** goes: `start` (the start screen) or `close`.
    pub back: &'static str,
    /// Whether this process is elevated.
    pub elevated: bool,
    /// Whether **Continue** shows the real UAC prompt (Windows). `false` only
    /// in a `dev-simulate` build on macOS or Linux, where it is simulated.
    pub uac: bool,
    /// Whether this is a `dev-simulate` build, which offers the simulated demo.
    pub demo: bool,
}

/// The answer to [`relaunch_elevated`].
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RelaunchOutcome {
    /// `started` (this window is closing), `declined` or `failed`.
    pub outcome: &'static str,
    /// A plain explanation when it failed.
    pub message: String,
    /// The admin step after the attempt.
    pub launch: LaunchView,
}

/// This window's admin step.
#[tauri::command]
pub(super) fn get_launch(admin: State<'_, AdminState>) -> LaunchView {
    admin.view()
}

/// **Continue** and **Try again**: starts this program elevated with
/// `--role customer`, behind the UAC prompt, and closes this window. A
/// declined prompt is reported as `declined`, never as a failure.
#[tauri::command]
pub(super) async fn relaunch_elevated(
    app: AppHandle,
    admin: State<'_, AdminState>,
    state: State<'_, AppState>,
) -> Result<RelaunchOutcome, CommandError> {
    admin.ensure_explaining(&state)?;
    if !may_relaunch(admin.entry) {
        return Err(CommandError::new(
            "admin",
            "this window was already started by CretLink and does not start again",
        ));
    }
    let exe = std::env::current_exe()
        .map_err(|err| CommandError::new("admin", format!("CretLink cannot find itself: {err}")))?;
    let args = vec!["--role".to_owned(), Role::Customer.arg().to_owned()];
    // The UAC prompt blocks until the customer answers, so it runs off the
    // window's thread.
    let started =
        tauri::async_runtime::spawn_blocking(move || crate::start_elevated(&exe, &args)).await;
    let outcome = match started {
        Ok(Ok(true)) => {
            app.exit(0);
            ("started", String::new())
        }
        Ok(Ok(false)) => {
            admin.set(Step::Declined);
            ("declined", String::new())
        }
        Ok(Err(err)) => ("failed", format!("CretLink could not start: {err}")),
        Err(_) => (
            "failed",
            "CretLink could not ask Windows for permission".to_owned(),
        ),
    };
    Ok(RelaunchOutcome {
        outcome: outcome.0,
        message: outcome.1,
        launch: admin.view(),
    })
}

/// **Continue without admin rights**, offered after a declined prompt: this
/// unelevated window goes on to choosing the technician, and starts its
/// startup sweep (ST-M009-010).
#[tauri::command]
pub(super) fn continue_without_admin(
    app: AppHandle,
    admin: State<'_, AdminState>,
    state: State<'_, AppState>,
) -> Result<LaunchView, CommandError> {
    let view = admin.continue_unelevated()?;
    state.startup.start(&app, &state);
    Ok(view)
}

/// **Back**: returns to the start screen (started from it), or closes this
/// `CretLink-Support*` copy.
#[tauri::command]
pub(super) fn go_back(
    app: AppHandle,
    admin: State<'_, AdminState>,
    state: State<'_, AppState>,
) -> Result<(), CommandError> {
    admin.ensure_explaining(&state)?;
    if back_target(admin.entry) == BackTo::StartScreen {
        let exe = std::env::current_exe().map_err(|err| {
            CommandError::new("admin", format!("CretLink cannot find itself: {err}"))
        })?;
        crate::start_process(&exe, &[]).map_err(|err| {
            CommandError::new("admin", format!("the start screen could not open: {err}"))
        })?;
    }
    app.exit(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unelevated_window_starts_with_the_explanation() {
        for entry in [Entry::FromStart, Entry::Direct] {
            let admin = AdminState::new(entry, false);
            assert_eq!(admin.view().step, "admin");
            assert!(!admin.ready(), "no dial before the admin step is passed");
        }
        assert_eq!(
            AdminState::new(Entry::FromStart, false).view().back,
            "start"
        );
        assert_eq!(AdminState::new(Entry::Direct, false).view().back, "close");
    }

    #[test]
    fn an_elevated_or_relaunched_window_goes_straight_on() {
        assert!(AdminState::new(Entry::Direct, true).ready());
        assert!(AdminState::new(Entry::FromStart, true).ready());
        // The child started after the prompt, elevated or not.
        let child = AdminState::new(Entry::Relaunched, false);
        assert!(child.ready());
        assert!(!child.view().elevated);
    }

    #[test]
    fn continuing_without_admin_rights_needs_a_declined_prompt() {
        let admin = AdminState::new(Entry::FromStart, false);
        assert!(admin.continue_unelevated().is_err());
        admin.set(Step::Declined);
        assert_eq!(admin.view().step, "declined");
        assert!(admin.continue_unelevated().is_ok());
        assert!(admin.ready());
        // Honest: still not elevated.
        assert!(!admin.view().elevated);
    }

    #[test]
    fn the_view_says_whether_the_prompt_is_real() {
        assert_eq!(
            AdminState::new(Entry::Direct, false).view().uac,
            cfg!(windows)
        );
        let json =
            serde_json::to_value(AdminState::new(Entry::Direct, false).view()).unwrap_or_default();
        assert_eq!(json["step"], "admin");
        assert_eq!(json["back"], "close");
        assert!(json.get("elevated").is_some());
    }
}
