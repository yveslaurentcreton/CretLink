//! The start screen and the role question every window asks (ST-M009-005).
//!
//! [`get_role`] is registered in every process, so the frontend can load the
//! right interface. [`choose_role`] exists only in the start-screen process: it
//! starts the chosen role in a new process — elevated through UAC for Get help
//! — and closes this one, so each role keeps a process of its own (ADR-0018).

use serde::Serialize;
use tauri::{AppHandle, State};

use crate::launch::Role;
use crate::{CUSTOMER_AVAILABLE, Screen};

/// What this process shows, as managed state.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProcessScreen(pub(crate) Screen);

/// The answer to [`get_role`].
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RoleView {
    /// `start`, `technician` or `customer`.
    pub screen: &'static str,
    /// The release version.
    pub version: &'static str,
    /// Whether Get help exists in this build.
    pub can_get_help: bool,
    /// Whether the customer just declined the UAC prompt of Get help.
    pub declined: bool,
}

impl RoleView {
    fn of(screen: Screen) -> Self {
        let (name, declined) = match screen {
            Screen::Start { declined } => ("start", declined),
            Screen::Role(Role::Technician) => ("technician", false),
            Screen::Role(Role::Customer) => ("customer", false),
        };
        Self {
            screen: name,
            version: crate::RELEASE_VERSION,
            can_get_help: CUSTOMER_AVAILABLE,
            declined,
        }
    }
}

/// Which interface this process shows.
#[tauri::command]
pub(crate) fn get_role(screen: State<'_, ProcessScreen>) -> RoleView {
    RoleView::of(screen.0)
}

/// The answer to [`choose_role`].
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ChooseOutcome {
    /// `started` (this process is closing), `declined` (UAC was declined) or
    /// `failed`.
    pub outcome: &'static str,
    /// A plain-language explanation when it failed.
    pub message: String,
}

impl ChooseOutcome {
    fn new(outcome: &'static str, message: impl Into<String>) -> Self {
        Self {
            outcome,
            message: message.into(),
        }
    }
}

/// Starts the chosen role in a new process and closes this one.
#[tauri::command]
pub(crate) fn choose_role(app: AppHandle, role: String) -> ChooseOutcome {
    let role = match role.as_str() {
        "technician" => Role::Technician,
        "customer" if CUSTOMER_AVAILABLE => Role::Customer,
        _ => return ChooseOutcome::new("failed", "That choice is not available here."),
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            return ChooseOutcome::new("failed", format!("CretLink cannot find itself: {err}"));
        }
    };
    let args = vec!["--role".to_owned(), role.arg().to_owned()];
    match crate::start_role_process(&exe, role, &args) {
        Ok(true) => {
            app.exit(0);
            ChooseOutcome::new("started", "")
        }
        Ok(false) => ChooseOutcome::new("declined", ""),
        Err(err) => ChooseOutcome::new("failed", format!("CretLink could not start: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_view_names_the_screen() {
        assert_eq!(
            RoleView::of(Screen::Start { declined: true }).screen,
            "start"
        );
        assert!(RoleView::of(Screen::Start { declined: true }).declined);
        assert_eq!(
            RoleView::of(Screen::Role(Role::Technician)).screen,
            "technician"
        );
        assert_eq!(
            RoleView::of(Screen::Role(Role::Customer)).screen,
            "customer"
        );
        assert_eq!(
            RoleView::of(Screen::Role(Role::Customer)).can_get_help,
            CUSTOMER_AVAILABLE
        );
    }
}
