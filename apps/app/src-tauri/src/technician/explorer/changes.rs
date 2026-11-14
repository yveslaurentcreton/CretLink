//! Renaming and deleting from the file explorer (ST-M010-002, REQ-F-028).
//!
//! # Rename
//!
//! A rename gives one entry of the open folder another name in that folder.
//! The new name is one name: a text with a separator in it would move the
//! entry somewhere else, and is refused before anything is asked. A name that
//! is taken is refused by the customer computer, which never replaces by a
//! move, and the entry stays as it is.
//!
//! # Delete
//!
//! The technician confirms the selection once, in the window. Each item is
//! then removed by the operation its kind has: a folder with everything in
//! it ([`FileTarget::remove_tree`]), a file or a link as that one entry
//! ([`FileTarget::delete`]) — a link as the link, never what it points at.
//! The kind is the one the window listed. It only chooses the operation: the
//! customer computer refuses a folder that is asked to be deleted as a file,
//! and a file or a link that is asked to be removed as a folder, so a kind
//! that is out of date removes nothing it was not asked for.
//!
//! Nothing is moved to a recycle bin on this path, and the window says so
//! before it asks.
//!
//! # An honest result (REQ-N-008)
//!
//! [`DeleteReport`] counts an item as removed only when the customer
//! computer said so. What could not be removed is named with the reason the
//! customer computer gave: the first entries one by one, the rest as a
//! number. A removal the technician cancelled says what was removed until
//! then, and one that the end of the session stopped says that.

use cretlink_mcp::TargetError;
use cretlink_protocol::{ErrorCode, ErrorStage, FileKind, JobOutcome};
use cretlink_session::{CancelToken, TreeRemovalProgress, activity::error_text};
use serde::{Deserialize, Serialize};

use super::target::{Ended, FileTarget, failure_words};
use super::transfers::join;

/// The most items one delete takes (REQ-N-006). A larger selection is
/// refused before anything is removed.
pub(crate) const MAX_DELETE_ITEMS: usize = 10_000;

/// The most entries a result names one by one; the others are counted.
const MAX_NAMED: usize = 20;

/// One selected item of a delete, as the window listed it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct DeleteItem {
    /// Its name in the open folder.
    pub(crate) name: String,
    /// What the listing said it is.
    pub(crate) kind: FileKind,
}

/// One entry that could not be removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct NotRemoved {
    /// Where it is on the customer computer.
    pub(crate) path: String,
    /// Why, in the words the application has for the customer computer's
    /// reason.
    pub(crate) reason: String,
}

/// How a delete ended (`DeleteReport` in the frontend contract).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeleteReport {
    /// How many items were selected.
    pub(crate) selected: u64,
    /// How many of them the customer computer removed.
    pub(crate) removed: u64,
    /// The files and links removed in all, those inside removed folders
    /// included.
    pub(crate) removed_files: u64,
    /// The folders removed in all.
    pub(crate) removed_dirs: u64,
    /// The first entries that could not be removed, each with its reason.
    pub(crate) failed: Vec<NotRemoved>,
    /// How many more could not be removed.
    pub(crate) failed_more: u64,
    /// The technician cancelled; the items that were not tried are counted
    /// in `not_tried`.
    pub(crate) cancelled: bool,
    /// The session ended on the way: what the window says about it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ended: Option<String>,
    /// How many selected items were not tried at all.
    pub(crate) not_tried: u64,
}

impl DeleteReport {
    fn not_removed(&mut self, path: String, reason: String) {
        if self.failed.len() < MAX_NAMED {
            self.failed.push(NotRemoved { path, reason });
        } else {
            self.failed_more += 1;
        }
    }
}

/// Where a delete stands, for the window (`RemovalProgress` in the frontend
/// contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RemovalProgress {
    /// The number the window gave this delete.
    pub(crate) removal: u64,
    /// The item that is being removed.
    pub(crate) item: String,
    /// Its place among the selected items, from 1.
    pub(crate) at: u64,
    /// How many items were selected.
    pub(crate) of: u64,
    /// The files and links removed so far, over all items.
    pub(crate) removed_files: u64,
    /// The folders removed so far, over all items.
    pub(crate) removed_dirs: u64,
}

/// A refusal this computer makes before anything is asked.
fn refused(message: impl Into<String>) -> TargetError {
    TargetError::Remote {
        code: ErrorCode::InvalidPath,
        message: message.into(),
        stage: ErrorStage::Validation,
    }
}

/// Whether `name` is one name of a folder: not empty, and without a
/// character that would make it a path or that no name on Windows may hold.
pub(crate) fn one_name(name: &str) -> Result<(), TargetError> {
    if name.trim().is_empty() {
        return Err(refused("a name cannot be empty"));
    }
    if name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|'])
        || name.chars().any(char::is_control)
    {
        return Err(refused(
            "a name cannot hold any of \\ / : * ? \" < > | or a control character",
        ));
    }
    if name == "." || name == ".." {
        return Err(refused("this name stands for a folder, not for an entry"));
    }
    if name.ends_with(['.', ' ']) {
        return Err(refused("a name cannot end in a dot or a space"));
    }
    Ok(())
}

/// Renames the entry `name` of the folder `folder` to `to`, in that folder.
///
/// # Errors
///
/// An `invalid_path` refusal in the stage `validation` when either name is
/// not one name: nothing was asked then. Otherwise what the customer computer
/// answered — `already_exists` for a name that is taken — or the end of the
/// session.
pub(crate) async fn rename<T: FileTarget>(
    target: &T,
    folder: &str,
    name: &str,
    to: &str,
) -> Result<(), TargetError> {
    one_name(name)?;
    one_name(to)?;
    if name == to {
        return Err(refused("that is the name it has"));
    }
    target.rename(&join(folder, name), &join(folder, to)).await
}

/// Deletes `items` from the folder `folder`, one after the other, and says
/// how it ended. See the module documentation.
///
/// `cancel` ends the delete between two items and between two steps of a
/// folder's removal. `progress` is told each item when it begins and the
/// counts after each step of a folder's removal.
pub(crate) async fn delete<T: FileTarget>(
    target: &T,
    removal: u64,
    folder: &str,
    items: &[DeleteItem],
    cancel: &CancelToken,
    progress: std::sync::Arc<dyn Fn(RemovalProgress) + Send + Sync>,
) -> DeleteReport {
    let mut report = DeleteReport {
        selected: items.len() as u64,
        ..DeleteReport::default()
    };
    for (index, item) in items.iter().enumerate() {
        let rest = (items.len() - index) as u64;
        if cancel.is_cancelled() {
            report.cancelled = true;
            report.not_tried = rest;
            break;
        }
        let path = join(folder, &item.name);
        if let Err(error) = one_name(&item.name) {
            report.not_removed(path, failure_words(&error));
            continue;
        }
        let at = index as u64 + 1;
        let (files_before, dirs_before) = (report.removed_files, report.removed_dirs);
        progress(RemovalProgress {
            removal,
            item: item.name.clone(),
            at,
            of: report.selected,
            removed_files: files_before,
            removed_dirs: dirs_before,
        });
        if item.kind != FileKind::Dir {
            // A file or a link, removed as that one entry.
            match target.delete(&path).await {
                Ok(()) => {
                    report.removed += 1;
                    report.removed_files += 1;
                }
                Err(error) => match Ended::of(&error) {
                    Some(ended) => {
                        report.ended = Some(ended.words().to_owned());
                        report.not_tried = rest;
                        break;
                    }
                    None => report.not_removed(path, failure_words(&error)),
                },
            }
            continue;
        }
        let sink = {
            let progress = std::sync::Arc::clone(&progress);
            let (item, of) = (item.name.clone(), report.selected);
            Box::new(move |step: TreeRemovalProgress| {
                progress(RemovalProgress {
                    removal,
                    item: item.clone(),
                    at,
                    of,
                    removed_files: files_before + step.removed_files,
                    removed_dirs: dirs_before + step.removed_dirs,
                });
            })
        };
        let removed = match target.remove_tree(&path, cancel.clone(), sink).await {
            Ok(removed) => removed,
            Err(error) => match Ended::of(&error) {
                Some(ended) => {
                    report.ended = Some(ended.words().to_owned());
                    report.not_tried = rest;
                    break;
                }
                None => {
                    report.not_removed(path, failure_words(&error));
                    continue;
                }
            },
        };
        report.removed_files += removed.removed_files;
        report.removed_dirs += removed.removed_dirs;
        match removed.outcome {
            JobOutcome::Completed => report.removed += 1,
            JobOutcome::Cancelled if cancel.is_cancelled() => {
                report.cancelled = true;
                // This item was begun and is not gone; the others were not
                // tried.
                report.not_removed(
                    path,
                    "its removal was cancelled before it was complete".to_owned(),
                );
                report.not_tried = rest - 1;
                break;
            }
            // Cancelled, and not by the technician: a stop or a revocation
            // ends a removal between two of its steps.
            JobOutcome::Cancelled => {
                report.ended = Some(Ended::Stopped.words().to_owned());
                report.not_removed(path, "the session ended before it was removed".to_owned());
                report.not_tried = rest - 1;
                break;
            }
            JobOutcome::Failed => {
                if let Some(error) = &removed.error {
                    report.not_removed(path.clone(), error_text(error.code).to_owned());
                }
                for left in &removed.failed {
                    report.not_removed(left.path.clone(), error_text(left.code).to_owned());
                }
                report.failed_more += removed.failed_more;
                if removed.blocked {
                    report.not_removed(
                        path,
                        "the removal gave up before it had tried every entry".to_owned(),
                    );
                } else if removed.error.is_none() && removed.failed.is_empty() {
                    report.not_removed(path, "the folder could not be removed".to_owned());
                }
            }
            JobOutcome::TimedOut | JobOutcome::Unknown => {
                report.not_removed(path, "whether it was removed is not known".to_owned());
            }
        }
    }
    report
}

#[cfg(test)]
mod tests;
