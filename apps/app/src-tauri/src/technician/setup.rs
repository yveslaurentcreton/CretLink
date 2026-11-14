//! This copy of CretLink, its AI clients, and run once or install
//! (ST-M009-006, ST-M009-008).
//!
//! The Tauri commands over [`cretlink_cli::setup`] and [`cretlink_cli::install`]:
//!
//! - in the technician role: [`get_app_info`], [`ai_clients`],
//!   [`ai_client_install`], [`ai_client_uninstall`], [`uninstall_plan`] and
//!   [`uninstall_app`] (the AI assistants and Settings screens);
//! - on the start screen: [`get_app_info`], [`install_options`],
//!   [`install_app`] and [`open_installed`] (`#start`, `#t-install`).
//!
//! Every type that crosses to the frontend serialises exactly to
//! `apps/app/src/shared/contract.ts`. Client command lines run on a blocking
//! thread, never on the window's. The customer role registers none of these.

use std::path::PathBuf;

use cretlink_cli::install::{self, InstallError, Layout, OsSystem, System as _};
use cretlink_cli::setup::{self, Action, Client, ClientStatus, RegistrationState, SystemRunner};
use cretlink_console::AuditServiceConfig;
use serde::Serialize;
use tauri::{AppHandle, State};

use super::CommandError;
use crate::profile::Profile;

/// Whether this copy runs from the install location (`AppInfo.install`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallKind {
    /// It is the installed copy.
    Installed,
    /// It runs from wherever it was unpacked and writes nothing but the audit
    /// trail.
    RunOnce,
}

/// This copy of the application (`AppInfo` of the contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    /// This computer's host name.
    pub hostname: String,
    /// This computer's operating system.
    pub os: String,
    /// The release version.
    pub version: String,
    /// Installed or run once.
    pub install: InstallKind,
    /// The install location, or the folder this copy runs from.
    pub location: String,
    /// The console's audit folder.
    pub audit_dir: String,
}

/// A client's registration (`ClientRegistration` of the contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ClientRegistration {
    /// No `cretlink` entry.
    Absent,
    /// The entry starts this copy.
    ThisExecutable {
        /// The command it starts.
        command: String,
    },
    /// The entry starts another command.
    OtherCommand {
        /// The command it starts.
        command: String,
    },
    /// The entry or the configuration could not be read.
    Unreadable {
        /// Why.
        message: String,
    },
}

/// One AI client (`AiClientStatus` of the contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiClientStatus {
    /// `claude`, `codex` or `gemini`.
    pub id: &'static str,
    /// The product name.
    pub label: &'static str,
    /// Whether its command line is on `PATH`.
    pub present: bool,
    /// Its per-user configuration file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
    /// Its registration.
    pub registration: ClientRegistration,
    /// The exact commands install would run.
    pub install_plan: Vec<String>,
    /// The exact commands uninstall would run.
    pub uninstall_plan: Vec<String>,
}

impl AiClientStatus {
    /// The contract view of the setup interface's status.
    #[must_use]
    pub fn of(status: &ClientStatus) -> Self {
        let registration = match &status.registration {
            RegistrationState::Absent => ClientRegistration::Absent,
            RegistrationState::ThisExecutable(registration) => ClientRegistration::ThisExecutable {
                command: registration.display(),
            },
            RegistrationState::OtherCommand(registration) => ClientRegistration::OtherCommand {
                command: registration.display(),
            },
            RegistrationState::Unreadable(message) => ClientRegistration::Unreadable {
                message: message.clone(),
            },
        };
        let shown = |plan: &[setup::PlannedCommand]| {
            plan.iter()
                .map(setup::PlannedCommand::display)
                .collect::<Vec<_>>()
        };
        Self {
            id: status.client.program(),
            label: status.client.label(),
            present: status.present(),
            config_path: status
                .config_path
                .as_ref()
                .map(|path| path.display().to_string()),
            registration,
            install_plan: shown(&status.install_plan),
            uninstall_plan: shown(&status.uninstall_plan),
        }
    }
}

/// What the install screen offers (`InstallOptions` of the contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallOptions {
    /// Where install puts CretLink.
    pub location: String,
    /// The menu entry install can add: `start_menu`, `desktop_entry`, or none
    /// (macOS, where the bundle is the entry).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<&'static str>,
    /// Where that entry goes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_path: Option<String>,
    /// Each client, planned against the installed program.
    pub clients: Vec<AiClientStatus>,
    /// Whether a copy is installed already, so install updates it in place.
    pub update: bool,
    /// Whether the installed copy runs now, so install would stop.
    pub running: bool,
}

/// How an install ended (`InstallOutcome` of the contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallOutcome {
    /// `started`: the installed copy is starting and this window closes.
    /// `installed`: installed, but something needs the technician first.
    pub state: &'static str,
    /// The install location.
    pub location: String,
    /// What did not work, in words.
    pub failures: Vec<String>,
}

fn layout() -> Result<Layout, CommandError> {
    Layout::current().ok_or_else(|| {
        CommandError::new(
            "install_location",
            "CretLink cannot find your user folders (HOME, LOCALAPPDATA or APPDATA is not set).",
        )
    })
}

fn current_exe() -> Result<PathBuf, CommandError> {
    std::env::current_exe().map_err(|err| {
        CommandError::new("current_exe", format!("CretLink cannot find itself: {err}"))
    })
}

fn install_error(err: &InstallError) -> CommandError {
    let stage = match err {
        InstallError::CustomerCopy => "customer_copy",
        InstallError::AlreadyInstalled(_) => "already_installed",
        InstallError::NotInstalled(_) => "not_installed",
        InstallError::Running(_) => "running",
        InstallError::Io { .. } => "install_io",
    };
    let mut message = err.to_string();
    if let Some(first) = message.get(..1) {
        let upper = first.to_uppercase();
        message.replace_range(..1, &upper);
    }
    message.push('.');
    CommandError::new(stage, message)
}

/// Runs blocking work (client command lines, file copies) off the window's
/// thread.
pub(super) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, CommandError> + Send + 'static,
) -> Result<T, CommandError> {
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|err| CommandError::new("internal", format!("the work did not finish: {err}")))?
}

/// Builds [`AppInfo`] for this process.
pub(crate) fn app_info() -> AppInfo {
    let host = super::technician_host();
    let exe = std::env::current_exe().ok();
    let layout = Layout::current();
    let installed =
        matches!((&layout, &exe), (Some(layout), Some(exe)) if layout.is_installed_exe(exe));
    let location = match (&layout, &exe) {
        (Some(layout), _) if installed => layout.location.clone(),
        (_, Some(exe)) => exe.parent().map(PathBuf::from).unwrap_or_default(),
        _ => PathBuf::new(),
    };
    AppInfo {
        hostname: host.hostname,
        os: host.os,
        version: crate::RELEASE_VERSION.to_owned(),
        install: if installed {
            InstallKind::Installed
        } else {
            InstallKind::RunOnce
        },
        location: location.display().to_string(),
        audit_dir: AuditServiceConfig::default().dir.display().to_string(),
    }
}

/// This copy: host, version, installed or run once, where it runs from and the
/// audit folder.
#[tauri::command]
pub(crate) fn get_app_info() -> AppInfo {
    app_info()
}

/// Parses a contract `ClientId`, exactly.
fn client_id(client: &str) -> Result<Client, CommandError> {
    Client::ALL
        .into_iter()
        .find(|known| known.program() == client)
        .ok_or_else(|| {
            CommandError::new(
                "unknown_client",
                format!("'{client}' is not an AI client CretLink knows."),
            )
        })
}

fn statuses(context: &setup::Context) -> Vec<AiClientStatus> {
    setup::inspect_clients(context, &[])
        .iter()
        .map(AiClientStatus::of)
        .collect()
}

/// Each AI client, planned against this copy.
#[tauri::command]
pub(crate) async fn ai_clients() -> Result<Vec<AiClientStatus>, CommandError> {
    let exe = current_exe()?;
    blocking(move || Ok(statuses(&setup::Context::from_env(exe)))).await
}

fn change_client(client: &str, action: Action) -> Result<Vec<AiClientStatus>, CommandError> {
    let client = client_id(client)?;
    let exe = current_exe()?;
    let context = setup::Context::from_env(exe.clone());
    let status = setup::inspect(&context, client);
    setup::apply(&SystemRunner, &status, action)
        .map_err(|err| CommandError::new("ai_client", format!("{err}.")))?;
    if action == Action::Install
        && let Some(layout) = Layout::current().filter(|layout| layout.is_installed_exe(&exe))
    {
        // The installed copy's own registrations go into its record, so
        // uninstall removes them too (REQ-F-024).
        install::record_registration(&layout, client).map_err(|reason| {
            CommandError::new(
                "install_record",
                format!(
                    "{} is connected, but the install record could not be updated, so uninstall \
                     will not remove this registration: {reason}",
                    client.label()
                ),
            )
        })?;
    }
    Ok(statuses(&context))
}

/// Runs the client's install plan, exactly as `ai_clients` showed it.
#[tauri::command]
pub(crate) async fn ai_client_install(client: String) -> Result<Vec<AiClientStatus>, CommandError> {
    blocking(move || change_client(&client, Action::Install)).await
}

/// Runs the client's uninstall plan, exactly as `ai_clients` showed it.
#[tauri::command]
pub(crate) async fn ai_client_uninstall(
    client: String,
) -> Result<Vec<AiClientStatus>, CommandError> {
    blocking(move || change_client(&client, Action::Uninstall)).await
}

/// The uninstall plan. `own_profile` is whether this window's webview holds
/// the installed copy's profile: it then goes once the window has closed,
/// and the plan says so (ST-M009-014).
fn plan(
    layout: &Layout,
    exe: &std::path::Path,
    own_profile: bool,
) -> Result<install::UninstallPlan, CommandError> {
    let plan = install::uninstall_plan(
        layout,
        &OsSystem,
        &setup::Context::from_env(exe.to_path_buf()),
        exe,
    )
    .map_err(|err| install_error(&err))?;
    Ok(if own_profile {
        plan.from_its_window()
    } else {
        plan
    })
}

/// What uninstall would do, before anything changes (`UninstallPlan` of the
/// contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UninstallPlanView {
    /// What it removes, one line each.
    pub removes: Vec<String>,
    /// What it leaves in place, one line each: a registration that points
    /// elsewhere now, and a relay profile next to the installed program with
    /// whether it holds an admission key (ST-M007-011). The audit trail
    /// stays too; the screen says that itself.
    pub stays: Vec<String>,
}

impl UninstallPlanView {
    fn of(plan: install::UninstallPlan) -> Self {
        Self {
            removes: plan.lines(),
            stays: plan.kept,
        }
    }
}

/// What uninstall would remove and what it would leave, before anything
/// changes.
#[tauri::command]
pub(crate) async fn uninstall_plan(
    profile: State<'_, Profile>,
) -> Result<UninstallPlanView, CommandError> {
    let exe = current_exe()?;
    let own_profile = profile.is_installed();
    blocking(move || Ok(UninstallPlanView::of(plan(&layout()?, &exe, own_profile)?))).await
}

/// Uninstalls this copy. On Windows the folder goes once this window closes;
/// so does, everywhere, the webview profile this window holds (ST-M009-014).
/// A partial uninstall is an error that lists what is left.
#[tauri::command]
pub(crate) async fn uninstall_app(profile: State<'_, Profile>) -> Result<(), CommandError> {
    let exe = current_exe()?;
    let own_profile = profile.is_installed();
    let report = blocking(move || {
        let layout = layout()?;
        let plan = plan(&layout, &exe, own_profile)?;
        install::uninstall(
            &layout,
            &OsSystem,
            &SystemRunner,
            &setup::Context::from_env(exe.clone()),
            &plan,
            &exe,
        )
        .map_err(|err| install_error(&err))
    })
    .await?;
    if own_profile {
        // The uninstall ran: this process removes its profile at its end.
        profile.remove_when_closed();
    }
    if report.complete() {
        Ok(())
    } else {
        Err(CommandError::new(
            "uninstall_incomplete",
            format!("These could not be removed: {}.", report.failed.join("; ")),
        ))
    }
}

/// What the install screen shows: the location, the menu entry, and each
/// client with the change install would make.
#[tauri::command]
pub(crate) async fn install_options() -> Result<InstallOptions, CommandError> {
    blocking(|| {
        let layout = layout()?;
        let context = setup::Context::from_env(layout.exe.clone());
        // A process list that cannot be read never hides the screen; install
        // itself checks again and stops honestly.
        let running = [&layout.exe, &layout.cli]
            .into_iter()
            .any(|exe| OsSystem.is_running(exe).unwrap_or(false));
        Ok(InstallOptions {
            location: layout.location.display().to_string(),
            entry: layout.platform.entry_kind().map(|kind| match kind {
                install::EntryKind::StartMenu => "start_menu",
                install::EntryKind::DesktopEntry => "desktop_entry",
            }),
            entry_path: layout.entry.as_ref().map(|path| path.display().to_string()),
            clients: statuses(&context),
            update: layout.is_installed(),
            running,
        })
    })
    .await
}

/// Installs this copy with the chosen entry and clients. When everything
/// worked, it starts the installed copy in the technician role and closes this
/// window; otherwise it says what needs attention and stays.
#[tauri::command]
pub(crate) async fn install_app(
    app: AppHandle,
    entry: bool,
    clients: Vec<String>,
) -> Result<InstallOutcome, CommandError> {
    let clients = clients
        .iter()
        .map(|client| client_id(client))
        .collect::<Result<Vec<_>, _>>()?;
    let source = current_exe()?;
    let (layout, report) = blocking(move || {
        let layout = layout()?;
        let report = install::install(
            &layout,
            &OsSystem,
            &SystemRunner,
            &setup::Context::from_env(source.clone()),
            &install::InstallRequest {
                source,
                entry,
                clients,
            },
        )
        .map_err(|err| install_error(&err))?;
        Ok((layout, report))
    })
    .await?;
    let location = report.location.display().to_string();
    if !report.failures.is_empty() {
        return Ok(InstallOutcome {
            state: "installed",
            location,
            failures: report.failures,
        });
    }
    match install::start_installed(&layout) {
        Ok(()) => {
            app.exit(0);
            Ok(InstallOutcome {
                state: "started",
                location,
                failures: Vec::new(),
            })
        }
        Err(err) => Ok(InstallOutcome {
            state: "installed",
            location,
            failures: vec![format!(
                "The installed CretLink could not be started: {err}"
            )],
        }),
    }
}

/// Starts the installed copy in the technician role and closes this window.
#[tauri::command]
pub(crate) fn open_installed(app: AppHandle) -> Result<(), CommandError> {
    let layout = layout()?;
    install::start_installed(&layout).map_err(|err| {
        CommandError::new(
            "start_installed",
            format!("The installed CretLink could not be started: {err}."),
        )
    })?;
    app.exit(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cretlink_cli::setup::{PlannedCommand, Registration};
    use serde_json::json;

    #[test]
    fn the_uninstall_plan_says_what_goes_and_what_stays() {
        use std::path::PathBuf;

        let stays = "The relay profile /home/sam/.local/bin/cretlink.toml, which CretLink did not \
                     write and does not remove. It holds the admission key of a relay in readable \
                     form: remove the file yourself when nobody else should be able to read that \
                     key";
        let plan = install::UninstallPlan {
            location: PathBuf::from("/home/sam/.local/bin"),
            files: vec![PathBuf::from("/home/sam/.local/bin/cretlink")],
            settings: vec![PathBuf::from(
                "/home/sam/.local/share/CretLink/settings.toml",
            )],
            after_exit: false,
            entry: None,
            uninstall_entry: false,
            registrations: vec![],
            kept: vec![stays.to_owned()],
            webview: Some(PathBuf::from("/home/sam/.local/share/CretLink/webview")),
            webview_after_close: true,
            legacy_webview: None,
            audit_dir: PathBuf::from("/home/sam/.local/share/CretLink/audit"),
        };
        // A relay profile next to the program is among what stays, never
        // among what is removed (ST-M007-011).
        assert_eq!(
            serde_json::to_value(UninstallPlanView::of(plan)).unwrap(),
            json!({
                "removes": [
                    "/home/sam/.local/bin/cretlink",
                    "Your settings (the relay with its key, the network to wait on): \
                     /home/sam/.local/share/CretLink/settings.toml",
                    // The profile of the window that runs the uninstall goes
                    // when that window has closed (ST-M009-014).
                    "The webview profile /home/sam/.local/share/CretLink/webview (removed once \
                     CretLink has closed)",
                ],
                "stays": [stays],
            })
        );
    }

    fn status(registration: RegistrationState) -> ClientStatus {
        ClientStatus {
            client: Client::Codex,
            program: Some(PathBuf::from("/usr/bin/codex")),
            config_path: Some(PathBuf::from("/home/ann/.codex/config.toml")),
            registration,
            install_plan: vec![PlannedCommand {
                program: "codex",
                args: vec![
                    "mcp".into(),
                    "add".into(),
                    "cretlink".into(),
                    "--".into(),
                    "/opt/my apps/cretlink".into(),
                    "mcp".into(),
                ],
            }],
            uninstall_plan: vec![],
        }
    }

    #[test]
    fn a_client_serialises_to_the_contract() {
        let view = AiClientStatus::of(&status(RegistrationState::OtherCommand(Registration {
            command: "/old/cretlink".into(),
            args: vec!["mcp".into()],
        })));
        assert_eq!(
            serde_json::to_value(&view).unwrap(),
            json!({
                "id": "codex",
                "label": "Codex CLI",
                "present": true,
                "configPath": "/home/ann/.codex/config.toml",
                "registration": { "state": "other_command", "command": "/old/cretlink mcp" },
                "installPlan": ["codex mcp add cretlink -- \"/opt/my apps/cretlink\" mcp"],
                "uninstallPlan": [],
            })
        );
        let absent = AiClientStatus::of(&ClientStatus {
            program: None,
            config_path: None,
            ..status(RegistrationState::Absent)
        });
        let value = serde_json::to_value(&absent).unwrap();
        assert_eq!(value["registration"], json!({ "state": "absent" }));
        assert_eq!(value["present"], json!(false));
        assert!(value.get("configPath").is_none());
        let unreadable = AiClientStatus::of(&status(RegistrationState::Unreadable("bad".into())));
        assert_eq!(
            serde_json::to_value(&unreadable).unwrap()["registration"],
            json!({ "state": "unreadable", "message": "bad" })
        );
    }

    #[test]
    fn app_info_serialises_to_the_contract() {
        let info = app_info();
        let value = serde_json::to_value(&info).unwrap();
        for key in [
            "hostname", "os", "version", "install", "location", "auditDir",
        ] {
            assert!(value.get(key).is_some(), "{key}: {value}");
        }
        // A test binary never runs from the install location.
        assert_eq!(value["install"], json!("run_once"));
        assert_eq!(value["version"], json!(crate::RELEASE_VERSION));
    }

    #[test]
    fn the_audit_folder_is_the_consoles() {
        assert_eq!(install::audit_dir(), AuditServiceConfig::default().dir);
        assert_eq!(
            app_info().audit_dir,
            AuditServiceConfig::default().dir.display().to_string()
        );
    }

    #[test]
    fn install_options_and_outcome_serialise_to_the_contract() {
        let options = InstallOptions {
            location: "/home/ann/.local/bin".into(),
            entry: Some("desktop_entry"),
            entry_path: Some("/home/ann/.local/share/applications/cretlink.desktop".into()),
            clients: vec![],
            update: false,
            running: false,
        };
        assert_eq!(
            serde_json::to_value(&options).unwrap(),
            json!({
                "location": "/home/ann/.local/bin",
                "entry": "desktop_entry",
                "entryPath": "/home/ann/.local/share/applications/cretlink.desktop",
                "clients": [],
                "update": false,
                "running": false,
            })
        );
        let mac = InstallOptions {
            entry: None,
            entry_path: None,
            ..options
        };
        assert!(serde_json::to_value(&mac).unwrap().get("entry").is_none());
        assert_eq!(
            serde_json::to_value(InstallOutcome {
                state: "installed",
                location: "/x".into(),
                failures: vec!["Codex CLI: failed".into()],
            })
            .unwrap(),
            json!({ "state": "installed", "location": "/x", "failures": ["Codex CLI: failed"] })
        );
    }

    #[test]
    fn only_the_contract_client_ids_are_accepted() {
        assert_eq!(client_id("claude").unwrap(), Client::Claude);
        assert_eq!(client_id("codex").unwrap(), Client::Codex);
        assert_eq!(client_id("gemini").unwrap(), Client::Gemini);
        for bad in ["claude-code", "CLAUDE", "copilot", ""] {
            assert_eq!(client_id(bad).unwrap_err().stage, "unknown_client", "{bad}");
        }
    }

    #[test]
    fn install_errors_name_their_stage() {
        let err = install_error(&InstallError::Running(PathBuf::from("/x/cretlink")));
        assert_eq!(err.stage, "running");
        assert!(
            err.message.starts_with("The installed CretLink is running"),
            "{}",
            err.message
        );
        assert_eq!(
            install_error(&InstallError::CustomerCopy).stage,
            "customer_copy"
        );
    }
}
