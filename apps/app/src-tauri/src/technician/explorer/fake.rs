//! A customer computer held in memory, for the tests of the transfer queue
//! and of the changes (ST-M010-002).
//!
//! The wire accepts a path only when it is rooted on a drive, so a test off
//! Windows cannot send into a real folder or walk one through a session.
//! [`Fake`] answers [`FileTarget`] from a tree of folders in memory, the way
//! the real one answers: a transfer in steps with its progress, a part that a
//! cancel leaves or removes, a name that is taken, a refusal with its
//! category, and the end of the session. It never waits: a test that awaits
//! the queue has run it to its end.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use cretlink_console::{ProgressSink, SendToFolder};
use cretlink_mcp::{DownloadReport, TargetError, TransferFailure, UploadReport};
use cretlink_protocol::{ErrorCode, ErrorStage, FileEntry, FileKind, JobOutcome, TreeFailure};
use cretlink_session::{
    CancelToken, TransferProgress, TreeRemovalProgress, TreeRemovalReport, download_part_path,
};

use super::target::{FileTarget, Listed};

/// One entry of a folder of the fake computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Node {
    File(Vec<u8>),
    Folder,
    Link,
    Other,
}

/// What a test wants to happen while a transfer of the fake computer runs:
/// called with the path on the customer computer and the bytes confirmed.
pub(crate) type Hook = Box<dyn FnMut(&str, u64) + Send>;

#[derive(Default)]
struct State {
    /// Every folder by its path, with its entries by name.
    folders: BTreeMap<String, BTreeMap<String, Node>>,
    /// The paths an unfinished part of a transfer lies beside.
    parts: HashSet<String>,
    /// Requests on these paths are refused with this category.
    refused: HashMap<String, ErrorCode>,
    /// Folders whose listing stops before their end.
    cut: HashSet<String>,
    /// A cancelled send leaves its part behind.
    keeps_parts: bool,
    /// The session ended this way: every request says so.
    ended: Option<TargetError>,
    /// What was asked, in order.
    asked: Vec<String>,
    hook: Option<Hook>,
}

/// The bytes one step of a transfer carries.
const STEP: usize = 1000;

/// A customer computer held in memory. Cloning shares the same computer.
#[derive(Clone, Default)]
pub(crate) struct Fake(Arc<Mutex<State>>);

fn parent_of(path: &str) -> (String, String) {
    let (above, name) = path.rsplit_once('\\').expect("a path below a drive");
    let above = if above.contains('\\') {
        above.to_owned()
    } else {
        format!("{above}\\")
    };
    (above, name.to_owned())
}

fn refusal(code: ErrorCode) -> TargetError {
    TargetError::Remote {
        code,
        message: "refused by the fake customer computer".to_owned(),
        stage: ErrorStage::Execution,
    }
}

fn kind_of(node: &Node) -> FileKind {
    match node {
        Node::File(_) => FileKind::File,
        Node::Folder => FileKind::Dir,
        Node::Link => FileKind::Symlink,
        Node::Other => FileKind::Other,
    }
}

impl Fake {
    /// A computer with the drive `C:` and the folders of an account on it.
    pub(crate) fn new() -> Self {
        let fake = Self::default();
        for folder in [
            "C:\\",
            "C:\\Users",
            "C:\\Users\\ann",
            "C:\\Users\\ann\\Documents",
        ] {
            fake.folder(folder);
        }
        fake
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }

    /// Makes the folder `path`, and names it in the folder above it.
    pub(crate) fn folder(&self, path: &str) {
        let mut state = self.state();
        state.folders.entry(path.to_owned()).or_default();
        if path.trim_end_matches('\\').contains('\\') {
            let (above, name) = parent_of(path);
            state
                .folders
                .entry(above)
                .or_default()
                .insert(name, Node::Folder);
        }
    }

    /// Puts the entry `node` at `path`.
    pub(crate) fn put(&self, path: &str, node: Node) {
        let (above, name) = parent_of(path);
        self.state()
            .folders
            .entry(above)
            .or_default()
            .insert(name, node);
    }

    /// Puts an entry under any name at all into `folder`, as only a computer
    /// that means harm could list it.
    pub(crate) fn put_raw(&self, folder: &str, name: &str, node: Node) {
        let mut state = self.state();
        if node == Node::Folder {
            state
                .folders
                .entry(super::transfers::join(folder, name))
                .or_default();
        }
        state
            .folders
            .entry(folder.to_owned())
            .or_default()
            .insert(name.to_owned(), node);
    }

    /// The entry at `path`, when there is one.
    pub(crate) fn at(&self, path: &str) -> Option<Node> {
        let (above, name) = parent_of(path);
        self.state().folders.get(&above)?.get(&name).cloned()
    }

    /// The names the folder `path` holds, in order.
    pub(crate) fn names(&self, path: &str) -> Vec<String> {
        self.state()
            .folders
            .get(path)
            .map(|entries| entries.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub(crate) fn refuse(&self, path: &str, code: ErrorCode) {
        self.state().refused.insert(path.to_owned(), code);
    }

    pub(crate) fn cut(&self, folder: &str) {
        self.state().cut.insert(folder.to_owned());
    }

    pub(crate) fn keep_parts(&self) {
        self.state().keeps_parts = true;
    }

    /// Ends the session: every later request answers `error`.
    pub(crate) fn end(&self, error: TargetError) {
        self.state().ended = Some(error);
    }

    pub(crate) fn on_progress(&self, hook: Hook) {
        self.state().hook = Some(hook);
    }

    /// What was asked of this computer, in order.
    pub(crate) fn asked(&self) -> Vec<String> {
        self.state().asked.clone()
    }

    /// Whether an unfinished part lies beside `path`.
    pub(crate) fn has_part(&self, path: &str) -> bool {
        self.state().parts.contains(path)
    }

    /// Notes a request, and answers the end of the session or the refusal
    /// that was set up for its path.
    fn ask(&self, what: &str, path: &str) -> Result<(), TargetError> {
        let mut state = self.state();
        state.asked.push(format!("{what} {path}"));
        if let Some(ended) = &state.ended {
            return Err(ended.clone());
        }
        match state.refused.get(path) {
            Some(code) => Err(refusal(*code)),
            None => Ok(()),
        }
    }

    /// Runs the hook of the test, outside the lock, so that it may ask the
    /// computer or the queue for something.
    fn step(&self, path: &str, done: u64) {
        let hook = self.state().hook.take();
        if let Some(mut hook) = hook {
            hook(path, done);
            let mut state = self.state();
            if state.hook.is_none() {
                state.hook = Some(hook);
            }
        }
    }

    fn ended(&self) -> Option<TargetError> {
        self.state().ended.clone()
    }
}

fn failed(code: ErrorCode, stage: &str) -> TransferFailure {
    TransferFailure {
        code,
        message: "refused by the fake customer computer".to_owned(),
        stage: stage.to_owned(),
    }
}

fn upload(
    outcome: JobOutcome,
    len: u64,
    sent: u64,
    error: Option<TransferFailure>,
) -> UploadReport {
    UploadReport {
        outcome,
        remote_path: None,
        len,
        sent,
        digest: Vec::new(),
        resumed_from: 0,
        restarted: false,
        error,
    }
}

fn download(
    outcome: JobOutcome,
    dest: &std::path::Path,
    len: u64,
    sent: u64,
    error: Option<TransferFailure>,
) -> DownloadReport {
    DownloadReport {
        outcome,
        local_path: dest.display().to_string(),
        len,
        sent,
        digest: Vec::new(),
        resumed_from: 0,
        restarted: false,
        error,
    }
}

impl FileTarget for Fake {
    async fn list(&self, folder: &str, cancel: &CancelToken) -> Result<Listed, TargetError> {
        self.ask("list", folder)?;
        if cancel.is_cancelled() {
            return Ok(Listed {
                entries: Vec::new(),
                complete: false,
            });
        }
        let state = self.state();
        let entries = state
            .folders
            .get(folder)
            .ok_or_else(|| refusal(ErrorCode::PathNotFound))?;
        Ok(Listed {
            entries: entries
                .iter()
                .map(|(name, node)| FileEntry {
                    name: name.clone(),
                    kind: kind_of(node),
                    len: match node {
                        Node::File(bytes) => bytes.len() as u64,
                        _ => 0,
                    },
                    modified_ms: None,
                    readonly: false,
                })
                .collect(),
            complete: !state.cut.contains(folder),
        })
    }

    async fn send(
        &self,
        send: SendToFolder,
        cancel: CancelToken,
        mut progress: ProgressSink<TransferProgress>,
    ) -> Result<UploadReport, TargetError> {
        // Refused before it began, as the real service refuses it.
        send.dest()?;
        let dest = super::transfers::join(&send.folder, &send.name);
        let Ok(bytes) = std::fs::read(&send.local_path) else {
            return Ok(upload(
                JobOutcome::Failed,
                0,
                0,
                Some(TransferFailure {
                    code: ErrorCode::PathNotFound,
                    message: format!(
                        "cannot read {} on the technician computer: the file does not exist",
                        send.local_path.display()
                    ),
                    stage: "local".to_owned(),
                }),
            ));
        };
        let total = bytes.len() as u64;
        if let Err(error) = self.ask("send", &dest) {
            return match error {
                TargetError::Remote { code, .. } => Ok(upload(
                    JobOutcome::Failed,
                    total,
                    0,
                    Some(failed(code, "write")),
                )),
                other => Err(other),
            };
        }
        {
            let state = self.state();
            let Some(folder) = state.folders.get(&send.folder) else {
                return Ok(upload(
                    JobOutcome::Failed,
                    total,
                    0,
                    Some(failed(ErrorCode::PathNotFound, "stat")),
                ));
            };
            // Windows tells no two names apart by letter case.
            let taken = folder
                .iter()
                .find(|(name, _)| name.to_lowercase() == send.name.to_lowercase());
            match taken {
                Some((_, Node::File(_))) if send.overwrite => {}
                Some((_, Node::File(_))) => {
                    return Ok(upload(
                        JobOutcome::Failed,
                        total,
                        0,
                        Some(failed(ErrorCode::AlreadyExists, "stat")),
                    ));
                }
                Some(_) => {
                    return Ok(upload(
                        JobOutcome::Failed,
                        total,
                        0,
                        Some(failed(ErrorCode::WrongPathKind, "commit")),
                    ));
                }
                None => {}
            }
        }
        self.state().parts.insert(dest.clone());
        let mut sent = 0u64;
        progress(TransferProgress {
            transferred: 0,
            total,
        });
        for chunk in bytes.chunks(STEP) {
            if cancel.is_cancelled() {
                let mut state = self.state();
                if !state.keeps_parts {
                    state.parts.remove(&dest);
                }
                return Ok(upload(JobOutcome::Cancelled, total, sent, None));
            }
            if let Some(ended) = self.ended() {
                return Err(ended);
            }
            sent += chunk.len() as u64;
            progress(TransferProgress {
                transferred: sent,
                total,
            });
            self.step(&dest, sent);
        }
        if cancel.is_cancelled() {
            let mut state = self.state();
            if !state.keeps_parts {
                state.parts.remove(&dest);
            }
            return Ok(upload(JobOutcome::Cancelled, total, sent, None));
        }
        if let Some(ended) = self.ended() {
            return Err(ended);
        }
        let mut state = self.state();
        state.parts.remove(&dest);
        let folder = state
            .folders
            .get_mut(&send.folder)
            .expect("looked at above");
        // A file that is replaced keeps the name it had.
        let name = folder
            .keys()
            .find(|name| name.to_lowercase() == send.name.to_lowercase())
            .cloned()
            .unwrap_or_else(|| send.name.clone());
        folder.insert(name, Node::File(bytes));
        Ok(upload(JobOutcome::Completed, total, sent, None))
    }

    async fn download(
        &self,
        source: &str,
        dest: PathBuf,
        cancel: CancelToken,
        mut progress: ProgressSink<TransferProgress>,
    ) -> Result<DownloadReport, TargetError> {
        if let Err(error) = self.ask("download", source) {
            return match error {
                TargetError::Remote { code, .. } => Ok(download(
                    JobOutcome::Failed,
                    &dest,
                    0,
                    0,
                    Some(failed(code, "read")),
                )),
                other => Err(other),
            };
        }
        let bytes = match self.at(source) {
            Some(Node::File(bytes)) => bytes,
            Some(_) => {
                return Ok(download(
                    JobOutcome::Failed,
                    &dest,
                    0,
                    0,
                    Some(failed(ErrorCode::WrongPathKind, "read")),
                ));
            }
            None => {
                return Ok(download(
                    JobOutcome::Failed,
                    &dest,
                    0,
                    0,
                    Some(failed(ErrorCode::PathNotFound, "stat")),
                ));
            }
        };
        let total = bytes.len() as u64;
        let part = download_part_path(&dest);
        let mut written = Vec::new();
        let mut sent = 0u64;
        progress(TransferProgress {
            transferred: 0,
            total,
        });
        for chunk in bytes.chunks(STEP) {
            if cancel.is_cancelled() {
                // The real engine keeps the part of a cancelled copy.
                std::fs::write(&part, &written).expect("write the part");
                return Ok(download(JobOutcome::Cancelled, &dest, total, sent, None));
            }
            if let Some(ended) = self.ended() {
                std::fs::write(&part, &written).expect("write the part");
                return Err(ended);
            }
            written.extend_from_slice(chunk);
            sent += chunk.len() as u64;
            progress(TransferProgress {
                transferred: sent,
                total,
            });
            self.step(source, sent);
        }
        if cancel.is_cancelled() {
            std::fs::write(&part, &written).expect("write the part");
            return Ok(download(JobOutcome::Cancelled, &dest, total, sent, None));
        }
        if let Err(error) = std::fs::write(&dest, &written) {
            return Ok(download(
                JobOutcome::Failed,
                &dest,
                total,
                sent,
                Some(TransferFailure {
                    code: ErrorCode::Internal,
                    message: format!(
                        "cannot write {} on the technician computer: {error}",
                        dest.display()
                    ),
                    stage: "local".to_owned(),
                }),
            ));
        }
        Ok(download(JobOutcome::Completed, &dest, total, sent, None))
    }

    async fn create_folder(&self, path: &str) -> Result<(), TargetError> {
        self.ask("create_folder", path)?;
        let (above, name) = parent_of(path);
        let mut state = self.state();
        let folder = state
            .folders
            .get_mut(&above)
            .ok_or_else(|| refusal(ErrorCode::PathNotFound))?;
        if folder
            .keys()
            .any(|taken| taken.to_lowercase() == name.to_lowercase())
        {
            return Err(refusal(ErrorCode::AlreadyExists));
        }
        folder.insert(name, Node::Folder);
        state.folders.insert(path.to_owned(), BTreeMap::new());
        Ok(())
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), TargetError> {
        self.ask("rename", from)?;
        let (above, name) = parent_of(from);
        let (into, new) = parent_of(to);
        let mut state = self.state();
        if state
            .folders
            .get(&into)
            .ok_or_else(|| refusal(ErrorCode::PathNotFound))?
            .contains_key(&new)
        {
            return Err(refusal(ErrorCode::AlreadyExists));
        }
        let node = state
            .folders
            .get_mut(&above)
            .and_then(|folder| folder.remove(&name))
            .ok_or_else(|| refusal(ErrorCode::PathNotFound))?;
        if node == Node::Folder
            && let Some(held) = state.folders.remove(from)
        {
            state.folders.insert(to.to_owned(), held);
        }
        state.folders.entry(into).or_default().insert(new, node);
        Ok(())
    }

    async fn delete(&self, path: &str) -> Result<(), TargetError> {
        self.ask("delete", path)?;
        let (above, name) = parent_of(path);
        let mut state = self.state();
        let folder = state
            .folders
            .get_mut(&above)
            .ok_or_else(|| refusal(ErrorCode::PathNotFound))?;
        match folder.get(&name) {
            None => Err(refusal(ErrorCode::PathNotFound)),
            // A folder is never deleted as a file.
            Some(Node::Folder) => Err(refusal(ErrorCode::WrongPathKind)),
            Some(_) => {
                folder.remove(&name);
                Ok(())
            }
        }
    }

    async fn remove_tree(
        &self,
        path: &str,
        cancel: CancelToken,
        mut progress: ProgressSink<TreeRemovalProgress>,
    ) -> Result<TreeRemovalReport, TargetError> {
        let mut report = TreeRemovalReport {
            outcome: JobOutcome::Failed,
            removed_files: 0,
            removed_dirs: 0,
            failed: Vec::new(),
            failed_more: 0,
            blocked: false,
            error: None,
        };
        if let Err(error) = self.ask("remove_tree", path) {
            return match error {
                TargetError::Remote { code, message, .. } => {
                    report.error = Some(cretlink_session::TreeRemovalError { code, message });
                    Ok(report)
                }
                other => Err(other),
            };
        }
        // A link and a file are not folders: the removal refuses them.
        if self.at(path) != Some(Node::Folder) {
            report.error = Some(cretlink_session::TreeRemovalError {
                code: ErrorCode::WrongPathKind,
                message: "the path is not a folder".to_owned(),
            });
            return Ok(report);
        }
        // Deepest first: a folder goes after what it holds.
        let mut inside: Vec<String> = self
            .state()
            .folders
            .keys()
            .filter(|folder| *folder == path || folder.starts_with(&format!("{path}\\")))
            .cloned()
            .collect();
        inside.sort_by_key(|folder| std::cmp::Reverse(folder.len()));
        for folder in inside {
            if cancel.is_cancelled() {
                report.outcome = JobOutcome::Cancelled;
                return Ok(report);
            }
            if let Some(ended) = self.ended() {
                return Err(ended);
            }
            let names: Vec<String> = self.names(&folder);
            let mut left = false;
            for name in names {
                let at = super::transfers::join(&folder, &name);
                let held = self.state().refused.get(&at).copied();
                if let Some(code) = held {
                    report.failed.push(TreeFailure { path: at, code });
                    left = true;
                    continue;
                }
                let mut state = self.state();
                let node = state
                    .folders
                    .get_mut(&folder)
                    .and_then(|entries| entries.remove(&name));
                if node != Some(Node::Folder) {
                    report.removed_files += 1;
                }
            }
            if left {
                report.failed.push(TreeFailure {
                    path: folder.clone(),
                    code: ErrorCode::DirNotEmpty,
                });
                // The folders above it stay too; the fake stops here.
                break;
            }
            let mut state = self.state();
            state.folders.remove(&folder);
            let (above, name) = parent_of(&folder);
            if let Some(entries) = state.folders.get_mut(&above) {
                entries.remove(&name);
            }
            drop(state);
            report.removed_dirs += 1;
            progress(TreeRemovalProgress {
                removed_files: report.removed_files,
                removed_dirs: report.removed_dirs,
            });
            self.step(&folder, report.removed_dirs);
        }
        if report.failed.is_empty() {
            report.outcome = JobOutcome::Completed;
        }
        Ok(report)
    }

    async fn part_left(&self, path: &str) -> Result<bool, TargetError> {
        self.ask("look_up", path)?;
        Ok(self.state().parts.contains(path))
    }
}
