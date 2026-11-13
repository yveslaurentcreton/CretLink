//! The commands of the file explorer that send, copy, rename and delete
//! (ST-M010-002, REQ-F-028).
//!
//! | Command | What it does |
//! |---|---|
//! | [`explorer_pick_upload`] | the system's dialog for files or for a folder; what was picked is kept as an offer |
//! | [`explorer_send_check`] | looks at the folder on the customer computer for names of an offer that are taken |
//! | [`explorer_send`] | queues an offer for that folder, with the technician's choices |
//! | [`explorer_choose_folder`] | the system's dialog for a folder of this computer to copy to |
//! | [`explorer_download_check`] | looks at the folder on this computer for selected names that are taken |
//! | [`explorer_download`] | queues the selection for a copy to this computer |
//! | [`explorer_transfers`] | the queue as it is |
//! | [`explorer_cancel_transfer`], [`explorer_cancel_transfers`] | cancels one transfer, or all that wait or run |
//! | [`explorer_show_folder`] | shows the folder of a copy that arrived in the file manager |
//! | [`explorer_rename`] | renames one entry in its folder |
//! | [`explorer_delete`], [`explorer_cancel_delete`] | deletes the selection, and cancels that |
//!
//! Files dropped on the window become an offer too ([`dropped`]), delivered
//! on [`DROP_EVENT`].
//!
//! # No authority of its own (REQ-S-023)
//!
//! Every command that asks something of the customer computer, or that
//! prepares to, refuses with `no_consented_session` outside a session under
//! full control with an open audit trail, before anything else: no dialog
//! opens and nothing is kept. The four that only look at what this side
//! holds — the queue, a cancel, the folder of a copy — work whenever the
//! window asks, and reach nothing on the customer computer.
//!
//! # What the window can name
//!
//! - **A place on the customer computer**: a folder as text, and names in
//!   it. The folder must be a rooted path on a drive
//!   ([`cretlink_protocol::validate_remote_absolute_path`]); a name must be
//!   one name. Neither is ever opened on this computer.
//! - **Nothing on this computer.** A send names an offer by its number: the
//!   paths come from the system's dialog or from a drop, never from the
//!   window. A copy goes to `Downloads\CretLink\<target>` or to a folder the
//!   technician chose in the system's dialog, named by its number; the names
//!   of what is copied pass [`super::local`] first. The folder that
//!   [`explorer_show_folder`] shows is the one a copy of the queue went to.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use cretlink_protocol::validate_remote_absolute_path;
use cretlink_session::CancelToken;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State, WebviewWindow};

use super::super::{AppState, CommandError, no_session};
use super::changes::{self, DeleteItem, DeleteReport, MAX_DELETE_ITEMS, RemovalProgress};
use super::explorer_error;
use super::local;
use super::target::LiveFiles;
use super::transfers::{
    self, Check, CheckError, Choice, Emit, EntryKind, OfferView, Queued, Refused, TransferView,
};

/// The Tauri event that says what the technician dropped on the window: one
/// [`OfferView`].
pub(crate) const DROP_EVENT: &str = "console-explorer-drop";

/// The Tauri event the queue's changes are delivered on: one
/// [`TransfersEvent`] each.
pub(crate) const TRANSFERS_EVENT: &str = "console-transfers";

/// The Tauri event a delete says where it stands on: one [`RemovalProgress`].
pub(crate) const REMOVAL_EVENT: &str = "console-removal";

/// Entries of the queue that changed (`TransfersEvent` in the frontend
/// contract). The window replaces each one it has by its number, adds the
/// others, and drops a folder that says its files are queued.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct TransfersEvent {
    pub(crate) entries: Vec<TransferView>,
}

/// A selected entry of the open folder, as the window listed it.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Selected {
    pub(crate) name: String,
    pub(crate) kind: EntryKind,
}

/// A folder of this computer the technician chose (`ChosenFolder` in the
/// frontend contract): the number a copy names it by, and its path for the
/// window's words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ChosenFolder {
    pub(crate) folder: u64,
    pub(crate) path: String,
}

/// The deletes that run, so that the window can cancel one by its number.
#[derive(Debug, Clone, Default)]
pub(crate) struct Removals(Arc<Mutex<HashMap<u64, CancelToken>>>);

impl Removals {
    fn begin(&self, removal: u64) -> CancelToken {
        let token = CancelToken::new();
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(removal, token.clone());
        token
    }

    fn end(&self, removal: u64) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&removal);
    }

    pub(crate) fn cancel(&self, removal: u64) {
        if let Some(token) = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&removal)
        {
            token.cancel();
        }
    }
}

/// A refusal made on this computer, for the window.
fn refused(Refused(message): Refused) -> CommandError {
    CommandError::new("refused", message)
}

/// The folder `text` names on the customer computer, as the requests spell
/// it: a rooted path on a drive, with backslashes and without one at its
/// end, a drive's root excepted.
///
/// # Errors
///
/// `invalid_path` for a text that is no such path. Nothing was asked of the
/// customer computer.
pub(crate) fn remote_folder(text: &str) -> Result<String, CommandError> {
    let spelled: String = text
        .trim()
        .chars()
        .map(|c| if c == '/' { '\\' } else { c })
        .collect();
    validate_remote_absolute_path("folder", &spelled).map_err(|error| {
        CommandError::new(
            "invalid_path",
            format!("this is not a folder on a drive of the customer computer: {error}"),
        )
    })?;
    let trimmed = spelled.trim_end_matches('\\');
    // A drive alone is its root.
    Ok(if trimmed.len() <= 2 {
        format!("{trimmed}\\")
    } else {
        trimmed.to_owned()
    })
}

/// Where the queue's changes go: the window, on [`TRANSFERS_EVENT`].
fn emitter(app: &AppHandle) -> Emit {
    let app = app.clone();
    Arc::new(move |entries| {
        // A window that is gone is not an error of the transfers.
        let _ = app.emit(TRANSFERS_EVENT, TransfersEvent { entries });
    })
}

/// Starts a worker for the queue when entries wait and none carries them.
fn carry(app: &AppHandle, state: &AppState) {
    if let Some(epoch) = state.transfers.must_start() {
        let live = state.clone();
        tauri::async_runtime::spawn(transfers::run(
            state.transfers.clone(),
            epoch,
            move || live.files().map(LiveFiles::new),
            emitter(app),
        ));
    }
}

/// Files and folders were dropped on the window. While a session is under
/// full control they are kept as an offer and the window is told; outside
/// one nothing is kept and nothing is said.
pub(crate) fn dropped(app: &AppHandle, paths: &[PathBuf]) {
    let state = app.state::<AppState>();
    if paths.is_empty() || state.files().is_none() {
        return;
    }
    let offer = state.transfers.offer(paths.to_vec());
    let _ = app.emit(DROP_EVENT, &offer);
}

/// Looks at `folder` on the customer computer for names of the offer that
/// are taken. See [`explorer_send_check`].
pub(crate) async fn send_check(
    state: &AppState,
    offer: u64,
    folder: &str,
) -> Result<Check, CommandError> {
    let live = state.files().map(LiveFiles::new).ok_or_else(no_session)?;
    let folder = remote_folder(folder)?;
    state
        .transfers
        .check_send(&live, offer, &folder)
        .await
        .map_err(|error| match error {
            CheckError::Refused(why) => refused(why),
            CheckError::Target(error) => explorer_error(error),
        })
}

/// Queues the offer for `folder`. See [`explorer_send`]. Returns what was
/// queued and the entries to deliver to the window.
pub(crate) fn send(
    state: &AppState,
    offer: u64,
    folder: &str,
    choices: &HashMap<String, Choice>,
) -> Result<(Queued, Vec<TransferView>), CommandError> {
    state.files().ok_or_else(no_session)?;
    let folder = remote_folder(folder)?;
    state
        .transfers
        .queue_send(offer, &folder, choices)
        .map_err(refused)
}

/// The folder on this computer a copy goes to: the one the technician chose
/// under the number `to`, or `Downloads\CretLink\<target>` below `downloads`.
pub(crate) fn download_root(
    state: &AppState,
    downloads: Option<PathBuf>,
    to: Option<u64>,
) -> Result<PathBuf, CommandError> {
    let (service, _) = state.files().ok_or_else(no_session)?;
    match to {
        Some(chosen) => state.transfers.chosen(chosen).ok_or_else(|| {
            CommandError::new(
                "refused",
                "the folder that was chosen is no longer kept; choose it again",
            )
        }),
        None => downloads
            .map(|downloads| local::default_folder(&downloads, &service.target().hostname))
            .ok_or_else(|| {
                CommandError::new(
                    "local",
                    "the Downloads folder of this account was not found; choose a folder \
                     with Download to…",
                )
            }),
    }
}

/// The selection as the queue takes it; a name that is no single name is
/// refused whole.
fn selection(names: &[Selected]) -> Result<Vec<(String, EntryKind)>, CommandError> {
    if names.is_empty() {
        return Err(CommandError::new("refused", "nothing is selected"));
    }
    names
        .iter()
        .map(|selected| {
            changes::one_name(&selected.name).map_err(explorer_error)?;
            Ok((selected.name.clone(), selected.kind))
        })
        .collect()
}

/// Queues the selection for a copy into `root`. See [`explorer_download`].
pub(crate) fn download(
    state: &AppState,
    folder: &str,
    names: &[Selected],
    root: &std::path::Path,
    choices: &HashMap<String, Choice>,
) -> Result<(Queued, Vec<TransferView>), CommandError> {
    state.files().ok_or_else(no_session)?;
    let folder = remote_folder(folder)?;
    let names = selection(names)?;
    state
        .transfers
        .queue_download(&folder, &names, root, choices)
        .map_err(refused)
}

/// Renames one entry. See [`explorer_rename`].
pub(crate) async fn rename(
    state: &AppState,
    folder: &str,
    name: &str,
    to: &str,
) -> Result<(), CommandError> {
    let live = state.files().map(LiveFiles::new).ok_or_else(no_session)?;
    let folder = remote_folder(folder)?;
    changes::rename(&live, &folder, name, to)
        .await
        .map_err(explorer_error)
}

/// Deletes the selection. See [`explorer_delete`].
pub(crate) async fn delete(
    state: &AppState,
    folder: &str,
    items: &[DeleteItem],
    removal: u64,
    progress: Arc<dyn Fn(RemovalProgress) + Send + Sync>,
) -> Result<DeleteReport, CommandError> {
    let live = state.files().map(LiveFiles::new).ok_or_else(no_session)?;
    let folder = remote_folder(folder)?;
    if items.is_empty() {
        return Err(CommandError::new("refused", "nothing is selected"));
    }
    if items.len() > MAX_DELETE_ITEMS {
        return Err(CommandError::new(
            "refused",
            format!(
                "{} items are selected, and one delete takes at most {MAX_DELETE_ITEMS}; \
                 select fewer, or delete the folder that holds them",
                items.len()
            ),
        ));
    }
    let cancel = state.removals.begin(removal);
    let report = changes::delete(&live, removal, &folder, items, &cancel, progress).await;
    state.removals.end(removal);
    Ok(report)
}

/// Opens the system's dialog for files, or for one folder, to send to the
/// open folder, and keeps what the technician picked as an offer
/// (ST-M010-002). `kind` is `"files"` or `"folder"`. Answers `null` when the
/// technician closed the dialog without a choice.
///
/// Fails closed with `no_consented_session` outside a session under full
/// control, before the dialog opens.
#[tauri::command]
pub(in super::super) async fn explorer_pick_upload(
    window: WebviewWindow,
    state: State<'_, AppState>,
    kind: String,
) -> Result<Option<OfferView>, CommandError> {
    state.files().ok_or_else(no_session)?;
    let dialog = rfd::AsyncFileDialog::new().set_parent(&window);
    let picked: Vec<PathBuf> = match kind.as_str() {
        "files" => dialog
            .set_title("Send files to the customer computer")
            .pick_files()
            .await
            .unwrap_or_default()
            .iter()
            .map(|file| file.path().to_path_buf())
            .collect(),
        "folder" => dialog
            .set_title("Send a folder to the customer computer")
            .pick_folder()
            .await
            .iter()
            .map(|folder| folder.path().to_path_buf())
            .collect(),
        _ => {
            return Err(CommandError::new(
                "refused",
                "files or a folder can be picked",
            ));
        }
    };
    // The session may have ended while the dialog was open.
    state.files().ok_or_else(no_session)?;
    Ok((!picked.is_empty()).then(|| state.transfers.offer(picked)))
}

/// Looks at the folder `folder` of the customer computer and answers which
/// names of the offer `offer` are taken there, for the technician's choice:
/// replace, keep both or skip (ST-M010-002). The look is the technician's,
/// recorded like every listing.
///
/// Fails closed with `no_consented_session` outside a session under full
/// control.
#[tauri::command]
pub(in super::super) async fn explorer_send_check(
    state: State<'_, AppState>,
    offer: u64,
    folder: String,
) -> Result<Check, CommandError> {
    send_check(&state, offer, &folder).await
}

/// Queues the offer `offer` for the folder `folder` of the customer computer
/// (ST-M010-002). `choices` holds, by name, what the technician chose for
/// each name that is taken. The transfers arrive on [`TRANSFERS_EVENT`]; the
/// answer says how many were queued, left out and skipped.
///
/// Fails closed with `no_consented_session` outside a session under full
/// control.
#[tauri::command]
pub(in super::super) async fn explorer_send(
    app: AppHandle,
    state: State<'_, AppState>,
    offer: u64,
    folder: String,
    choices: HashMap<String, Choice>,
) -> Result<Queued, CommandError> {
    // The folder is looked at first if the window did not ask: a send never
    // goes without a look at the names that are taken.
    let state: &AppState = &state;
    state.files().ok_or_else(no_session)?;
    if !state.transfers.checked(offer, &remote_folder(&folder)?) {
        send_check(state, offer, &folder).await?;
    }
    let (queued, entries) = send(state, offer, &folder, &choices)?;
    transfers::deliver(&emitter(&app), entries);
    carry(&app, state);
    Ok(queued)
}

/// Opens the system's dialog for a folder of this computer to copy to, and
/// keeps the folder the technician chose (ST-M010-002). Answers `null` when
/// the dialog was closed without a choice.
///
/// Fails closed with `no_consented_session` outside a session under full
/// control, before the dialog opens.
#[tauri::command]
pub(in super::super) async fn explorer_choose_folder(
    window: WebviewWindow,
    state: State<'_, AppState>,
) -> Result<Option<ChosenFolder>, CommandError> {
    state.files().ok_or_else(no_session)?;
    let chosen = rfd::AsyncFileDialog::new()
        .set_parent(&window)
        .set_title("Copy to this folder")
        .pick_folder()
        .await;
    Ok(chosen.map(|folder| {
        let path = folder.path().to_path_buf();
        ChosenFolder {
            path: path.display().to_string(),
            folder: state.transfers.choose(path),
        }
    }))
}

/// Looks at the folder a copy would go to on this computer — the one chosen
/// under `to`, or `Downloads\CretLink\<target>` — and answers which of the
/// selected names are taken there (ST-M010-002). Nothing is asked of the
/// customer computer, and no folder is made.
///
/// Fails closed with `no_consented_session` outside a session under full
/// control.
#[tauri::command]
pub(in super::super) async fn explorer_download_check(
    app: AppHandle,
    state: State<'_, AppState>,
    names: Vec<Selected>,
    to: Option<u64>,
) -> Result<Check, CommandError> {
    let root = download_root(&state, app.path().download_dir().ok(), to)?;
    Ok(transfers::check_download(&root, &selection(&names)?))
}

/// Queues the entries `names` of the folder `folder` of the customer
/// computer for a copy to this computer: into the folder chosen under `to`,
/// or into `Downloads\CretLink\<target>`, which is made when the first file
/// goes there (ST-M010-002). `choices` holds what the technician chose for
/// each name that is taken there.
///
/// Fails closed with `no_consented_session` outside a session under full
/// control.
#[tauri::command]
pub(in super::super) async fn explorer_download(
    app: AppHandle,
    state: State<'_, AppState>,
    folder: String,
    names: Vec<Selected>,
    to: Option<u64>,
    choices: HashMap<String, Choice>,
) -> Result<Queued, CommandError> {
    let root = download_root(&state, app.path().download_dir().ok(), to)?;
    let (queued, entries) = download(&state, &folder, &names, &root, &choices)?;
    transfers::deliver(&emitter(&app), entries);
    carry(&app, &state);
    Ok(queued)
}

/// The queue of this session as it is: every transfer, in the order it was
/// queued. Nothing is asked of the customer computer.
#[tauri::command]
pub(in super::super) fn explorer_transfers(state: State<'_, AppState>) -> Vec<TransferView> {
    state.transfers.snapshot()
}

/// Cancels the transfer `transfer`: a waiting one at once, a running one as
/// soon as its next step ends; its row then says how it ended.
#[tauri::command]
pub(in super::super) fn explorer_cancel_transfer(
    app: AppHandle,
    state: State<'_, AppState>,
    transfer: u64,
) {
    transfers::deliver(&emitter(&app), state.transfers.cancel(transfer));
}

/// Cancels every transfer that waits or runs.
#[tauri::command]
pub(in super::super) fn explorer_cancel_transfers(app: AppHandle, state: State<'_, AppState>) {
    transfers::deliver(&emitter(&app), state.transfers.cancel_all());
}

/// Shows, in the file manager of this computer, the folder that holds the
/// copy the transfer `transfer` made. Only a copy that arrived has one.
#[tauri::command]
pub(in super::super) fn explorer_show_folder(
    state: State<'_, AppState>,
    transfer: u64,
) -> Result<(), CommandError> {
    let folder = state.transfers.folder_of(transfer).ok_or_else(|| {
        CommandError::new(
            "refused",
            "only the folder of a copy that arrived can be shown",
        )
    })?;
    crate::opener::show_folder(&folder).map_err(|error| {
        CommandError::new(
            "local",
            format!(
                "the folder {} could not be shown: {error}",
                folder.display()
            ),
        )
    })
}

/// Renames the entry `name` of the folder `folder` of the customer computer
/// to `to`, in that folder (ST-M010-002). A name that is taken is refused
/// and nothing changes.
///
/// Fails closed with `no_consented_session` outside a session under full
/// control.
#[tauri::command]
pub(in super::super) async fn explorer_rename(
    state: State<'_, AppState>,
    folder: String,
    name: String,
    to: String,
) -> Result<(), CommandError> {
    rename(&state, &folder, &name, &to).await
}

/// Deletes `items` from the folder `folder` of the customer computer
/// (ST-M010-002): a folder with everything in it, a file or a link as that
/// one entry. `removal` is a number the window chooses, to follow the delete
/// on [`REMOVAL_EVENT`] and to cancel it. The answer says what was removed
/// and what could not be, with the reasons.
///
/// The window asks the technician first: nothing here is moved to a recycle
/// bin.
///
/// Fails closed with `no_consented_session` outside a session under full
/// control.
#[tauri::command]
pub(in super::super) async fn explorer_delete(
    app: AppHandle,
    state: State<'_, AppState>,
    folder: String,
    items: Vec<DeleteItem>,
    removal: u64,
) -> Result<DeleteReport, CommandError> {
    let progress: Arc<dyn Fn(RemovalProgress) + Send + Sync> = Arc::new(move |step| {
        let _ = app.emit(REMOVAL_EVENT, &step);
    });
    delete(&state, &folder, &items, removal, progress).await
}

/// Cancels the delete `removal`: it ends between two items, or between two
/// steps of a folder's removal, and says what it had removed.
#[tauri::command]
pub(in super::super) fn explorer_cancel_delete(state: State<'_, AppState>, removal: u64) {
    state.removals.cancel(removal);
}

#[cfg(test)]
mod tests;
