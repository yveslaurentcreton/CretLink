//! Keeping the support program for next time (ST-M009-017, REQ-F-024,
//! ADR-0018).
//!
//! A customer who gets help more than once can keep the support program on
//! the computer. What that adds, how the program is written and how it goes
//! again is decided in [`cretlink_cli::kept`]; this module is the customer's
//! own way to it, and the only one:
//!
//! | Command | What it does |
//! |---|---|
//! | [`get_keep`] | what is kept on this computer, and what this window offers |
//! | [`keep_plan`] | what keeping would add, with the relay this copy brings, before the customer chooses |
//! | [`keep_program`] | keeps the program that runs, as the plan that was shown says |
//! | [`remove_plan`] | what removing would remove, before the customer chooses |
//! | [`remove_program`] | removes the kept program |
//!
//! - **Only on the customer's click.** These commands exist in the customer
//!   role alone and are called by its window. Nothing on a command line, in
//!   the environment or on the wire keeps a program: the remote tool surface
//!   has no operation for it, and [`keep_program`] takes no plan from its
//!   caller but the one this process made for [`keep_plan`].
//! - **Never during a session.** Every command refuses while a session
//!   exists ([`session_exists`]) and before the window is past the
//!   permission step. The window offers keeping before a session and on the
//!   end report.
//! - **With administrator rights.** Keeping, and removing in this process,
//!   need them: the window offers to keep only when it has them
//!   ([`offer`]). A kept program whose window runs without them removes
//!   itself by starting `uninstall` through the elevation prompt.
//! - **What was shown is what is kept.** The relay settings next to this
//!   copy are read once, for the plan; the customer sees the relay they
//!   name, where this copy would connect and what vouches for the name, and
//!   exactly that value is written.
//!
//! Off Windows the customer role does not keep itself: there is no layout,
//! and the window is told that nothing is offered.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use cretlink_cli::kept::{self, KeepPlan, Layout, OsSystem, Relation, System as _};
use cretlink_console::RelayTrust;
use cretlink_session::KeptProgram;
use serde::Serialize;
use tauri::{AppHandle, State};

use super::{AppState, CommandError, RelayUseView, admin::AdminState, session_exists};
use crate::relay;

/// The keeping of this process, as managed state.
#[derive(Debug)]
pub(crate) struct KeepState {
    /// Whether this process acts as an administrator, asked once: it does
    /// not change while the process runs.
    administrator: bool,
    /// The plan the customer was shown last, which [`keep_program`] carries
    /// out and no other.
    shown: Mutex<Option<KeepPlan>>,
}

impl KeepState {
    /// The state of this process.
    pub(crate) fn of_this_process() -> Self {
        Self {
            administrator: OsSystem.require_administrator().is_ok(),
            shown: Mutex::new(None),
        }
    }

    fn show(&self, plan: Option<KeepPlan>) {
        *self.shown.lock().unwrap_or_else(PoisonError::into_inner) = plan;
    }

    fn take_shown(&self) -> Option<KeepPlan> {
        self.shown
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

/// What this window offers about keeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Offer {
    /// Keep the program, or update the one that is kept.
    pub(crate) keep: bool,
    /// Remove the kept program: offered by the kept program itself.
    pub(crate) remove: bool,
}

/// Decides what a customer window offers (ST-M009-017): to keep only with
/// administrator rights and only a copy that is not kept yet or is newer
/// than what is kept; to remove only in the kept program itself, with or
/// without those rights; and nothing at all while a session exists.
#[must_use]
pub(crate) fn offer(relation: Relation, administrator: bool, session: bool) -> Offer {
    if session {
        return Offer {
            keep: false,
            remove: false,
        };
    }
    Offer {
        keep: administrator && matches!(relation, Relation::NotKept | Relation::ThisIsNewer),
        remove: relation == Relation::ThisIsIt,
    }
}

/// What the window shows about a kept program (`KeepView` of the contract).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KeepView {
    /// How this copy relates to what is kept: `not_kept`, `this` (this is
    /// the kept program), `same`, `newer` (this copy is newer), `older` or
    /// `unknown` (the kept version cannot be compared).
    pub state: &'static str,
    /// Whether the window offers to keep the program, or to update the kept
    /// one.
    pub can_keep: bool,
    /// Whether the window offers to remove the kept program.
    pub can_remove: bool,
    /// Whether removing first asks Windows for administrator rights: this
    /// window runs without them.
    pub remove_elevates: bool,
    /// The version of this copy.
    pub this_version: String,
    /// The version that is kept, when its record says one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kept_version: Option<String>,
    /// The folder of the kept program; absent where no program can be kept.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

fn relation_name(relation: Relation) -> &'static str {
    match relation {
        Relation::NotKept => "not_kept",
        Relation::ThisIsIt => "this",
        Relation::Same => "same",
        Relation::ThisIsNewer => "newer",
        Relation::ThisIsOlder => "older",
        Relation::Unknown => "unknown",
    }
}

/// The view of a window whose copy is `exe`. `layout` is `None` where no
/// program can be kept (off Windows): nothing is offered then.
fn view_of(
    layout: Option<&Layout>,
    exe: Option<&Path>,
    version: &str,
    administrator: bool,
    session: bool,
) -> KeepView {
    let (Some(layout), Some(exe)) = (layout, exe) else {
        return KeepView {
            state: relation_name(Relation::NotKept),
            can_keep: false,
            can_remove: false,
            remove_elevates: false,
            this_version: version.to_owned(),
            kept_version: None,
            location: None,
        };
    };
    let status = kept::status(layout, exe, version);
    let offer = offer(status.relation, administrator, session);
    KeepView {
        state: relation_name(status.relation),
        can_keep: offer.keep,
        can_remove: offer.remove,
        remove_elevates: offer.remove && !administrator,
        this_version: version.to_owned(),
        kept_version: status.kept_version,
        location: Some(layout.folder.display().to_string()),
    }
}

fn this_view(keep: &KeepState, state: &AppState) -> KeepView {
    view_of(
        Layout::current().as_ref(),
        std::env::current_exe().ok().as_deref(),
        crate::RELEASE_VERSION,
        keep.administrator,
        session_exists(&state.guard()),
    )
}

/// The support program that is kept on this computer, for the end report of
/// a session: it is there on the customer's own choice and was not left
/// behind (REQ-F-019). `None` when nothing is kept.
pub(super) fn kept_program() -> Option<KeptProgram> {
    kept_program_in(&Layout::current()?)
}

fn kept_program_in(layout: &Layout) -> Option<KeptProgram> {
    kept::kept(layout).map(|(folder, version)| KeptProgram {
        location: folder.display().to_string(),
        version,
    })
}

/// What keeping would add, as the customer is shown it before choosing
/// (`KeepPlanView` of the contract).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KeepPlanView {
    /// The version that would be kept.
    pub version: String,
    /// The version it would replace, when a program is kept already.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replaces: Option<String>,
    /// The folder the program is written to.
    pub location: String,
    /// The kept program, by its full path.
    pub program: String,
    /// Whether the kept program has a desktop shortcut already.
    pub desktop_kept: bool,
    /// The relay that the settings of this copy name, which would be kept
    /// with the program: its name, where the copy connects and what vouches
    /// for the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay: Option<RelayUseView>,
    /// Whether relay settings that are kept already would be replaced.
    pub relay_replaces: bool,
    /// Why the relay settings next to this copy are not kept, when they
    /// cannot be used: the file and what is wrong with it, as a sentence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay_problem: Option<String>,
}

impl KeepPlanView {
    fn of(plan: &KeepPlan, layout: &Layout) -> Self {
        Self {
            version: plan.version.clone(),
            replaces: plan.replaces.clone(),
            location: layout.folder.display().to_string(),
            program: layout.exe.display().to_string(),
            desktop_kept: plan.desktop_kept,
            relay: plan.relay.as_ref().map(|relay| RelayUseView {
                host: relay.profile().server_name().to_owned(),
                address: relay.profile().address().to_string(),
                trust: if relay.has_certificate_file() {
                    RelayTrust::ProfileFile
                } else {
                    RelayTrust::SystemAuthorities
                },
            }),
            relay_replaces: plan.replaces_relay,
            relay_problem: plan.relay_problem.as_ref().map(|error| {
                let problem = relay::ProblemView::of(error);
                format!("{} The file is {}.", problem.message, problem.file)
            }),
        }
    }
}

/// What removing would remove, before the customer chooses
/// (`RemovePlanView` of the contract).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemovePlanView {
    /// What it removes, one line each.
    pub removes: Vec<String>,
    /// What stays, one line each: what CretLink did not add, and a session
    /// report that the customer saved.
    pub stays: Vec<String>,
    /// Whether the folder goes only once this window has closed.
    pub after_exit: bool,
    /// Whether Windows asks for administrator rights first.
    pub elevates: bool,
}

/// What stays when the kept program is removed, said on every plan.
const SAVED_REPORT_STAYS: &str = "A session report that you saved stays where you saved it";

impl RemovePlanView {
    fn of(plan: &kept::RemovePlan, administrator: bool) -> Self {
        let mut stays = plan.left.clone();
        stays.push(SAVED_REPORT_STAYS.to_owned());
        Self {
            removes: plan.lines(),
            stays,
            after_exit: plan.after_exit,
            elevates: !administrator,
        }
    }
}

/// How a removal went (`RemoveOutcome` of the contract).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoveOutcome {
    /// `removed` (everything is gone), `pending` (the folder goes once this
    /// window has closed) or `elevating` (Windows was asked for
    /// administrator rights; this window is closing).
    pub state: &'static str,
    /// What stays on purpose, in words.
    pub left: Vec<String>,
}

fn error(err: &kept::KeptError) -> CommandError {
    CommandError::new(err.stage(), relay::sentence(&err.to_string()))
}

fn not_offered() -> CommandError {
    CommandError::new(
        "keep_unavailable",
        "The support program can be kept only on a Windows computer.",
    )
}

/// Refuses while a session exists, and before the window is past the
/// permission step: a program is kept or removed before a session or after
/// it, never during one.
fn ensure_idle(session: bool, ready: bool) -> Result<(), CommandError> {
    if session {
        return Err(CommandError::new(
            "keep_session",
            "A session is in progress. Keeping or removing the program is offered before a \
             session and after it, never during one.",
        ));
    }
    if !ready {
        return Err(CommandError::new(
            "keep_admin",
            "Choose first whether Windows may give CretLink administrator rights.",
        ));
    }
    Ok(())
}

fn guard(state: &AppState, admin: &AdminState) -> Result<(), CommandError> {
    ensure_idle(session_exists(&state.guard()), admin.ready())
}

fn this_copy() -> Result<(Layout, PathBuf), CommandError> {
    let layout = Layout::current().ok_or_else(not_offered)?;
    let exe = std::env::current_exe().map_err(|err| {
        CommandError::new("keep_failed", format!("CretLink cannot find itself: {err}"))
    })?;
    Ok((layout, exe))
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, CommandError> + Send + 'static,
) -> Result<T, CommandError> {
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|_| CommandError::new("keep_failed", "The work could not be started."))?
}

/// What is kept on this computer, and what this window offers.
#[tauri::command]
pub(super) fn get_keep(keep: State<'_, KeepState>, state: State<'_, AppState>) -> KeepView {
    this_view(&keep, &state)
}

/// **Keep CretLink Support on this computer**: what keeping would add, with
/// the relay that the settings of this copy name. Nothing is changed. The
/// plan is kept for [`keep_program`], which carries out this one and no
/// other.
#[tauri::command]
pub(super) async fn keep_plan(
    keep: State<'_, KeepState>,
    state: State<'_, AppState>,
    admin: State<'_, AdminState>,
) -> Result<KeepPlanView, CommandError> {
    guard(&state, &admin)?;
    keep.show(None);
    let (layout, exe) = this_copy()?;
    let (plan, view) = blocking(move || {
        let plan = kept::keep_plan(&layout, &OsSystem, &exe, crate::RELEASE_VERSION)
            .map_err(|err| error(&err))?;
        let view = KeepPlanView::of(&plan, &layout);
        Ok((plan, view))
    })
    .await?;
    keep.show(Some(plan));
    Ok(view)
}

/// Keeps the program that runs, as the plan that [`keep_plan`] showed says,
/// with a desktop shortcut when the customer asked for one. Returns what is
/// kept now.
#[tauri::command]
pub(super) async fn keep_program(
    desktop: bool,
    keep: State<'_, KeepState>,
    state: State<'_, AppState>,
    admin: State<'_, AdminState>,
) -> Result<KeepView, CommandError> {
    guard(&state, &admin)?;
    let plan = keep.take_shown().ok_or_else(|| {
        CommandError::new(
            "keep_plan",
            "Look at what keeping adds first; then choose to keep the program.",
        )
    })?;
    let (layout, _) = this_copy()?;
    blocking(move || {
        kept::keep(&layout, &OsSystem, &plan, desktop)
            .map(drop)
            .map_err(|err| error(&err))
    })
    .await?;
    Ok(this_view(&keep, &state))
}

/// **Remove from this computer**: what removing would remove. Nothing is
/// changed.
#[tauri::command]
pub(super) async fn remove_plan(
    keep: State<'_, KeepState>,
    state: State<'_, AppState>,
    admin: State<'_, AdminState>,
) -> Result<RemovePlanView, CommandError> {
    guard(&state, &admin)?;
    let administrator = keep.administrator;
    let (layout, exe) = this_copy()?;
    blocking(move || {
        kept::remove_plan(&layout, &OsSystem, &exe)
            .map(|plan| RemovePlanView::of(&plan, administrator))
            .map_err(|err| error(&err))
    })
    .await
}

/// Removes the kept program. With administrator rights it is removed here:
/// the shortcuts and the entry at once, the folder once this window has
/// closed when this is the kept program itself. Without them this program is
/// started again as `uninstall` through the elevation prompt of Windows,
/// which shows the list once more and asks, and this window closes.
#[tauri::command]
pub(super) async fn remove_program(
    app: AppHandle,
    keep: State<'_, KeepState>,
    state: State<'_, AppState>,
    admin: State<'_, AdminState>,
) -> Result<RemoveOutcome, CommandError> {
    guard(&state, &admin)?;
    let (layout, exe) = this_copy()?;
    if !keep.administrator {
        let started = blocking(move || {
            crate::start_elevated(&exe, &["uninstall".to_owned()]).map_err(|err| {
                CommandError::new(
                    "keep_failed",
                    format!("Windows could not be asked for administrator rights: {err}."),
                )
            })
        })
        .await?;
        if !started {
            return Err(CommandError::new(
                "keep_declined",
                "Windows was not given permission. Nothing was removed.",
            ));
        }
        app.exit(0);
        return Ok(RemoveOutcome {
            state: "elevating",
            left: Vec::new(),
        });
    }
    let report = blocking(move || {
        let plan = kept::remove_plan(&layout, &OsSystem, &exe).map_err(|err| error(&err))?;
        kept::remove(&layout, &OsSystem, &plan).map_err(|err| error(&err))
    })
    .await?;
    if !report.complete() {
        return Err(CommandError::new(
            "keep_incomplete",
            format!("These could not be removed: {}.", report.failed.join("; ")),
        ));
    }
    Ok(RemoveOutcome {
        state: if report.pending.is_empty() {
            "removed"
        } else {
            "pending"
        },
        left: report.left,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::fs;

    use serde_json::json;

    use super::*;

    /// The places of a kept program in a temporary folder, removed
    /// afterwards.
    struct Places {
        root: PathBuf,
        layout: Layout,
    }

    impl Places {
        fn new(test: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("cretlink-app-keep-{test}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            let folder = root.join("Program Files").join("CretLink Support");
            fs::create_dir_all(&folder).unwrap();
            Self {
                layout: Layout::new(folder, &root.join("Start Menu"), &root.join("Desktop")),
                root,
            }
        }

        /// A program that was kept as `version`.
        fn kept(&self, version: &str) {
            fs::write(&self.layout.exe, b"program").unwrap();
            fs::write(
                &self.layout.record,
                format!(
                    r#"{{"schema":1,"version":"{version}","files":["CretLink-Support.exe","install.json"],"startMenu":null,"desktop":null,"uninstallEntry":null}}"#
                ),
            )
            .unwrap();
        }

        /// A copy that the customer downloaded.
        fn copy(&self) -> PathBuf {
            self.root.join("Downloads").join("CretLink-Support.exe")
        }
    }

    impl Drop for Places {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// The elevated customer role offers to keep the program; the one
    /// without administrator rights does not, and a session never does.
    #[test]
    fn the_elevated_customer_role_offers_it_and_the_unelevated_one_and_a_session_do_not() {
        // Elevated, before a session and after it.
        for relation in [Relation::NotKept, Relation::ThisIsNewer] {
            assert_eq!(
                offer(relation, true, false),
                Offer {
                    keep: true,
                    remove: false
                },
                "{relation:?}"
            );
            // Without administrator rights nothing is offered.
            assert_eq!(
                offer(relation, false, false),
                Offer {
                    keep: false,
                    remove: false
                },
                "{relation:?}"
            );
        }
        // During a session nothing is offered, whoever asks and whatever is
        // kept.
        for relation in [
            Relation::NotKept,
            Relation::ThisIsIt,
            Relation::Same,
            Relation::ThisIsNewer,
            Relation::ThisIsOlder,
            Relation::Unknown,
        ] {
            for administrator in [false, true] {
                assert_eq!(
                    offer(relation, administrator, true),
                    Offer {
                        keep: false,
                        remove: false
                    },
                    "{relation:?}"
                );
            }
        }
        // The same version, a newer one and one that cannot be compared are
        // never replaced: nothing to offer.
        for relation in [Relation::Same, Relation::ThisIsOlder, Relation::Unknown] {
            assert_eq!(
                offer(relation, true, false),
                Offer {
                    keep: false,
                    remove: false
                },
                "{relation:?}"
            );
        }
        // The kept program offers to remove itself, with or without
        // administrator rights, and never to keep itself again.
        for administrator in [false, true] {
            assert_eq!(
                offer(Relation::ThisIsIt, administrator, false),
                Offer {
                    keep: false,
                    remove: true
                }
            );
        }
    }

    #[test]
    fn the_commands_refuse_during_a_session_and_before_the_permission_step() {
        assert!(ensure_idle(false, true).is_ok());
        let during = ensure_idle(true, true).unwrap_err();
        assert_eq!(during.stage, "keep_session");
        assert!(
            during.message.contains("never during one"),
            "{}",
            during.message
        );
        assert_eq!(ensure_idle(false, false).unwrap_err().stage, "keep_admin");

        // A live dial is a session; the resting window and an ended session
        // are not.
        use super::super::ConsentSession;
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let live = ConsentSession::live_dialing(tx, None);
        assert!(ensure_idle(session_exists(&live), true).is_err());
        let resting = ConsentSession::resting(&relay::Lookup::None);
        assert!(ensure_idle(session_exists(&resting), true).is_ok());
    }

    #[test]
    fn the_view_says_what_is_kept_and_what_this_window_offers() {
        let places = Places::new("view");
        let copy = places.copy();
        // Nothing is kept: an elevated window offers to keep.
        assert_eq!(
            serde_json::to_value(view_of(
                Some(&places.layout),
                Some(&copy),
                "1.0.0",
                true,
                false
            ))
            .unwrap(),
            json!({
                "state": "not_kept",
                "canKeep": true,
                "canRemove": false,
                "removeElevates": false,
                "thisVersion": "1.0.0",
                "location": places.layout.folder.display().to_string(),
            })
        );
        // Without administrator rights it says the same and offers nothing.
        let unelevated = view_of(Some(&places.layout), Some(&copy), "1.0.0", false, false);
        assert_eq!(unelevated.state, "not_kept");
        assert!(!unelevated.can_keep && !unelevated.can_remove);

        places.kept("1.0.0");
        for (version, state, can_keep) in [
            ("1.0.0", "same", false),
            ("1.1.0", "newer", true),
            ("0.9.0", "older", false),
            ("a build of today", "unknown", false),
        ] {
            let view = view_of(Some(&places.layout), Some(&copy), version, true, false);
            assert_eq!(view.state, state, "{version}");
            assert_eq!(view.can_keep, can_keep, "{version}");
            assert!(!view.can_remove, "{version}");
            assert_eq!(view.kept_version.as_deref(), Some("1.0.0"));
            // During a session the same copy offers nothing.
            let during = view_of(Some(&places.layout), Some(&copy), version, true, true);
            assert_eq!(during.state, state);
            assert!(!during.can_keep && !during.can_remove, "{version}");
        }

        // The kept program itself: installed, with its version, and it
        // offers to remove itself. Without administrator rights Windows is
        // asked first.
        let kept = places.layout.exe.clone();
        let view = view_of(Some(&places.layout), Some(&kept), "1.0.0", true, false);
        assert_eq!(view.state, "this");
        assert!(view.can_remove && !view.can_keep && !view.remove_elevates);
        let view = view_of(Some(&places.layout), Some(&kept), "1.0.0", false, false);
        assert!(view.can_remove && view.remove_elevates);
        let during = view_of(Some(&places.layout), Some(&kept), "1.0.0", true, true);
        assert!(!during.can_remove, "never during a session");
    }

    /// Off Windows the customer role cannot keep itself, and the window is
    /// told that nothing is offered.
    #[test]
    fn where_no_program_can_be_kept_nothing_is_offered() {
        let view = view_of(None, Some(Path::new("/opt/cretlink")), "1.0.0", true, false);
        assert_eq!(
            serde_json::to_value(view).unwrap(),
            json!({
                "state": "not_kept",
                "canKeep": false,
                "canRemove": false,
                "removeElevates": false,
                "thisVersion": "1.0.0",
            })
        );
        if !cfg!(windows) {
            assert_eq!(Layout::current(), None);
            assert_eq!(kept_program(), None);
            assert!(!KeepState::of_this_process().administrator);
        }
    }

    #[test]
    fn the_end_report_names_a_kept_program_as_kept_on_purpose() {
        let places = Places::new("report");
        assert_eq!(kept_program_in(&places.layout), None);
        places.kept("1.2.0");
        assert_eq!(
            kept_program_in(&places.layout),
            Some(KeptProgram {
                location: places.layout.folder.display().to_string(),
                version: Some("1.2.0".to_owned()),
            })
        );
    }

    #[test]
    fn the_plan_names_the_relay_before_the_customer_chooses() {
        let places = Places::new("plan");
        let downloads = places.copy().parent().unwrap().to_path_buf();
        fs::create_dir_all(&downloads).unwrap();
        fs::write(places.copy(), b"program").unwrap();
        fs::write(
            downloads.join("cretlink.toml"),
            "[relay]\naddress = \"192.0.2.1:9970\"\nkey = \"key-of-ann\"\nname = \
             \"relay.example.org\"\nca = \"authority.pem\"\n",
        )
        .unwrap();
        fs::write(
            downloads.join("authority.pem"),
            b"-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        let relay = kept::read_relay_next_to(&places.copy()).unwrap();
        let plan = KeepPlan {
            version: "1.1.0".to_owned(),
            replaces: Some("1.0.0".to_owned()),
            desktop_kept: true,
            relay,
            relay_problem: None,
            replaces_relay: true,
        };

        let view = serde_json::to_value(KeepPlanView::of(&plan, &places.layout)).unwrap();

        assert_eq!(
            view,
            json!({
                "version": "1.1.0",
                "replaces": "1.0.0",
                "location": places.layout.folder.display().to_string(),
                "program": places.layout.exe.display().to_string(),
                "desktopKept": true,
                // The name, where this copy would connect, and that a file
                // of the copy vouches for the name.
                "relay": {
                    "host": "relay.example.org",
                    "address": "192.0.2.1:9970",
                    "trust": "profile_file",
                },
                "relayReplaces": true,
            })
        );
        // The key of the copy is in no view.
        assert!(!view.to_string().contains("key-of-ann"));

        // Settings that cannot be used are said with their file, and
        // nothing of what the file held is repeated.
        fs::write(
            downloads.join("cretlink.toml"),
            "[relay]\naddress = \"192.0.2.1:9970\"\nsecret = \"key-of-ann\"\n",
        )
        .unwrap();
        let plan = KeepPlan {
            relay: None,
            relay_problem: kept::read_relay_next_to(&places.copy()).err(),
            replaces_relay: false,
            ..plan
        };
        let view = KeepPlanView::of(&plan, &places.layout);
        assert!(view.relay.is_none());
        let problem = view.relay_problem.unwrap();
        assert!(
            problem.starts_with("Line 3: this is not a setting of a relay;"),
            "{problem}"
        );
        assert!(problem.ends_with("cretlink.toml."), "{problem}");
        assert!(!problem.contains("key-of-ann"), "{problem}");
    }

    #[test]
    fn the_plan_of_a_removal_says_what_goes_and_what_stays() {
        let plan = kept::RemovePlan {
            folder: PathBuf::from("/c/Program Files/CretLink Support"),
            folder_goes: true,
            files: vec![PathBuf::from(
                "/c/Program Files/CretLink Support/CretLink-Support.exe",
            )],
            shortcuts: vec![PathBuf::from("/c/Start Menu/CretLink Support.lnk")],
            uninstall_entry: None,
            after_exit: true,
            left: vec!["Something that CretLink did not add".to_owned()],
        };
        assert_eq!(
            serde_json::to_value(RemovePlanView::of(&plan, false)).unwrap(),
            json!({
                "removes": [
                    "/c/Program Files/CretLink Support/CretLink-Support.exe (removed once \
                     CretLink has closed)",
                    "The folder /c/Program Files/CretLink Support (removed once CretLink has \
                     closed)",
                    "The shortcut /c/Start Menu/CretLink Support.lnk",
                ],
                // A report that the customer saved stays, and the screen
                // says so.
                "stays": [
                    "Something that CretLink did not add",
                    "A session report that you saved stays where you saved it",
                ],
                "afterExit": true,
                "elevates": true,
            })
        );
        assert!(!RemovePlanView::of(&plan, true).elevates);
    }

    #[test]
    fn a_plan_is_carried_out_once_and_only_after_it_was_shown() {
        let keep = KeepState {
            administrator: true,
            shown: Mutex::new(None),
        };
        assert!(keep.take_shown().is_none(), "nothing was shown yet");
        keep.show(Some(KeepPlan {
            version: "1.0.0".to_owned(),
            replaces: None,
            desktop_kept: false,
            relay: None,
            relay_problem: None,
            replaces_relay: false,
        }));
        assert_eq!(
            keep.take_shown().map(|plan| plan.version).as_deref(),
            Some("1.0.0")
        );
        assert!(keep.take_shown().is_none(), "a plan is carried out once");
    }

    #[test]
    fn refusals_are_sentences_with_their_stage() {
        let older = error(&kept::KeptError::NotNewer {
            kept: Some("1.2.0".to_owned()),
            this: "1.1.0".to_owned(),
        });
        assert_eq!(older.stage, "keep_older");
        assert_eq!(
            older.message,
            "The support program that is kept on this computer (1.2.0) is not older than this \
             copy (1.1.0); it is not replaced."
        );
        assert_eq!(
            error(&kept::KeptError::NotAdministrator).stage,
            "keep_administrator"
        );
        assert_eq!(not_offered().stage, "keep_unavailable");
    }
}
