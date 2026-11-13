//! The transfer queue of the file explorer (ST-M010-002, REQ-F-028).
//!
//! Everything the technician sends to the open folder or copies to this
//! computer becomes entries of one queue, which the Files tab shows as its
//! transfers panel: each file a transfer of its own, with its progress, its
//! state and, when it did not arrive, the reason. Folders go file by file.
//!
//! # What is sent
//!
//! The files and folders of a send never come from the window as paths. The
//! technician drops them on the window or picks them in the system's dialog;
//! this side keeps the paths as an *offer* and the window is told the
//! offer's number and the names ([`OfferView`]). A send names an offer, never
//! a path, so the window cannot make this computer read a file the
//! technician did not choose.
//!
//! # A name that is taken
//!
//! Before anything is queued, the folder it goes to is looked at
//! ([`Transfers::check_send`] lists the folder on the customer computer,
//! [`check_download`] looks at this computer's folder) and the window gets
//! the names that are taken. For each one the technician chooses
//! ([`Choice`]): replace it, keep both under a new name (`name (2).ext`), or
//! skip. A file replaces a file and a folder goes into a folder; a file is
//! never replaced by a folder or the other way round. A name that is taken
//! and has no choice is left out and says so. Nothing is ever replaced
//! without the request: a transfer whose name turns out to be taken after
//! all fails, and what is there stays.
//!
//! # Folders
//!
//! A dropped folder is walked on this computer when it is queued; a folder
//! of the customer computer is walked through its listings when the queue
//! reaches it. Both walks are bounded ([`MAX_BATCH_ENTRIES`],
//! [`MAX_WALK_DEPTH`]), follow no link, and say what they left out: a link, a
//! name that cannot be a name on the other side, and whatever lies beyond a
//! bound are rows of their own, so that a folder never looks complete that is
//! not (REQ-N-008). What a folder of the customer computer holds is read from
//! listings this side asked for itself, never from what the window says a
//! name is.
//!
//! # Names from the customer computer
//!
//! A copy lands at `root\name`, where `name` is a name of a listing. Every
//! such name passes [`local::beside`] first: one plain name, or nothing is
//! written. See [`super::local`].
//!
//! # One at a time
//!
//! [`run`] carries one transfer at a time and the others wait: the session
//! serves one transfer at a time, so more than one "running" here would only
//! be a longer wait behind the same request. A waiting transfer is cancelled
//! at once. A running one is cancelled through its token, and its unfinished
//! part goes with it: the customer computer discards the part of a send (the
//! queue asks afterwards whether it is gone and says so when it is not), and
//! this side removes the part of a copy.
//!
//! # Honest states (REQ-N-008)
//!
//! `done` is a transfer whose length and SHA-256 were verified. A cancelled,
//! failed or stopped one is never `done`. When the session ends, the running
//! transfer and every waiting one end as `stopped`, with the reason.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use cretlink_console::{ProgressSink, SendToFolder};
use cretlink_mcp::{TargetError, TransferFailure};
use cretlink_protocol::{FileEntry, FileKind, JobOutcome};
use cretlink_session::{CancelToken, TransferProgress, activity::error_text, download_part_path};
use serde::{Deserialize, Serialize};

use super::local::{self, LINK_LEFT_OUT};
use super::target::{Ended, FileTarget, failure_words};

/// The most entries one session's queue remembers, finished ones included
/// (REQ-N-006). A send or a copy that would go beyond it is refused whole.
pub(crate) const MAX_QUEUE_ENTRIES: usize = 100_000;

/// The most files and folders one send or one copy queues. A folder that
/// holds more is queued up to here and says that it is not complete.
pub(crate) const MAX_BATCH_ENTRIES: usize = 10_000;

/// How many folders deep a walk goes below the folder that was dropped or
/// selected.
pub(crate) const MAX_WALK_DEPTH: usize = 32;

/// The most offers kept at once: a drop that was never sent is forgotten
/// when this many newer ones came.
const MAX_OFFERS: usize = 8;

/// The most folders of this computer the technician chose that are kept.
const MAX_CHOSEN: usize = 32;

/// The most names of an offer the window is told, for its words.
const OFFER_NAMES: usize = 5;

/// The most views one change of the queue delivers at once.
const EVENT_CHUNK: usize = 500;

/// Which way a transfer goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Direction {
    /// From this computer into a folder of the customer computer.
    Send,
    /// From the customer computer to a folder of this computer.
    Download,
}

/// What an entry of the queue stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EntryKind {
    /// One file: a transfer.
    File,
    /// A folder: made, or walked into its files.
    Folder,
}

/// The state of an entry. Only `Done` is a transfer that arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TransferState {
    /// Not begun: it waits for the transfers before it.
    Waiting,
    /// On its way.
    Running,
    /// Arrived and verified; a folder that was made.
    Done,
    /// Did not arrive, with the reason.
    Failed,
    /// Cancelled by the technician.
    Cancelled,
    /// Not copied at all, with the reason: a link, a name that cannot be
    /// used, a name that is taken without a choice.
    LeftOut,
    /// Ended with the session, before it was complete.
    Stopped,
    /// A folder whose files are queued: the files stand for it from here on,
    /// and the window drops it.
    Listed,
}

impl TransferState {
    fn is_open(self) -> bool {
        matches!(self, Self::Waiting | Self::Running)
    }
}

/// What the technician chose for a name that is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Choice {
    /// A file replaces the file; a folder goes into the folder and its files
    /// replace those of the same name.
    Replace,
    /// It gets a new name beside the one that is there: `name (2).ext`.
    KeepBoth,
    /// It is not copied.
    Skip,
}

/// One entry of the queue as the window is shown it (`TransferEntry` in the
/// frontend contract). Every text may hold names of the customer computer and
/// is written as text by the window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TransferView {
    /// The number of the entry, rising in the order of the queue.
    pub(crate) id: u64,
    pub(crate) direction: Direction,
    pub(crate) kind: EntryKind,
    /// What is copied, as the technician knows it: a name, or a path below
    /// the folder that was dropped or selected.
    pub(crate) name: String,
    /// Where it is on the customer computer.
    pub(crate) remote: String,
    /// Where it is on this computer: the file that is sent, or the place the
    /// copy goes to.
    pub(crate) local: String,
    /// Its length in bytes, once known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) bytes: Option<u64>,
    /// The bytes confirmed so far.
    pub(crate) done: u64,
    pub(crate) state: TransferState,
    /// Why it is in this state, as sentences, when there is something to say.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
}

/// What the technician dropped or picked, as the window is told
/// (`UploadOffer` in the frontend contract): a number to send it by, and
/// names for the words. Never a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OfferView {
    /// The number a send names the offer by.
    pub(crate) offer: u64,
    /// How many files and folders were dropped or picked.
    pub(crate) items: u64,
    /// The names of the first few.
    pub(crate) names: Vec<String>,
}

/// A name that is taken where a transfer would put it (`NameConflict` in the
/// frontend contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Conflict {
    pub(crate) name: String,
    /// What is to be copied.
    pub(crate) kind: EntryKind,
    /// What lies under that name now.
    pub(crate) existing: FileKind,
    /// Whether **Replace** can be chosen: a file for a file, a folder into a
    /// folder.
    pub(crate) replace: bool,
}

/// What a check found (`TransferCheck` in the frontend contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Check {
    /// The folder the transfers go to, as text for the question.
    pub(crate) folder: String,
    /// The names that are taken there, in the order of the selection.
    pub(crate) conflicts: Vec<Conflict>,
}

/// How a send or a copy was queued (`TransferQueued` in the frontend
/// contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Queued {
    /// How many transfers wait now because of it. For a copy of folders this
    /// grows when the queue reaches them.
    pub(crate) queued: u64,
    /// How many items were left out or are not complete, each with a row
    /// that says why.
    pub(crate) left_out: u64,
    /// How many items the technician skipped.
    pub(crate) skipped: u64,
}

/// Why a send or a copy could not be queued, in words for the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refused(pub(crate) String);

/// Where a change of the queue is delivered: the views that changed.
pub(crate) type Emit = Arc<dyn Fn(Vec<TransferView>) + Send + Sync>;

/// The names a folder of the customer computer holds, each by its spelling
/// in small letters, since Windows tells no two names apart by letter case.
#[derive(Debug, Clone, Default)]
struct Names {
    kinds: HashMap<String, FileKind>,
    /// Whether these are all names of the folder.
    complete: bool,
}

impl Names {
    fn of(entries: &[FileEntry], complete: bool) -> Self {
        Self {
            kinds: entries
                .iter()
                .map(|entry| (entry.name.to_lowercase(), entry.kind))
                .collect(),
            complete,
        }
    }

    fn kind(&self, name: &str) -> Option<FileKind> {
        self.kinds.get(&name.to_lowercase()).copied()
    }
}

/// What the open folder holds, by exact name, and whether these are all
/// its entries.
type Selection = (HashMap<String, FileEntry>, bool);

/// What a batch — one send, or one copy — knows while its entries run.
#[derive(Debug, Default)]
struct Batch {
    /// A send: the folders of the customer computer that are there, by
    /// spelling in small letters: `Ok(true)` found there, `Ok(false)` made by
    /// this send, `Err` with why it could not be made.
    folders: HashMap<String, Result<bool, String>>,
    /// A send: what the folders that were there before hold.
    listings: HashMap<String, Names>,
    /// A copy: what the open folder holds, once it was listed; or why it
    /// could not be.
    selection: Option<Result<Selection, String>>,
    /// A copy: how many files its folders have queued.
    walked: usize,
}

/// What the worker does for an entry.
#[derive(Debug, Clone)]
enum Work {
    /// Nothing: the row only says something.
    Note,
    /// Send `source` into `folder` under `name`.
    SendFile {
        source: PathBuf,
        folder: String,
        name: String,
        overwrite: bool,
    },
    /// Make the folder `path` on the customer computer. With `merge` it may
    /// be there already.
    MakeFolder { path: String, merge: bool },
    /// Find out what the selected `name` in `folder` is, and copy it below
    /// `root`.
    Selected {
        folder: String,
        name: String,
        root: PathBuf,
        choice: Option<Choice>,
    },
    /// Copy the file `source` to `dest`, which lies below `root`.
    DownloadFile {
        source: String,
        dest: PathBuf,
        root: PathBuf,
        overwrite: bool,
    },
}

#[derive(Debug)]
struct Entry {
    view: TransferView,
    batch: u64,
    work: Work,
    cancel: CancelToken,
    /// The technician asked to cancel it.
    asked: bool,
    /// A folder a send makes on its way: shown only when it failed.
    quiet: bool,
    /// The progress the window was told last.
    told: u64,
}

impl Entry {
    fn shown(&self) -> bool {
        !self.quiet || self.view.state == TransferState::Failed
    }
}

#[derive(Debug)]
struct Offer {
    id: u64,
    paths: Vec<PathBuf>,
    /// The folder of the customer computer it was checked against, and what
    /// that folder holds.
    checked: Option<(String, Names)>,
}

#[derive(Debug, Default)]
struct Queue {
    /// Rises when a session starts: what an older session queued is gone.
    epoch: u64,
    last_id: u64,
    entries: Vec<Entry>,
    /// The place of each entry by its number.
    places: HashMap<u64, usize>,
    /// Every entry before this place has begun or ended.
    cursor: usize,
    /// A worker carries the queue.
    working: bool,
    offers: Vec<Offer>,
    last_offer: u64,
    chosen: Vec<(u64, PathBuf)>,
    last_chosen: u64,
    batches: HashMap<u64, Batch>,
    last_batch: u64,
}

impl Queue {
    fn entry(&mut self, id: u64) -> Option<&mut Entry> {
        let place = *self.places.get(&id)?;
        self.entries.get_mut(place)
    }

    /// How many entries wait. The one that runs lies just before the cursor.
    fn waiting(&self) -> usize {
        self.entries[self.cursor.min(self.entries.len())..]
            .iter()
            .filter(|entry| entry.view.state == TransferState::Waiting)
            .count()
    }
}

/// The queue of one technician process, shared by its commands and its
/// worker. Cloning shares the same queue.
#[derive(Debug, Clone, Default)]
pub(crate) struct Transfers(Arc<Mutex<Queue>>);

/// An entry to add to the queue.
struct Planned {
    kind: EntryKind,
    name: String,
    remote: String,
    local: String,
    bytes: Option<u64>,
    state: TransferState,
    reason: Option<String>,
    work: Work,
    quiet: bool,
}

impl Planned {
    /// A row that only says why something is not copied.
    fn note(kind: EntryKind, name: String, remote: String, local: String, why: &str) -> Self {
        Self {
            kind,
            name,
            remote,
            local,
            bytes: None,
            state: TransferState::LeftOut,
            reason: Some(why.to_owned()),
            work: Work::Note,
            quiet: false,
        }
    }
}

/// The path of the entry `name` in `folder` on the customer computer.
pub(crate) fn join(folder: &str, name: &str) -> String {
    if folder.ends_with('\\') {
        format!("{folder}{name}")
    } else {
        format!("{folder}\\{name}")
    }
}

/// A reason as a sentence: with a capital first letter and a full stop.
fn sentence(text: &str) -> String {
    let mut letters = text.trim().chars();
    let Some(first) = letters.next() else {
        return String::new();
    };
    let mut out: String = first.to_uppercase().chain(letters).collect();
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}

/// Why a transfer that ran failed, from its honest report.
fn failed_words(error: Option<&TransferFailure>) -> String {
    match error {
        // An error on this computer's own file names the path and the reason.
        Some(failure) if failure.stage == "local" => sentence(&failure.message),
        Some(failure) => sentence(error_text(failure.code)),
        None => "It did not arrive, and the customer computer gave no reason.".to_owned(),
    }
}

/// What kind of entry lies at `path` on this computer, when one does.
fn local_kind(path: &Path) -> Option<FileKind> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    let kind = meta.file_type();
    Some(if kind.is_symlink() {
        FileKind::Symlink
    } else if kind.is_dir() {
        FileKind::Dir
    } else if kind.is_file() {
        FileKind::File
    } else {
        FileKind::Other
    })
}

// What stood in the way of an item whose name is taken. A row says it for
// the way the item goes: [`not_sent`] or [`not_copied`].
const NEEDS_A_CHOICE: &str = "A name that is taken needs your choice: replace, keep both or skip.";
const KINDS_DIFFER: &str = "A file cannot replace a folder, and a folder cannot replace a file.";
const NO_FREE_NAME: &str = "No free name was found to keep both.";

/// `why`, as the reason of an item that was not sent to the customer computer.
fn not_sent(why: &str) -> String {
    format!("{why} It was not sent.")
}

/// `why`, as the reason of an item that was not copied to this computer.
fn not_copied(why: &str) -> String {
    format!("{why} It was not copied.")
}
const NOT_A_FILE_OR_FOLDER: &str = "It is neither a file nor a folder.";
const QUEUE_FULL: &str = "CretLink keeps at most 100,000 transfers of one session in its list. \
     Wait until some have run, or end the session and start a new one.";

impl Transfers {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A new session starts: the queue, the offers and the chosen folders of
    /// the one before are dropped. A worker that still runs finds nothing of
    /// its own and ends.
    pub(crate) fn reset(&self) {
        let mut queue = self.lock();
        let epoch = queue.epoch + 1;
        // The numbers go on, so that no entry of the new session answers to a
        // number of the old one.
        let (last_id, last_offer, last_chosen, last_batch) = (
            queue.last_id,
            queue.last_offer,
            queue.last_chosen,
            queue.last_batch,
        );
        for entry in &queue.entries {
            entry.cancel.cancel();
        }
        *queue = Queue {
            epoch,
            last_id,
            last_offer,
            last_chosen,
            last_batch,
            ..Queue::default()
        };
    }

    /// Keeps what the technician dropped or picked and returns what the
    /// window is told of it.
    pub(crate) fn offer(&self, paths: Vec<PathBuf>) -> OfferView {
        let mut queue = self.lock();
        queue.last_offer += 1;
        let id = queue.last_offer;
        let view = OfferView {
            offer: id,
            items: paths.len() as u64,
            names: paths
                .iter()
                .take(OFFER_NAMES)
                .map(|path| {
                    path.file_name().map_or_else(
                        || path.display().to_string(),
                        |name| name.to_string_lossy().into_owned(),
                    )
                })
                .collect(),
        };
        if queue.offers.len() >= MAX_OFFERS {
            queue.offers.remove(0);
        }
        queue.offers.push(Offer {
            id,
            paths,
            checked: None,
        });
        view
    }

    /// Keeps a folder of this computer the technician chose in the system's
    /// dialog, and returns the number a copy names it by.
    pub(crate) fn choose(&self, folder: PathBuf) -> u64 {
        let mut queue = self.lock();
        queue.last_chosen += 1;
        let id = queue.last_chosen;
        if queue.chosen.len() >= MAX_CHOSEN {
            queue.chosen.remove(0);
        }
        queue.chosen.push((id, folder));
        id
    }

    /// The folder the technician chose under the number `id`.
    pub(crate) fn chosen(&self, id: u64) -> Option<PathBuf> {
        self.lock()
            .chosen
            .iter()
            .find(|(number, _)| *number == id)
            .map(|(_, folder)| folder.clone())
    }

    /// Every entry the window shows, in the order of the queue.
    pub(crate) fn snapshot(&self) -> Vec<TransferView> {
        self.lock()
            .entries
            .iter()
            .filter(|entry| entry.shown() && entry.view.state != TransferState::Listed)
            .map(|entry| entry.view.clone())
            .collect()
    }

    /// The folder on this computer that holds the copy the entry `id` made:
    /// only for a copy that arrived.
    pub(crate) fn folder_of(&self, id: u64) -> Option<PathBuf> {
        let mut queue = self.lock();
        let entry = queue.entry(id)?;
        if entry.view.direction != Direction::Download || entry.view.state != TransferState::Done {
            return None;
        }
        match &entry.work {
            Work::DownloadFile { dest, .. } => dest.parent().map(Path::to_path_buf),
            Work::Selected { root, .. } => Some(root.clone()),
            _ => None,
        }
    }

    /// Cancels the entry `id`: a waiting one is cancelled at once, a running
    /// one through its token (the worker says how it ended). Returns the view
    /// that changed, if one did.
    pub(crate) fn cancel(&self, id: u64) -> Vec<TransferView> {
        let mut queue = self.lock();
        let Some(entry) = queue.entry(id) else {
            return Vec::new();
        };
        Self::cancel_entry(entry).into_iter().collect()
    }

    /// Cancels every transfer that waits or runs.
    pub(crate) fn cancel_all(&self) -> Vec<TransferView> {
        let mut queue = self.lock();
        let from = queue.cursor.min(queue.entries.len());
        // The running one may lie before the cursor.
        let from = from.saturating_sub(1);
        queue.entries[from..]
            .iter_mut()
            .filter_map(Self::cancel_entry)
            .collect()
    }

    fn cancel_entry(entry: &mut Entry) -> Option<TransferView> {
        match entry.view.state {
            TransferState::Waiting => {
                entry.asked = true;
                entry.cancel.cancel();
                entry.view.state = TransferState::Cancelled;
                entry.shown().then(|| entry.view.clone())
            }
            TransferState::Running => {
                entry.asked = true;
                entry.cancel.cancel();
                None
            }
            _ => None,
        }
    }

    /// The session ended: the running transfer and every waiting one are
    /// stopped, with the reason. Returns the views that changed.
    pub(crate) fn stop_all(&self, ended: Ended) -> Vec<TransferView> {
        let mut queue = self.lock();
        let from = queue.cursor.min(queue.entries.len()).saturating_sub(1);
        let mut changed = Vec::new();
        for entry in &mut queue.entries[from..] {
            if entry.view.state.is_open() {
                entry.cancel.cancel();
                entry.view.state = TransferState::Stopped;
                entry.view.reason = Some(ended.words().to_owned());
                if entry.shown() {
                    changed.push(entry.view.clone());
                }
            }
        }
        queue.cursor = queue.entries.len();
        changed
    }

    /// Adds `planned` to the queue as one batch and returns the views to
    /// deliver, or `None` when the queue cannot take that many.
    fn push(
        &self,
        batch: Batch,
        planned: Vec<Planned>,
        direction: Direction,
    ) -> Option<Vec<TransferView>> {
        let mut queue = self.lock();
        if queue.entries.len() + planned.len() > MAX_QUEUE_ENTRIES {
            return None;
        }
        queue.last_batch += 1;
        let batch_id = queue.last_batch;
        queue.batches.insert(batch_id, batch);
        Some(Self::append(&mut queue, batch_id, planned, direction))
    }

    /// Adds `planned` to the batch `batch` of the queue.
    fn append(
        queue: &mut Queue,
        batch: u64,
        planned: Vec<Planned>,
        direction: Direction,
    ) -> Vec<TransferView> {
        let mut views = Vec::new();
        for plan in planned {
            queue.last_id += 1;
            let id = queue.last_id;
            let entry = Entry {
                view: TransferView {
                    id,
                    direction,
                    kind: plan.kind,
                    name: plan.name,
                    remote: plan.remote,
                    local: plan.local,
                    bytes: plan.bytes,
                    done: 0,
                    state: plan.state,
                    reason: plan.reason,
                },
                batch,
                work: plan.work,
                cancel: CancelToken::new(),
                asked: false,
                quiet: plan.quiet,
                told: 0,
            };
            if entry.shown() {
                views.push(entry.view.clone());
            }
            let place = queue.entries.len();
            queue.places.insert(id, place);
            queue.entries.push(entry);
        }
        views
    }

    // --- A send ---

    /// Looks at the folder `folder` of the customer computer and says which
    /// names of the offer `offer` are taken there. What the folder holds is
    /// kept with the offer for [`Self::queue_send`].
    ///
    /// # Errors
    ///
    /// [`CheckError::Refused`] for an offer that is not kept (any more), and
    /// [`CheckError::Target`] with what the customer computer answered, or
    /// with the end of the session.
    pub(crate) async fn check_send<T: FileTarget>(
        &self,
        target: &T,
        offer: u64,
        folder: &str,
    ) -> Result<Check, CheckError> {
        let paths = self
            .lock()
            .offers
            .iter()
            .find(|kept| kept.id == offer)
            .map(|kept| kept.paths.clone())
            .ok_or_else(|| CheckError::Refused(Refused(OFFER_GONE.to_owned())))?;
        let listed = target
            .list(folder, &CancelToken::new())
            .await
            .map_err(CheckError::Target)?;
        let names = Names::of(&listed.entries, listed.complete);
        let conflicts = paths
            .iter()
            .filter_map(|path| {
                let name = path.file_name()?.to_str()?;
                let existing = names.kind(name)?;
                let kind = match local_kind(path)? {
                    FileKind::Dir => EntryKind::Folder,
                    FileKind::File => EntryKind::File,
                    // Left out when it is queued; nothing to ask.
                    FileKind::Symlink | FileKind::Other => return None,
                };
                Some(Conflict {
                    name: name.to_owned(),
                    kind,
                    existing,
                    replace: can_replace(kind, existing),
                })
            })
            .collect();
        if let Some(kept) = self.lock().offers.iter_mut().find(|kept| kept.id == offer) {
            kept.checked = Some((folder.to_owned(), names));
        }
        Ok(Check {
            folder: folder.to_owned(),
            conflicts,
        })
    }

    /// Whether the offer `offer` was checked against the folder `folder`.
    pub(crate) fn checked(&self, offer: u64, folder: &str) -> bool {
        self.lock().offers.iter().any(|kept| {
            kept.id == offer
                && kept
                    .checked
                    .as_ref()
                    .is_some_and(|(checked, _)| checked == folder)
        })
    }

    /// Queues the offer `offer` for the folder `folder` of the customer
    /// computer, with the technician's `choices` for the names that are
    /// taken. The offer must have been checked against that folder
    /// ([`Self::check_send`]); it is used up.
    ///
    /// Returns what was queued and the views to deliver.
    ///
    /// # Errors
    ///
    /// [`Refused`] for an offer that is not kept or was not checked against
    /// this folder, and for a queue that cannot take it.
    pub(crate) fn queue_send(
        &self,
        offer: u64,
        folder: &str,
        choices: &HashMap<String, Choice>,
    ) -> Result<(Queued, Vec<TransferView>), Refused> {
        let (paths, names) = {
            let mut queue = self.lock();
            let place = queue
                .offers
                .iter()
                .position(|kept| kept.id == offer)
                .ok_or_else(|| Refused(OFFER_GONE.to_owned()))?;
            match &queue.offers[place].checked {
                Some((checked, _)) if checked == folder => {}
                _ => {
                    return Err(Refused(
                        "This folder was not looked at for names that are taken. Drop the \
                         files again."
                            .to_owned(),
                    ));
                }
            }
            let kept = queue.offers.remove(place);
            let names = kept.checked.map(|(_, names)| names).unwrap_or_default();
            (kept.paths, names)
        };

        let mut planned = Vec::new();
        let mut queued = Queued::default();
        let mut budget = MAX_BATCH_ENTRIES;
        // The names this send gives its own items, so that no two share one.
        let mut used: Vec<String> = Vec::new();
        for path in &paths {
            let local = path.display().to_string();
            let shown = path
                .file_name()
                .map_or_else(|| local.clone(), |name| name.to_string_lossy().into_owned());
            let leave = |kind: EntryKind, why: &str, planned: &mut Vec<Planned>| {
                planned.push(Planned::note(
                    kind,
                    shown.clone(),
                    join(folder, &shown),
                    local.clone(),
                    why,
                ));
            };
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                leave(
                    EntryKind::File,
                    "Its name is not text that can be sent.",
                    &mut planned,
                );
                queued.left_out += 1;
                continue;
            };
            let kind = match local_kind(path) {
                Some(FileKind::File) => EntryKind::File,
                Some(FileKind::Dir) => EntryKind::Folder,
                Some(FileKind::Symlink) => {
                    leave(EntryKind::File, LINK_LEFT_OUT, &mut planned);
                    queued.left_out += 1;
                    continue;
                }
                Some(FileKind::Other) => {
                    leave(EntryKind::File, NOT_A_FILE_OR_FOLDER, &mut planned);
                    queued.left_out += 1;
                    continue;
                }
                None => {
                    leave(
                        EntryKind::File,
                        "It could not be read on this computer.",
                        &mut planned,
                    );
                    queued.left_out += 1;
                    continue;
                }
            };
            if used.contains(&name.to_lowercase()) {
                leave(
                    kind,
                    &not_sent("Another item of this drop has the same name."),
                    &mut planned,
                );
                queued.left_out += 1;
                continue;
            }
            // What the technician chose when the name is taken.
            let (name, overwrite) = match names.kind(name) {
                None => (name.to_owned(), false),
                Some(existing) => match choices.get(name) {
                    None => {
                        leave(kind, &not_sent(NEEDS_A_CHOICE), &mut planned);
                        queued.left_out += 1;
                        continue;
                    }
                    Some(Choice::Skip) => {
                        queued.skipped += 1;
                        continue;
                    }
                    Some(Choice::Replace) if can_replace(kind, existing) => (name.to_owned(), true),
                    Some(Choice::Replace) => {
                        leave(kind, &not_sent(KINDS_DIFFER), &mut planned);
                        queued.left_out += 1;
                        continue;
                    }
                    Some(Choice::KeepBoth) => {
                        let taken = |candidate: &str| {
                            names.kind(candidate).is_some()
                                || used.contains(&candidate.to_lowercase())
                        };
                        match local::kept_beside(name, kind == EntryKind::Folder, &taken) {
                            Some(free) => (free, false),
                            None => {
                                leave(kind, &not_sent(NO_FREE_NAME), &mut planned);
                                queued.left_out += 1;
                                continue;
                            }
                        }
                    }
                },
            };
            used.push(name.to_lowercase());
            if budget == 0 {
                leave(kind, MORE_THAN_ONE_DROP, &mut planned);
                queued.left_out += 1;
                continue;
            }
            budget -= 1;
            match kind {
                EntryKind::File => {
                    let bytes = std::fs::metadata(path).ok().map(|meta| meta.len());
                    planned.push(Planned {
                        kind,
                        name: name.clone(),
                        remote: join(folder, &name),
                        local,
                        bytes,
                        state: TransferState::Waiting,
                        reason: None,
                        work: Work::SendFile {
                            source: path.clone(),
                            folder: folder.to_owned(),
                            name,
                            overwrite,
                        },
                        quiet: false,
                    });
                    queued.queued += 1;
                }
                EntryKind::Folder => {
                    let top = join(folder, &name);
                    let walked = local::walk(path, &mut budget, MAX_WALK_DEPTH);
                    // The folder itself is a row only when no file stands for
                    // it: an empty folder is made and says so.
                    planned.push(Planned {
                        kind,
                        name: name.clone(),
                        remote: top.clone(),
                        local: local.clone(),
                        bytes: None,
                        state: TransferState::Waiting,
                        reason: None,
                        work: Work::MakeFolder {
                            path: top.clone(),
                            merge: overwrite,
                        },
                        quiet: !walked.files.is_empty(),
                    });
                    if walked.files.is_empty() {
                        queued.queued += 1;
                    }
                    for below in &walked.folders {
                        let remote = below.iter().fold(top.clone(), |at, part| join(&at, part));
                        planned.push(Planned {
                            kind: EntryKind::Folder,
                            name: format!("{name}\\{}", below.join("\\")),
                            remote: remote.clone(),
                            local: local.clone(),
                            bytes: None,
                            state: TransferState::Waiting,
                            reason: None,
                            work: Work::MakeFolder {
                                path: remote,
                                merge: overwrite,
                            },
                            quiet: true,
                        });
                    }
                    for (parts, len, source) in &walked.files {
                        let Some((file, above)) = parts.split_last() else {
                            continue;
                        };
                        let into = above.iter().fold(top.clone(), |at, part| join(&at, part));
                        planned.push(Planned {
                            kind: EntryKind::File,
                            name: format!("{name}\\{}", parts.join("\\")),
                            remote: join(&into, file),
                            local: source.display().to_string(),
                            bytes: Some(*len),
                            state: TransferState::Waiting,
                            reason: None,
                            work: Work::SendFile {
                                source: source.clone(),
                                folder: into,
                                name: file.clone(),
                                overwrite,
                            },
                            quiet: false,
                        });
                        queued.queued += 1;
                    }
                    for out in &walked.left_out {
                        planned.push(Planned::note(
                            EntryKind::File,
                            format!("{name}\\{}", out.name),
                            top.clone(),
                            local.clone(),
                            out.why,
                        ));
                        queued.left_out += 1;
                    }
                    if let Some(why) = &walked.stopped {
                        planned.push(Planned {
                            kind: EntryKind::Folder,
                            name: name.clone(),
                            remote: top.clone(),
                            local: local.clone(),
                            bytes: None,
                            state: TransferState::Failed,
                            reason: Some(format!("Not all of this folder was queued. {why}")),
                            work: Work::Note,
                            quiet: false,
                        });
                        queued.left_out += 1;
                    }
                }
            }
        }

        let mut batch = Batch::default();
        batch.folders.insert(folder.to_lowercase(), Ok(true));
        batch.listings.insert(folder.to_lowercase(), names);
        let views = self
            .push(batch, planned, Direction::Send)
            .ok_or_else(|| Refused(QUEUE_FULL.to_owned()))?;
        Ok((queued, views))
    }

    // --- A copy to this computer ---

    /// Queues the entries `names` of the folder `folder` of the customer
    /// computer for a copy into `root` on this computer, with the
    /// technician's `choices` for the names that are taken there.
    ///
    /// What each name is — a file, a folder, a link — is found out from a
    /// listing when the queue reaches it; `kinds` only says how the window
    /// showed it.
    ///
    /// # Errors
    ///
    /// [`Refused`] for a queue that cannot take the copy.
    pub(crate) fn queue_download(
        &self,
        folder: &str,
        names: &[(String, EntryKind)],
        root: &Path,
        choices: &HashMap<String, Choice>,
    ) -> Result<(Queued, Vec<TransferView>), Refused> {
        let mut planned = Vec::new();
        let mut queued = Queued::default();
        for (name, kind) in names.iter().take(MAX_BATCH_ENTRIES) {
            let remote = join(folder, name);
            let dest = match local::beside(root, name) {
                Ok(dest) => dest,
                Err(why) => {
                    planned.push(Planned::note(
                        *kind,
                        name.clone(),
                        remote,
                        root.display().to_string(),
                        &unusable(why),
                    ));
                    queued.left_out += 1;
                    continue;
                }
            };
            let choice = match (local_kind(&dest), choices.get(name)) {
                (None, _) => None,
                (Some(_), None) => {
                    planned.push(Planned::note(
                        *kind,
                        name.clone(),
                        remote,
                        dest.display().to_string(),
                        &not_copied(NEEDS_A_CHOICE),
                    ));
                    queued.left_out += 1;
                    continue;
                }
                (Some(_), Some(Choice::Skip)) => {
                    queued.skipped += 1;
                    continue;
                }
                (Some(_), Some(choice)) => Some(*choice),
            };
            planned.push(Planned {
                kind: *kind,
                name: name.clone(),
                remote,
                local: dest.display().to_string(),
                bytes: None,
                state: TransferState::Waiting,
                reason: None,
                work: Work::Selected {
                    folder: folder.to_owned(),
                    name: name.clone(),
                    root: root.to_path_buf(),
                    choice,
                },
                quiet: false,
            });
            queued.queued += 1;
        }
        if names.len() > MAX_BATCH_ENTRIES {
            planned.push(Planned {
                kind: EntryKind::Folder,
                name: folder.to_owned(),
                remote: folder.to_owned(),
                local: root.display().to_string(),
                bytes: None,
                state: TransferState::Failed,
                reason: Some(format!(
                    "Only the first {MAX_BATCH_ENTRIES} of the {} selected items were queued. \
                     Select the others and copy them next.",
                    names.len()
                )),
                work: Work::Note,
                quiet: false,
            });
            queued.left_out += 1;
        }
        let views = self
            .push(Batch::default(), planned, Direction::Download)
            .ok_or_else(|| Refused(QUEUE_FULL.to_owned()))?;
        Ok((queued, views))
    }

    // --- The worker's side ---

    /// Takes the next waiting entry and marks it running, or says that
    /// nothing waits: the worker then ends, under the same lock that a
    /// command takes to see whether it must start one.
    fn next(&self, epoch: u64) -> Option<(u64, u64, Work, CancelToken, TransferView)> {
        let mut guard = self.lock();
        let queue = &mut *guard;
        if queue.epoch != epoch {
            return None;
        }
        while let Some(entry) = queue.entries.get_mut(queue.cursor) {
            if entry.view.state == TransferState::Waiting {
                entry.view.state = TransferState::Running;
                let taken = (
                    entry.view.id,
                    entry.batch,
                    entry.work.clone(),
                    entry.cancel.clone(),
                    entry.view.clone(),
                );
                queue.cursor += 1;
                return Some(taken);
            }
            queue.cursor += 1;
        }
        queue.working = false;
        None
    }

    /// Whether a worker must be started: `true` once, when entries wait and
    /// none carries them.
    pub(crate) fn must_start(&self) -> Option<u64> {
        let mut queue = self.lock();
        if queue.working || queue.waiting() == 0 {
            return None;
        }
        queue.working = true;
        Some(queue.epoch)
    }

    /// Changes the entry `id` and returns its view when the window shows it.
    fn update(&self, id: u64, change: impl FnOnce(&mut Entry)) -> Vec<TransferView> {
        let mut queue = self.lock();
        let Some(entry) = queue.entry(id) else {
            return Vec::new();
        };
        let before = entry.shown();
        change(entry);
        if entry.shown() || before {
            vec![entry.view.clone()]
        } else {
            Vec::new()
        }
    }

    /// Whether the technician asked to cancel the entry `id`.
    fn asked(&self, id: u64) -> bool {
        self.lock().entry(id).is_some_and(|entry| entry.asked)
    }

    /// The progress of the running transfer `id`. Returns its view when the
    /// window is to be told: at the end, and each time a fiftieth of the
    /// file, or a quarter of a megabyte if that is more, was added.
    fn progress(&self, id: u64, step: TransferProgress) -> Option<TransferView> {
        let mut queue = self.lock();
        let entry = queue.entry(id)?;
        if entry.view.state != TransferState::Running {
            return None;
        }
        entry.view.bytes = Some(step.total);
        entry.view.done = step.transferred;
        let stride = (step.total / 50).max(256 * 1024);
        let tell = step.transferred >= step.total || step.transferred >= entry.told + stride;
        tell.then(|| {
            entry.told = step.transferred;
            entry.view.clone()
        })
    }

    fn with_batch<R>(&self, batch: u64, read: impl FnOnce(&mut Batch) -> R) -> Option<R> {
        self.lock().batches.get_mut(&batch).map(read)
    }
}

const OFFER_GONE: &str = "These files are no longer kept for sending. Drop or choose them again.";
const MORE_THAN_ONE_DROP: &str =
    "One drop queues at most 10,000 files and folders. This one was not queued.";

/// Why a check gave no answer.
#[derive(Debug)]
pub(crate) enum CheckError {
    /// Refused on this computer, in words.
    Refused(Refused),
    /// What the customer computer answered, or the end of the session.
    Target(TargetError),
}

fn can_replace(kind: EntryKind, existing: FileKind) -> bool {
    matches!(
        (kind, existing),
        (EntryKind::File, FileKind::File) | (EntryKind::Folder, FileKind::Dir)
    )
}

/// Why a name of the customer computer cannot be a name on this computer.
fn unusable(why: &str) -> String {
    format!("Its name {why}, so it cannot be a name on this computer. It was not copied.")
}

/// Looks at the folder `root` on this computer and says which of `names` are
/// taken there. A name that cannot be a name on this computer is no conflict:
/// it is left out when the copy is queued.
pub(crate) fn check_download(root: &Path, names: &[(String, EntryKind)]) -> Check {
    Check {
        folder: root.display().to_string(),
        conflicts: names
            .iter()
            .filter_map(|(name, kind)| {
                let existing = local_kind(&local::beside(root, name).ok()?)?;
                Some(Conflict {
                    name: name.clone(),
                    kind: *kind,
                    existing,
                    replace: can_replace(*kind, existing),
                })
            })
            .collect(),
    }
}

/// Delivers views in pieces, so that a folder of thousands of files is no
/// single event of thousands of rows.
pub(crate) fn deliver(emit: &Emit, views: Vec<TransferView>) {
    let mut views = views;
    while views.len() > EVENT_CHUNK {
        let rest = views.split_off(EVENT_CHUNK);
        emit(views);
        views = rest;
    }
    if !views.is_empty() {
        emit(views);
    }
}

/// How one entry ended, for the worker's loop.
enum Step {
    /// Go on with the next entry.
    Next,
    /// The session ended: nothing more runs.
    Ended(Ended),
}

/// Carries the queue: one entry after the other, until nothing waits or the
/// session ends. `live` gives the customer computer of the live session, or
/// nothing once no session is under full control. Every change is delivered
/// to `emit`.
///
/// One worker runs at a time ([`Transfers::must_start`] says when to start
/// one); it ends by itself, and a later send or copy starts the next.
pub(crate) async fn run<T: FileTarget>(
    transfers: Transfers,
    epoch: u64,
    live: impl Fn() -> Option<T> + Send,
    emit: Emit,
) {
    while let Some((id, batch, work, cancel, view)) = transfers.next(epoch) {
        deliver(&emit, if_shown(&transfers, id, view));
        let Some(target) = live() else {
            deliver(&emit, transfers.stop_all(Ended::Stopped));
            break;
        };
        let step = match work {
            Work::Note => Step::Next,
            Work::SendFile {
                source,
                folder,
                name,
                overwrite,
            } => {
                send_file(
                    &transfers, &target, &emit, id, batch, cancel, source, folder, name, overwrite,
                )
                .await
            }
            Work::MakeFolder { path, merge } => {
                make_folder(&transfers, &target, &emit, id, batch, &cancel, &path, merge).await
            }
            Work::Selected {
                folder,
                name,
                root,
                choice,
            } => {
                selected(
                    &transfers, &target, &emit, id, batch, cancel, &folder, &name, &root, choice,
                )
                .await
            }
            Work::DownloadFile {
                source,
                dest,
                root,
                overwrite,
            } => {
                download_file(
                    &transfers, &target, &emit, id, cancel, &source, dest, &root, overwrite,
                )
                .await
            }
        };
        if let Step::Ended(ended) = step {
            deliver(&emit, transfers.stop_all(ended));
            break;
        }
    }
    // A worker that stopped on its way no longer carries the queue.
    let mut queue = transfers.lock();
    if queue.epoch == epoch {
        queue.working = false;
    }
}

/// The view of the entry `id` that just began, when the window shows it.
fn if_shown(transfers: &Transfers, id: u64, view: TransferView) -> Vec<TransferView> {
    let shown = transfers
        .lock()
        .entry(id)
        .is_some_and(|entry| entry.shown());
    if shown { vec![view] } else { Vec::new() }
}

/// Ends the entry `id` in `state`, with `reason`.
fn end(transfers: &Transfers, emit: &Emit, id: u64, state: TransferState, reason: Option<String>) {
    deliver(
        emit,
        transfers.update(id, |entry| {
            entry.view.state = state;
            entry.view.reason = reason;
        }),
    );
}

/// Ends the entry `id` after a request that failed: stopped with the session
/// when that is what the failure says, else failed with its words.
fn end_failed(transfers: &Transfers, emit: &Emit, id: u64, error: &TargetError) -> Step {
    match Ended::of(error) {
        Some(ended) => {
            end(
                transfers,
                emit,
                id,
                TransferState::Stopped,
                Some(ended.words().to_owned()),
            );
            Step::Ended(ended)
        }
        None => {
            end(
                transfers,
                emit,
                id,
                TransferState::Failed,
                Some(sentence(&failure_words(error))),
            );
            Step::Next
        }
    }
}

/// The sink that follows the transfer `id`.
fn follow(transfers: &Transfers, emit: &Emit, id: u64) -> ProgressSink<TransferProgress> {
    let (transfers, emit) = (transfers.clone(), Arc::clone(emit));
    Box::new(move |step| {
        if let Some(view) = transfers.progress(id, step) {
            emit(vec![view]);
        }
    })
}

#[allow(clippy::too_many_arguments)]
async fn send_file<T: FileTarget>(
    transfers: &Transfers,
    target: &T,
    emit: &Emit,
    id: u64,
    batch: u64,
    cancel: CancelToken,
    source: PathBuf,
    folder: String,
    name: String,
    overwrite: bool,
) -> Step {
    // The folder it goes into was to be made by this send.
    let made = transfers
        .with_batch(batch, |batch| {
            batch.folders.get(&folder.to_lowercase()).cloned()
        })
        .flatten();
    match made {
        Some(Err(why)) => {
            end(
                transfers,
                emit,
                id,
                TransferState::Failed,
                Some(format!("Its folder {folder} could not be made. {why}")),
            );
            return Step::Next;
        }
        None => {
            end(
                transfers,
                emit,
                id,
                TransferState::Failed,
                Some(format!("Its folder {folder} was not made.")),
            );
            return Step::Next;
        }
        Some(Ok(_)) => {}
    }
    let dest = join(&folder, &name);
    let send = SendToFolder {
        local_path: source,
        folder,
        name,
        overwrite,
    };
    let report = match target.send(send, cancel, follow(transfers, emit, id)).await {
        Ok(report) => report,
        Err(error) => return end_failed(transfers, emit, id, &error),
    };
    match report.outcome {
        JobOutcome::Completed => {
            deliver(
                emit,
                transfers.update(id, |entry| {
                    entry.view.state = TransferState::Done;
                    entry.view.bytes = Some(report.len);
                    entry.view.done = report.len;
                    entry.view.reason = None;
                }),
            );
            Step::Next
        }
        JobOutcome::Cancelled if transfers.asked(id) => {
            // The customer computer was asked to discard the part. Whether it
            // is gone is looked at, and said when it is not.
            let reason = match target.part_left(&dest).await {
                Ok(false) => None,
                Ok(true) => Some(
                    "Its unfinished part is still in the folder: the customer computer could \
                     not remove it now. CretLink removes it when the session ends, and the end \
                     report names it if that fails."
                        .to_owned(),
                ),
                Err(error) => {
                    if let Some(ended) = Ended::of(&error) {
                        end(
                            transfers,
                            emit,
                            id,
                            TransferState::Cancelled,
                            Some(
                                "Whether its unfinished part is gone could not be looked at: \
                                 the session ended."
                                    .to_owned(),
                            ),
                        );
                        return Step::Ended(ended);
                    }
                    Some(format!(
                        "Whether its unfinished part is gone could not be looked at: {}.",
                        failure_words(&error)
                    ))
                }
            };
            end(transfers, emit, id, TransferState::Cancelled, reason);
            Step::Next
        }
        // Cancelled, and not by the technician: a stop or a revocation
        // cancels the transfer that runs.
        JobOutcome::Cancelled => {
            end(
                transfers,
                emit,
                id,
                TransferState::Stopped,
                Some(Ended::Stopped.words().to_owned()),
            );
            Step::Ended(Ended::Stopped)
        }
        JobOutcome::Failed => {
            end(
                transfers,
                emit,
                id,
                TransferState::Failed,
                Some(failed_words(report.error.as_ref())),
            );
            Step::Next
        }
        JobOutcome::TimedOut | JobOutcome::Unknown => {
            end(
                transfers,
                emit,
                id,
                TransferState::Failed,
                Some("Whether it arrived is not known.".to_owned()),
            );
            Step::Next
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn make_folder<T: FileTarget>(
    transfers: &Transfers,
    target: &T,
    emit: &Emit,
    id: u64,
    batch: u64,
    cancel: &CancelToken,
    path: &str,
    merge: bool,
) -> Step {
    let key = path.to_lowercase();
    let (above, name) = match path.trim_end_matches('\\').rsplit_once('\\') {
        Some((above, name)) if !above.contains('\\') => (format!("{above}\\"), name.to_owned()),
        Some((above, name)) => (above.to_owned(), name.to_owned()),
        None => (String::new(), path.to_owned()),
    };
    let fail = |why: String| {
        transfers.with_batch(batch, |batch| {
            batch.folders.insert(key.clone(), Err(why.clone()));
        });
        end(transfers, emit, id, TransferState::Failed, Some(why));
    };
    let parent = transfers
        .with_batch(batch, |batch| {
            batch.folders.get(&above.to_lowercase()).cloned()
        })
        .flatten();
    let existed = match parent {
        Some(Ok(existed)) => existed,
        Some(Err(_)) | None => {
            fail(format!("The folder above it, {above}, was not made."));
            return Step::Next;
        }
    };
    // A folder that was there before may hold this name already: its listing
    // says so. A folder this send made is empty.
    if existed {
        let known = transfers
            .with_batch(batch, |batch| {
                batch
                    .listings
                    .get(&above.to_lowercase())
                    .map(|names| (names.kind(&name), names.complete))
            })
            .flatten();
        let (kind, _complete) = match known {
            Some(known) => known,
            None => match target.list(&above, cancel).await {
                Ok(listed) => {
                    let names = Names::of(&listed.entries, listed.complete);
                    let known = (names.kind(&name), names.complete);
                    transfers.with_batch(batch, |batch| {
                        batch.listings.insert(above.to_lowercase(), names);
                    });
                    known
                }
                Err(error) => {
                    if let Some(ended) = Ended::of(&error) {
                        end(
                            transfers,
                            emit,
                            id,
                            TransferState::Stopped,
                            Some(ended.words().to_owned()),
                        );
                        return Step::Ended(ended);
                    }
                    fail(format!(
                        "The folder above it, {above}, could not be listed: {}.",
                        failure_words(&error)
                    ));
                    return Step::Next;
                }
            },
        };
        match kind {
            Some(FileKind::Dir) if merge => {
                transfers.with_batch(batch, |batch| {
                    batch.folders.insert(key.clone(), Ok(true));
                });
                end(transfers, emit, id, TransferState::Done, None);
                return Step::Next;
            }
            Some(FileKind::Dir) => {
                fail(
                    "A folder of that name is already there. Nothing was sent into it.".to_owned(),
                );
                return Step::Next;
            }
            Some(_) => {
                fail("A file of that name is in the way. The folder was not made.".to_owned());
                return Step::Next;
            }
            None => {}
        }
    }
    match target.create_folder(path).await {
        Ok(()) => {
            transfers.with_batch(batch, |batch| {
                batch.folders.insert(key.clone(), Ok(false));
            });
            end(transfers, emit, id, TransferState::Done, None);
            Step::Next
        }
        Err(error) => {
            if let Some(ended) = Ended::of(&error) {
                end(
                    transfers,
                    emit,
                    id,
                    TransferState::Stopped,
                    Some(ended.words().to_owned()),
                );
                return Step::Ended(ended);
            }
            fail(sentence(&failure_words(&error)));
            Step::Next
        }
    }
}

/// Removes the part a copy to `dest` left on this computer. `None` when it
/// is gone, else why it is not.
async fn remove_part(dest: &Path) -> Option<String> {
    let part = download_part_path(dest);
    match tokio::fs::remove_file(&part).await {
        Ok(()) => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => Some(format!(
            "Its unfinished part {} could not be removed: {error}.",
            part.display()
        )),
    }
}

#[allow(clippy::too_many_arguments)]
async fn download_file<T: FileTarget>(
    transfers: &Transfers,
    target: &T,
    emit: &Emit,
    id: u64,
    cancel: CancelToken,
    source: &str,
    dest: PathBuf,
    root: &Path,
    overwrite: bool,
) -> Step {
    // Whatever led here: nothing is written outside the folder the
    // technician chose.
    if !local::stays_inside(root, &dest) {
        end(
            transfers,
            emit,
            id,
            TransferState::Failed,
            Some("It would not land in the folder that was chosen. It was not copied.".to_owned()),
        );
        return Step::Next;
    }
    if !overwrite && local_kind(&dest).is_some() {
        end(
            transfers,
            emit,
            id,
            TransferState::Failed,
            Some(format!(
                "{} is already there. It was not replaced.",
                dest.display()
            )),
        );
        return Step::Next;
    }
    if let Some(folder) = dest.parent()
        && let Err(error) = tokio::fs::create_dir_all(folder).await
    {
        end(
            transfers,
            emit,
            id,
            TransferState::Failed,
            Some(format!(
                "The folder {} could not be made on this computer: {error}.",
                folder.display()
            )),
        );
        return Step::Next;
    }
    deliver(
        emit,
        transfers.update(id, |entry| {
            entry.view.local = dest.display().to_string();
        }),
    );
    let report = match target
        .download(source, dest.clone(), cancel, follow(transfers, emit, id))
        .await
    {
        Ok(report) => report,
        Err(error) => {
            // The session no longer carries it; no part of it is kept.
            let left = remove_part(&dest).await;
            return match Ended::of(&error) {
                Some(ended) => {
                    let mut reason = ended.words().to_owned();
                    if let Some(left) = left {
                        reason = format!("{reason} {left}");
                    }
                    end(transfers, emit, id, TransferState::Stopped, Some(reason));
                    Step::Ended(ended)
                }
                None => {
                    let mut reason = sentence(&failure_words(&error));
                    if let Some(left) = left {
                        reason = format!("{reason} {left}");
                    }
                    end(transfers, emit, id, TransferState::Failed, Some(reason));
                    Step::Next
                }
            };
        }
    };
    match report.outcome {
        JobOutcome::Completed => {
            deliver(
                emit,
                transfers.update(id, |entry| {
                    entry.view.state = TransferState::Done;
                    entry.view.bytes = Some(report.len);
                    entry.view.done = report.len;
                    entry.view.reason = None;
                }),
            );
            Step::Next
        }
        JobOutcome::Cancelled => {
            // The engine keeps the part of a cancelled copy for a resume; the
            // explorer does not resume, so the part goes.
            let left = remove_part(&dest).await;
            if transfers.asked(id) {
                end(transfers, emit, id, TransferState::Cancelled, left);
                Step::Next
            } else {
                let mut reason = Ended::Stopped.words().to_owned();
                if let Some(left) = left {
                    reason = format!("{reason} {left}");
                }
                end(transfers, emit, id, TransferState::Stopped, Some(reason));
                Step::Ended(Ended::Stopped)
            }
        }
        JobOutcome::Failed => {
            end(
                transfers,
                emit,
                id,
                TransferState::Failed,
                Some(failed_words(report.error.as_ref())),
            );
            Step::Next
        }
        JobOutcome::TimedOut | JobOutcome::Unknown => {
            end(
                transfers,
                emit,
                id,
                TransferState::Failed,
                Some("Whether it arrived is not known.".to_owned()),
            );
            Step::Next
        }
    }
}

/// The selected entry `name` of `folder`: what it is comes from a listing of
/// the folder that this side asks for, once for the whole selection.
#[allow(clippy::too_many_arguments)]
async fn selected<T: FileTarget>(
    transfers: &Transfers,
    target: &T,
    emit: &Emit,
    id: u64,
    batch: u64,
    cancel: CancelToken,
    folder: &str,
    name: &str,
    root: &Path,
    choice: Option<Choice>,
) -> Step {
    let known = transfers
        .with_batch(batch, |batch| batch.selection.clone())
        .flatten();
    let listing = match known {
        Some(listing) => listing,
        None => {
            let listing = match target.list(folder, &cancel).await {
                Ok(listed) => Ok((
                    listed
                        .entries
                        .into_iter()
                        .map(|entry| (entry.name.clone(), entry))
                        .collect::<HashMap<_, _>>(),
                    listed.complete,
                )),
                Err(error) => {
                    if let Some(ended) = Ended::of(&error) {
                        end(
                            transfers,
                            emit,
                            id,
                            TransferState::Stopped,
                            Some(ended.words().to_owned()),
                        );
                        return Step::Ended(ended);
                    }
                    Err(format!(
                        "{folder} could not be listed: {}.",
                        failure_words(&error)
                    ))
                }
            };
            transfers.with_batch(batch, |batch| batch.selection = Some(listing.clone()));
            listing
        }
    };
    let (entries, complete) = match listing {
        Ok(listing) => listing,
        Err(why) => {
            end(transfers, emit, id, TransferState::Failed, Some(why));
            return Step::Next;
        }
    };
    if cancel.is_cancelled() {
        end(transfers, emit, id, TransferState::Cancelled, None);
        return Step::Next;
    }
    let Some(entry) = entries.get(name) else {
        let why = if complete {
            "It is no longer in the folder."
        } else {
            "It was not found among the entries the explorer lists of this folder, which \
             holds more."
        };
        end(
            transfers,
            emit,
            id,
            TransferState::Failed,
            Some(why.to_owned()),
        );
        return Step::Next;
    };
    let is_folder = match entry.kind {
        FileKind::File => false,
        FileKind::Dir => true,
        FileKind::Symlink => {
            end(
                transfers,
                emit,
                id,
                TransferState::LeftOut,
                Some(LINK_LEFT_OUT.to_owned()),
            );
            return Step::Next;
        }
        FileKind::Other => {
            end(
                transfers,
                emit,
                id,
                TransferState::LeftOut,
                Some(NOT_A_FILE_OR_FOLDER.to_owned()),
            );
            return Step::Next;
        }
    };
    // Where it goes on this computer: under its own name, or under a free
    // one beside what is there.
    let local = match choice {
        Some(Choice::KeepBoth) => {
            let taken = |candidate: &str| local_kind(&root.join(candidate)).is_some();
            local::kept_beside(name, is_folder, &taken)
        }
        _ => Some(name.to_owned()),
    };
    let dest = local
        .as_deref()
        .ok_or_else(|| not_copied(NO_FREE_NAME))
        .and_then(|local| local::beside(root, local).map_err(unusable));
    let dest = match dest {
        Ok(dest) => dest,
        Err(why) => {
            end(transfers, emit, id, TransferState::LeftOut, Some(why));
            return Step::Next;
        }
    };
    let merge = choice == Some(Choice::Replace);
    let source = join(folder, name);
    if !is_folder {
        // A file replaces a file only, whatever was chosen and whatever lay
        // there when the technician was asked.
        if merge && !matches!(local_kind(&dest), None | Some(FileKind::File)) {
            end(
                transfers,
                emit,
                id,
                TransferState::Failed,
                Some(not_copied(KINDS_DIFFER)),
            );
            return Step::Next;
        }
        deliver(
            emit,
            transfers.update(id, |entry| {
                entry.view.kind = EntryKind::File;
                entry.work = Work::DownloadFile {
                    source: source.clone(),
                    dest: dest.clone(),
                    root: root.to_path_buf(),
                    overwrite: merge,
                };
            }),
        );
        return download_file(
            transfers, target, emit, id, cancel, &source, dest, root, merge,
        )
        .await;
    }

    deliver(
        emit,
        transfers.update(id, |entry| {
            entry.view.kind = EntryKind::Folder;
            entry.view.local = dest.display().to_string();
        }),
    );
    match local_kind(&dest) {
        None => {}
        Some(FileKind::Dir) if merge => {}
        Some(FileKind::Dir) => {
            end(
                transfers,
                emit,
                id,
                TransferState::Failed,
                Some(format!(
                    "{} is already there. Nothing was copied into it.",
                    dest.display()
                )),
            );
            return Step::Next;
        }
        Some(_) => {
            end(
                transfers,
                emit,
                id,
                TransferState::Failed,
                Some(format!(
                    "{} is in the way and is not a folder. Nothing was copied.",
                    dest.display()
                )),
            );
            return Step::Next;
        }
    }
    walk_remote(
        transfers, target, emit, id, batch, &cancel, &source, name, &dest, root, merge,
    )
    .await
}

/// Walks the folder `source` of the customer computer through its listings,
/// makes its folders below `dest` on this computer and queues its files.
#[allow(clippy::too_many_arguments)]
async fn walk_remote<T: FileTarget>(
    transfers: &Transfers,
    target: &T,
    emit: &Emit,
    id: u64,
    batch: u64,
    cancel: &CancelToken,
    source: &str,
    shown: &str,
    dest: &Path,
    root: &Path,
    merge: bool,
) -> Step {
    let make = |folder: &Path| -> Result<(), String> {
        match std::fs::create_dir_all(folder) {
            Ok(()) => Ok(()),
            Err(error) => Err(format!(
                "The folder {} could not be made on this computer: {error}.",
                folder.display()
            )),
        }
    };
    if let Err(why) = make(dest) {
        end(transfers, emit, id, TransferState::Failed, Some(why));
        return Step::Next;
    }
    // What makes this folder incomplete here, the first reason first.
    let mut incomplete: Vec<String> = Vec::new();
    let mut files = 0u64;
    // Folders still to list: on the customer computer, on this computer, as
    // shown, and how deep.
    let mut pending: Vec<(String, PathBuf, String, usize)> =
        vec![(source.to_owned(), dest.to_path_buf(), shown.to_owned(), 0)];
    'walk: while let Some((remote, local, shown, depth)) = pending.pop() {
        if cancel.is_cancelled() {
            end(
                transfers,
                emit,
                id,
                TransferState::Cancelled,
                Some("What it had queued until then stays queued.".to_owned()),
            );
            return Step::Next;
        }
        let listed = match target.list(&remote, cancel).await {
            Ok(listed) => listed,
            Err(error) => {
                if let Some(ended) = Ended::of(&error) {
                    end(
                        transfers,
                        emit,
                        id,
                        TransferState::Stopped,
                        Some(ended.words().to_owned()),
                    );
                    return Step::Ended(ended);
                }
                incomplete.push(format!(
                    "{remote} could not be listed: {}.",
                    failure_words(&error)
                ));
                continue;
            }
        };
        if !listed.complete {
            incomplete.push(format!(
                "{remote} holds more entries than the explorer lists; the rest was not queued."
            ));
        }
        let mut planned = Vec::new();
        let mut below = Vec::new();
        for entry in listed.entries {
            let name = format!("{shown}\\{}", entry.name);
            let at = join(&remote, &entry.name);
            let here = match local::beside(&local, &entry.name) {
                Ok(here) => here,
                Err(why) => {
                    planned.push(Planned::note(
                        EntryKind::File,
                        name,
                        at,
                        local.display().to_string(),
                        &unusable(why),
                    ));
                    continue;
                }
            };
            match entry.kind {
                FileKind::Dir => {
                    if depth >= MAX_WALK_DEPTH {
                        incomplete.push(format!(
                            "Its folders go deeper than {MAX_WALK_DEPTH} levels; what lies \
                             deeper was not queued."
                        ));
                        continue;
                    }
                    if let Err(why) = make(&here) {
                        planned.push(Planned {
                            kind: EntryKind::Folder,
                            name,
                            remote: at,
                            local: here.display().to_string(),
                            bytes: None,
                            state: TransferState::Failed,
                            reason: Some(why),
                            work: Work::Note,
                            quiet: false,
                        });
                        continue;
                    }
                    below.push((at, here, name, depth + 1));
                }
                FileKind::File => {
                    let walked = transfers
                        .with_batch(batch, |batch| {
                            batch.walked += 1;
                            batch.walked
                        })
                        .unwrap_or(usize::MAX);
                    if walked > MAX_BATCH_ENTRIES {
                        incomplete.push(format!(
                            "It holds more than the {MAX_BATCH_ENTRIES} files one copy queues; \
                             the rest was not queued."
                        ));
                        deliver_planned(transfers, emit, batch, planned);
                        break 'walk;
                    }
                    files += 1;
                    planned.push(Planned {
                        kind: EntryKind::File,
                        name,
                        remote: at.clone(),
                        local: here.display().to_string(),
                        bytes: Some(entry.len),
                        state: TransferState::Waiting,
                        reason: None,
                        work: Work::DownloadFile {
                            source: at,
                            dest: here,
                            root: root.to_path_buf(),
                            overwrite: merge,
                        },
                        quiet: false,
                    });
                }
                FileKind::Symlink => planned.push(Planned::note(
                    EntryKind::File,
                    name,
                    at,
                    local.display().to_string(),
                    LINK_LEFT_OUT,
                )),
                FileKind::Other => planned.push(Planned::note(
                    EntryKind::File,
                    name,
                    at,
                    local.display().to_string(),
                    NOT_A_FILE_OR_FOLDER,
                )),
            }
        }
        deliver_planned(transfers, emit, batch, planned);
        // Depth first, in the order of the listing.
        pending.extend(below.into_iter().rev());
    }
    match incomplete.first() {
        Some(first) => {
            let more = incomplete.len() - 1;
            let mut reason = format!("Not all of this folder was queued. {first}");
            if more > 0 {
                reason.push_str(&format!(
                    " And {more} more of its folders are not complete."
                ));
            }
            end(transfers, emit, id, TransferState::Failed, Some(reason));
        }
        // Its files stand for it from here on; an empty folder is made, and
        // that is all there was to copy.
        None if files > 0 => end(transfers, emit, id, TransferState::Listed, None),
        None => end(transfers, emit, id, TransferState::Done, None),
    }
    Step::Next
}

/// Adds the rows a walk found to the batch `batch`, unless the queue is full:
/// then one row says so.
fn deliver_planned(transfers: &Transfers, emit: &Emit, batch: u64, planned: Vec<Planned>) {
    if planned.is_empty() {
        return;
    }
    let views = {
        let mut queue = transfers.lock();
        if queue.entries.len() + planned.len() > MAX_QUEUE_ENTRIES {
            let room = MAX_QUEUE_ENTRIES.saturating_sub(queue.entries.len());
            let mut kept: Vec<Planned> = planned.into_iter().take(room.saturating_sub(1)).collect();
            if room > 0 {
                kept.push(Planned {
                    kind: EntryKind::Folder,
                    name: "The list of transfers is full".to_owned(),
                    remote: String::new(),
                    local: String::new(),
                    bytes: None,
                    state: TransferState::Failed,
                    reason: Some(QUEUE_FULL.to_owned()),
                    work: Work::Note,
                    quiet: false,
                });
            }
            Transfers::append(&mut queue, batch, kept, Direction::Download)
        } else {
            Transfers::append(&mut queue, batch, planned, Direction::Download)
        }
    };
    deliver(emit, views);
}

#[cfg(test)]
mod tests;
