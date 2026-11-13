//! Settings → Relay of the technician role (ST-M007-011).
//!
//! | Command | What it does |
//! |---|---|
//! | [`get_relay_settings`] | which relay this copy uses and where that comes from (`RelaySettings` of the contract) |
//! | [`choose_relay_ca`] | the system's dialog for the certificate authority file of the relay; what was chosen is kept under a number |
//! | [`save_relay`] | keeps the relay of the form in the settings of the installed copy |
//! | [`test_relay`] | reaches the relay once and says in words what came of it |
//! | [`remove_relay`] | removes the relay from the settings |
//!
//! - **Only the installed copy keeps settings.** [`save_relay`] and
//!   [`remove_relay`] refuse in a copy that runs once, which writes nothing
//!   but its audit trail; such a copy takes its relay from the file
//!   `cretlink.toml` next to the program.
//! - **A file next to the program wins**, also over a relay kept here, and
//!   the view says which of the two is used
//!   ([`TechnicianRelays::active`](crate::relay::TechnicianRelays::active)).
//! - **The key never comes back.** A form sends a key to keep; nothing sends
//!   one to the window. The view says only whether a key is set, a form that
//!   leaves the key alone says `keep`, and no message names it.
//! - **The window names no path of this computer.** The certificate file is
//!   chosen in the system's dialog, opened from here, and a copy of it is
//!   kept next to the settings; the form names it by a number.
//! - **Nothing here grants anything.** A relay profile is addressing
//!   (ADR-0016 §2, §4): saving one opens no connection, and [`test_relay`]
//!   presents a ticket of its own that meets nobody.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use cretlink_cli::install::{Layout, RELAY_CA_FILE};
use cretlink_cli::relay_profile::{self, Field, Problem, RelayProfile};
use cretlink_cli::settings::{self, Settings};
use serde::{Deserialize, Serialize};
use tauri::{State, WebviewWindow};

use super::{AppState, CommandError};
use crate::relay::{
    Audience, Lookup, PlaceView, ProblemView, Relay, Source, TechnicianRelays, TestResult,
    ThisCopy, sentence,
};

/// What Settings → Relay shows (`RelaySettings` of the contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelaySettingsView {
    /// Whether this copy keeps settings: only the installed copy does.
    pub editable: bool,
    /// Which profile this copy uses: `file`, `settings` or `none`.
    pub source: &'static str,
    /// The profile file next to the program: where it is looked for, and
    /// what it holds.
    pub file: PlaceView,
    /// What the settings hold; absent in a copy that keeps none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stored: Option<PlaceView>,
}

impl RelaySettingsView {
    /// The view of what `copy` knows. Nothing is written.
    pub(crate) fn of(copy: &ThisCopy) -> Self {
        let relays = TechnicianRelays::read(copy);
        let source = match (&relays.file, &relays.stored) {
            (Lookup::None, Some(Lookup::None)) | (Lookup::None, None) => "none",
            (Lookup::None, Some(_)) => "settings",
            _ => "file",
        };
        Self {
            editable: copy.installed.is_some(),
            source,
            file: relays
                .file_path
                .as_deref()
                .map(|path| PlaceView::of(path, &relays.file))
                .unwrap_or_default(),
            stored: copy
                .installed
                .as_ref()
                .zip(relays.stored.as_ref())
                .map(|(layout, stored)| PlaceView::of(&layout.settings, stored)),
        }
    }
}

/// What to do with the admission key of the relay in the settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyMode {
    /// Leave the key that is kept as it is.
    Keep,
    /// Use no key.
    None,
    /// Use the key of the form.
    Set,
}

/// What to do with the certificate authority file of the relay in the
/// settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaMode {
    /// Leave the file that is kept as it is.
    Keep,
    /// Use the system's authorities: no file.
    None,
    /// Use the file that was chosen in the system's dialog.
    Chosen,
}

/// The relay as the form of Settings → Relay gives it (`RelayForm` of the
/// contract).
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelayForm {
    /// The relay's host name and port.
    pub address: String,
    /// What to do with the key.
    pub key_mode: KeyMode,
    /// The key, with [`KeyMode::Set`].
    #[serde(default)]
    pub key: Option<String>,
    /// What to do with the certificate authority file.
    pub ca_mode: CaMode,
    /// The number of the chosen file, with [`CaMode::Chosen`].
    #[serde(default)]
    pub ca_pick: Option<u64>,
    /// The name in the relay's certificate; empty for the host of the
    /// address.
    #[serde(default)]
    pub name: Option<String>,
}

/// The key is not printed.
impl fmt::Debug for RelayForm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RelayForm")
            .field("address", &self.address)
            .field("key_mode", &self.key_mode)
            .field("has_key", &self.key.is_some())
            .field("ca_mode", &self.ca_mode)
            .field("ca_pick", &self.ca_pick)
            .field("name", &self.name)
            .finish()
    }
}

/// A certificate authority file the technician chose in the system's dialog
/// (`ChosenCa` of the contract): the number a form names it by, and its file
/// name for the words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChosenCa {
    /// The number of this choice.
    pub pick: u64,
    /// The name of the file, without its folder.
    pub name: String,
}

/// A chosen certificate authority file: its number, and its content.
type Picked = (u64, Vec<u8>);

/// The certificate authority file that was chosen last and not saved yet.
/// Kept on this side: the window never holds a path of this computer.
#[derive(Debug, Clone, Default)]
pub(crate) struct PendingCa(Arc<Mutex<Option<Picked>>>);

/// The number of the next choice.
static NEXT_PICK: AtomicU64 = AtomicU64::new(1);

impl PendingCa {
    /// Keeps the content of a chosen file and answers its number. An earlier
    /// choice is dropped.
    pub(crate) fn keep(&self, pem: Vec<u8>) -> u64 {
        let pick = NEXT_PICK.fetch_add(1, Ordering::Relaxed);
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some((pick, pem));
        pick
    }

    /// The content of the choice `pick`, while it is the latest one.
    fn get(&self, pick: u64) -> Option<Vec<u8>> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|(kept, _)| *kept == pick)
            .map(|(_, pem)| pem.clone())
    }

    /// Forgets the choice: it was saved, or the relay was removed.
    fn clear(&self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

/// A refusal that names the setting of the form it is about: `relay_address`,
/// `relay_key`, `relay_ca`, `relay_name`, or `relay` for the whole.
fn refused(field: Option<Field>, text: &str) -> CommandError {
    let stage = match field {
        Some(Field::Address) => "relay_address",
        Some(Field::Key) => "relay_key",
        Some(Field::Ca) => "relay_ca",
        Some(Field::Name) => "relay_name",
        None => "relay",
    };
    CommandError::new(stage, sentence(text))
}

fn of_problem(problem: &Problem) -> CommandError {
    refused(problem.field, &problem.text)
}

fn of_view(problem: &ProblemView) -> CommandError {
    CommandError::new(
        "relay_profile",
        format!("{} ({})", problem.message, problem.file),
    )
}

fn not_installed() -> CommandError {
    CommandError::new(
        "relay_run_once",
        "This copy runs once and keeps no settings. To give it a relay, put a file cretlink.toml \
         next to the program.",
    )
}

fn settings_io(what: &str, error: &std::io::Error) -> CommandError {
    CommandError::new("relay_settings", format!("{what}: {error}."))
}

/// The relay of a form, with what the relay in the settings keeps. Returns
/// the profile and the PEM of its certificate authority file, when it has
/// one. Nothing is written.
fn relay_of(
    layout: &Layout,
    form: &RelayForm,
    pending: &PendingCa,
) -> Result<(RelayProfile, Option<Vec<u8>>), CommandError> {
    // A settings file that cannot be read keeps nothing: the form replaces it.
    let stored = settings::read(layout)
        .ok()
        .and_then(|settings| settings.relay);
    let key = match form.key_mode {
        KeyMode::Set => Some(form.key.as_deref().unwrap_or_default()),
        KeyMode::Keep | KeyMode::None => None,
    };
    let name = form
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());
    let mut profile = RelayProfile::from_fields(&form.address, key, None, name)
        .map_err(|problem| of_problem(&problem))?;
    if form.key_mode == KeyMode::Keep {
        profile = profile.with_key(stored.as_ref().and_then(RelayProfile::key).cloned());
    }
    let ca_pem = match form.ca_mode {
        CaMode::None => None,
        CaMode::Chosen => Some(form.ca_pick.and_then(|pick| pending.get(pick)).ok_or_else(
            || {
                refused(
                    Some(Field::Ca),
                    "the certificate file that was chosen is no longer at hand; choose it again",
                )
            },
        )?),
        CaMode::Keep => match stored.as_ref().filter(|stored| stored.ca().is_some()) {
            None => None,
            Some(stored) => relay_profile::read_ca(&layout.settings, stored)
                .map_err(|error| of_problem(&error.problem))?,
        },
    };
    if ca_pem.is_some() {
        profile = profile
            .with_ca(Some(RELAY_CA_FILE))
            .map_err(|problem| of_problem(&problem))?;
    }
    Ok((profile, ca_pem))
}

/// The relay of a form as one that can be reached, checked as the saved one
/// would be.
fn checked(
    layout: &Layout,
    form: &RelayForm,
    pending: &PendingCa,
) -> Result<(Relay, Option<Vec<u8>>), CommandError> {
    let (profile, ca_pem) = relay_of(layout, form, pending)?;
    let relay = Relay::with_ca(
        Source::Settings(layout.settings.clone()),
        profile,
        ca_pem.as_deref(),
    )
    .map_err(|error| of_problem(&error.problem))?;
    Ok((relay, ca_pem))
}

/// Keeps the relay of `form` in the settings of the installed copy `copy`.
/// The network to wait on, kept in the same file, stays as it is.
pub(crate) fn save(
    copy: &ThisCopy,
    form: &RelayForm,
    pending: &PendingCa,
) -> Result<RelaySettingsView, CommandError> {
    let layout = copy.installed.as_ref().ok_or_else(not_installed)?;
    let (relay, ca_pem) = checked(layout, form, pending)?;
    // The certificate file first: a profile never names a file that is not
    // there. A file that is kept is there already.
    if let (Some(pem), CaMode::Chosen) = (&ca_pem, form.ca_mode) {
        settings::write_relay_ca(layout, pem)
            .map_err(|error| settings_io("The certificate file could not be kept", &error))?;
    }
    let network_adapter = settings::read(layout)
        .ok()
        .and_then(|settings| settings.network_adapter);
    settings::write(
        layout,
        &Settings {
            relay: Some(relay.profile().clone()),
            network_adapter,
        },
    )
    .map_err(|error| settings_io("The settings could not be written", &error))?;
    if ca_pem.is_none() {
        settings::remove_relay_ca(layout).map_err(|error| {
            settings_io(
                "The certificate file of the earlier relay could not be removed",
                &error,
            )
        })?;
    }
    pending.clear();
    Ok(RelaySettingsView::of(copy))
}

/// Removes the relay from the settings of the installed copy `copy`, with
/// its certificate file. A settings file that cannot be read cannot be kept
/// in part: it goes as a whole.
pub(crate) fn remove(
    copy: &ThisCopy,
    pending: &PendingCa,
) -> Result<RelaySettingsView, CommandError> {
    let layout = copy.installed.as_ref().ok_or_else(not_installed)?;
    let kept = match settings::read(layout) {
        Ok(settings) => Settings {
            relay: None,
            network_adapter: settings.network_adapter,
        },
        Err(_) => Settings::default(),
    };
    settings::write(layout, &kept)
        .map_err(|error| settings_io("The settings could not be written", &error))?;
    settings::remove_relay_ca(layout)
        .map_err(|error| settings_io("The certificate file could not be removed", &error))?;
    pending.clear();
    Ok(RelaySettingsView::of(copy))
}

/// The relay to reach for a test: the one of `form` as it stands, not yet
/// saved, or without a form the one this copy uses.
pub(crate) fn relay_to_test(
    copy: &ThisCopy,
    form: Option<&RelayForm>,
    pending: &PendingCa,
) -> Result<Relay, CommandError> {
    if let Some(form) = form {
        let layout = copy.installed.as_ref().ok_or_else(not_installed)?;
        return checked(layout, form, pending).map(|(relay, _)| relay);
    }
    match TechnicianRelays::read(copy).active() {
        Lookup::Relay(relay) => Ok((**relay).clone()),
        Lookup::Unusable(error) => Err(of_view(&ProblemView::of(error))),
        Lookup::None => Err(CommandError::new(
            "relay_none",
            "This copy has no relay to test.",
        )),
    }
}

fn this_copy() -> Result<ThisCopy, CommandError> {
    ThisCopy::current()
        .ok_or_else(|| CommandError::new("current_exe", "CretLink cannot find itself."))
}

/// Which relay this copy uses, and where that comes from.
#[tauri::command]
pub(crate) async fn get_relay_settings() -> Result<RelaySettingsView, CommandError> {
    let copy = this_copy()?;
    super::setup::blocking(move || Ok(RelaySettingsView::of(&copy))).await
}

/// Opens the system's dialog for the certificate authority file of the
/// relay, reads the file that was chosen and keeps its content for the next
/// [`save_relay`] or [`test_relay`]. Answers `null` when the dialog was
/// closed without a choice.
#[tauri::command]
pub(crate) async fn choose_relay_ca(
    window: WebviewWindow,
    state: State<'_, AppState>,
) -> Result<Option<ChosenCa>, CommandError> {
    let Some(file) = rfd::AsyncFileDialog::new()
        .set_parent(&window)
        .set_title("The certificate authority of your relay")
        .add_filter("Certificates (PEM)", &["pem", "crt", "cer"])
        .add_filter("All files", &["*"])
        .pick_file()
        .await
    else {
        return Ok(None);
    };
    let path = file.path().to_path_buf();
    let name = file.file_name();
    let pem = super::setup::blocking(move || read_chosen_ca(&path)).await?;
    Ok(Some(ChosenCa {
        pick: state.relay_ca.keep(pem),
        name,
    }))
}

/// Reads a certificate file the technician chose and checks that it holds a
/// certificate that can be used.
pub(crate) fn read_chosen_ca(path: &std::path::Path) -> Result<Vec<u8>, CommandError> {
    let pem = relay_profile::read_certificate_file(path).map_err(|problem| of_problem(&problem))?;
    cretlink_session::WssRelayDial::connector(Some(&pem)).map_err(|_| {
        refused(
            Some(Field::Ca),
            "the certificate file holds no certificate that can be used",
        )
    })?;
    Ok(pem)
}

/// Keeps the relay of the form in the settings of the installed copy.
#[tauri::command]
pub(crate) async fn save_relay(
    state: State<'_, AppState>,
    form: RelayForm,
) -> Result<RelaySettingsView, CommandError> {
    let copy = this_copy()?;
    let pending = state.relay_ca.clone();
    super::setup::blocking(move || save(&copy, &form, &pending)).await
}

/// Removes the relay from the settings of the installed copy.
#[tauri::command]
pub(crate) async fn remove_relay(
    state: State<'_, AppState>,
) -> Result<RelaySettingsView, CommandError> {
    let copy = this_copy()?;
    let pending = state.relay_ca.clone();
    super::setup::blocking(move || remove(&copy, &pending)).await
}

/// Reaches the relay once and says in words what came of it: the relay of
/// `form` as it stands in the form, or without a form the one this copy
/// uses. Nothing is saved.
#[tauri::command]
pub(crate) async fn test_relay(
    state: State<'_, AppState>,
    form: Option<RelayForm>,
) -> Result<TestResult, CommandError> {
    let copy = this_copy()?;
    let pending = state.relay_ca.clone();
    let relay =
        super::setup::blocking(move || relay_to_test(&copy, form.as_ref(), &pending)).await?;
    Ok(relay.test(Audience::Technician).await)
}

#[cfg(test)]
mod tests;
