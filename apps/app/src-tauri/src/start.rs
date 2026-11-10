//! The start screen and the role question every window asks (ST-M009-005,
//! ST-M009-006).
//!
//! [`get_role`] is registered in every process, so the frontend can load the
//! right interface. [`choose_role`] exists only in the start-screen process: it
//! starts the chosen role in a new process and closes this one, so each role
//! keeps a process of its own (ADR-0018). Get help starts the customer role
//! unelevated with `--from-start`: that window first explains the UAC prompt
//! and relaunches elevated only when the customer presses **Continue**
//! (`#c-admin`, ST-M009-009).
//! Choosing Give help this way is **Run once without installing**; **Install
//! for me** goes through `technician::setup::install_app`, which starts the
//! installed copy instead.

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
}

impl RoleView {
    fn of(screen: Screen) -> Self {
        let name = match screen {
            Screen::Start => "start",
            Screen::Role(Role::Technician) => "technician",
            Screen::Role(Role::Customer) => "customer",
        };
        Self {
            screen: name,
            version: crate::RELEASE_VERSION,
            can_get_help: CUSTOMER_AVAILABLE,
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
    /// `started` (this process is closing) or `failed`.
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
    match crate::start_process(&exe, &role_args(role)) {
        Ok(()) => {
            app.exit(0);
            ChooseOutcome::new("started", "")
        }
        Err(err) => ChooseOutcome::new("failed", format!("CretLink could not start: {err}")),
    }
}

/// The arguments the start screen starts a role with. Get help is marked
/// `--from-start`, so its window explains the UAC prompt and **Back** returns
/// here (ST-M009-009).
fn role_args(role: Role) -> Vec<String> {
    let mut args = vec!["--role".to_owned(), role.arg().to_owned()];
    if role == Role::Customer {
        args.push(crate::launch::FROM_START.to_owned());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_view_names_the_screen() {
        assert_eq!(RoleView::of(Screen::Start).screen, "start");
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

    #[test]
    fn get_help_starts_the_unelevated_explanation_first() {
        // The start screen never shows the UAC prompt itself: the customer
        // window it starts explains it first (`#c-admin`).
        let args = role_args(Role::Customer);
        assert_eq!(
            crate::launch::resolve(&args, "cretlink", true, false),
            crate::launch::Launch::Role {
                role: Role::Customer,
                entry: crate::launch::Entry::FromStart
            }
        );
        assert_eq!(
            crate::launch::resolve(&role_args(Role::Technician), "cretlink", true, false),
            crate::launch::Launch::Role {
                role: Role::Technician,
                entry: crate::launch::Entry::Relaunched
            }
        );
    }
}
