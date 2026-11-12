//! Console-side drive list and folder-removal engine (ST-M010-001, ADR-0012).
//!
//! [`HostHandle::list_drives`] asks the target for its drives.
//! [`HostHandle::remove_tree`] removes a folder with everything in it. On the
//! wire that removal is a sequence of bounded steps
//! ([`Operation::RemoveTree`]): each request removes what one step may and
//! answers; this engine repeats the request while the answer is
//! [`TreeState::More`], adds the counts up, and turns the last answer into one
//! honest [`TreeRemovalReport`].
//!
//! # One folder
//!
//! An answer that says more remains names the folder the agent is removing
//! (`folder_id`). The engine gives that name back with the next request, so
//! the agent goes on with the folder it began with and refuses any other that
//! lies at the path by then: between two steps the agent holds nothing, and a
//! program on the target could swap the folder for another one.
//!
//! # Why steps
//!
//! The agent serves one request at a time. A removal that ran to its end
//! inside one request would hold the agent for as long as the folder takes,
//! and neither a cancellation nor a stop by the customer could be served in
//! the meantime. With bounded steps the agent is free again after each one:
//! a stop or a revocation takes effect between two steps, and so does a
//! cancellation, which is this engine no longer asking (REQ-S-004,
//! REQ-N-006).
//!
//! # Bounded
//!
//! Each step is bounded by the agent, and the number of steps is bounded
//! here: after [`MAX_TREE_REMOVAL_STEPS`] steps that each left more to do the
//! removal ends as failed, with the reason, and the agent is told that it
//! ended. A folder that a program on the target keeps refilling can therefore
//! not keep a removal running for ever (REQ-N-006).
//!
//! # Honest outcomes (REQ-N-008)
//!
//! The report is [`JobOutcome::Completed`] only when the folder is gone. A
//! removal that left something behind is [`JobOutcome::Failed`] and names the
//! first entries with their reasons; a cancelled one is
//! [`JobOutcome::Cancelled`] and says what it had removed until then, and the
//! agent is told that it ended, so the customer does not see it as still
//! running. An answer that contradicts itself — a step that claims progress
//! and removed nothing, a folder that is gone and yet has failures — is a
//! [`SessionError::Unexpected`], never a result.

use std::future::Future;

use cretlink_protocol::{B64Bytes, DriveEntry, ErrorCode, OpResult, Operation, RemotePath};
use cretlink_protocol::{TreeFailure, TreeState};

use crate::{Attribution, CancelToken, HostHandle, JobOutcome, Origin, SessionError};

/// The most steps one folder removal asks the agent for.
///
/// Every step that says more remains has removed something, so a removal of a
/// folder that nobody adds to ends by itself. One that never ends is a folder
/// that something keeps filling while it is being removed; the engine then
/// gives up and says so, instead of asking for as long as the session lasts.
/// A million steps are far more than any real folder takes: a step settles up
/// to [`cretlink_protocol::limits::MAX_TREE_STEP_ENTRIES`] entries.
pub const MAX_TREE_REMOVAL_STEPS: u64 = 1_000_000;

/// A progress snapshot delivered to a folder removal's `on_progress` callback
/// after each step the agent answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeRemovalProgress {
    /// Files and links removed so far.
    pub removed_files: u64,
    /// Folders removed so far.
    pub removed_dirs: u64,
}

/// Why the agent refused a step of a folder removal (secret-free).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRemovalError {
    /// The machine-readable category the agent returned.
    pub code: ErrorCode,
    /// The agent's human-readable, secret-free description.
    pub message: String,
}

/// The honest terminal report of a folder removal (ST-M010-001).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRemovalReport {
    /// Terminal outcome: `Completed` only when the folder is gone, `Cancelled`
    /// when the caller cancelled, `Failed` when something was left behind or
    /// the agent refused.
    pub outcome: JobOutcome,
    /// Files and links removed, over all steps.
    pub removed_files: u64,
    /// Folders removed, over all steps.
    pub removed_dirs: u64,
    /// The first entries that could not be removed, each with its reason (at
    /// most [`cretlink_protocol::limits::MAX_TREE_FAILURES`]). Empty unless
    /// the outcome is `Failed`. Their paths are the target's own text.
    pub failed: Vec<TreeFailure>,
    /// How many more entries could not be removed, beyond `failed`.
    pub failed_more: u64,
    /// Whether the removal gave up before it had tried every entry: a step
    /// of the agent reached its bound without removing anything
    /// ([`TreeState::Blocked`]), so `failed` and `failed_more` do not cover
    /// what lies beyond. `failed` then names the entries that step could not
    /// remove, or the folder it stood in with [`ErrorCode::LimitExceeded`]
    /// when it met none it could name.
    pub blocked: bool,
    /// The error of a step the agent refused, for a `Failed` removal that
    /// ended that way (the folder does not exist, it is a file, the agent
    /// does not serve the operation).
    pub error: Option<TreeRemovalError>,
}

impl TreeRemovalReport {
    /// A report of a removal that has removed nothing yet, to be filled in.
    fn started() -> Self {
        Self {
            outcome: JobOutcome::Unknown,
            removed_files: 0,
            removed_dirs: 0,
            failed: Vec::new(),
            failed_more: 0,
            blocked: false,
            error: None,
        }
    }

    /// This report, ended with `outcome`.
    fn ended(mut self, outcome: JobOutcome) -> Self {
        self.outcome = outcome;
        self
    }
}

impl HostHandle {
    /// Lists the drives of the target, each with its root, kind, label and
    /// total and free space; a value the target could not read is absent
    /// (ST-M010-001).
    ///
    /// # Errors
    ///
    /// [`SessionError::Remote`] with [`ErrorCode::Unsupported`] if the agent
    /// does not advertise the explorer file capability; plus the errors of
    /// [`Self::file_op`].
    pub async fn list_drives(&self) -> Result<Vec<DriveEntry>, SessionError> {
        self.list_drives_as(Origin::Console).await
    }

    /// Like [`Self::list_drives`], but stamps an explicit [`Attribution`] onto
    /// the wire request.
    ///
    /// # Errors
    ///
    /// See [`Self::list_drives`].
    pub async fn list_drives_as(
        &self,
        attribution: impl Into<Attribution>,
    ) -> Result<Vec<DriveEntry>, SessionError> {
        match self.file_op_as(Operation::FsDrives {}, attribution).await? {
            OpResult::Drives { drives } => Ok(drives),
            _ => Err(SessionError::Unexpected {
                context: "list_drives",
            }),
        }
    }

    /// Removes the folder at `path` on the target with everything in it and
    /// returns an honest [`TreeRemovalReport`] (ST-M010-001). The files are
    /// deleted, not moved to a recycle bin.
    ///
    /// `on_progress` is called with the counts so far after each step the
    /// agent answered. Setting `cancel` ends the removal between two steps:
    /// nothing more is removed, the agent is told that the removal ended, and
    /// the report says what was removed until then.
    ///
    /// # Errors
    ///
    /// `Err` only when the session could not carry the removal (not active,
    /// revoked, disconnected), the path is invalid, or the agent's answer
    /// contradicts itself. A step the agent refused — the folder does not
    /// exist, it is no longer the folder the removal began with, the agent
    /// does not serve the operation — is a `Failed` [`TreeRemovalReport`]
    /// with the refusal in `error`, and so is a removal that did not end
    /// within [`MAX_TREE_REMOVAL_STEPS`] steps.
    pub async fn remove_tree(
        &self,
        path: RemotePath,
        cancel: &CancelToken,
        on_progress: impl FnMut(TreeRemovalProgress),
    ) -> Result<TreeRemovalReport, SessionError> {
        self.remove_tree_as(path, Origin::Console, cancel, on_progress)
            .await
    }

    /// Like [`Self::remove_tree`], but stamps an explicit [`Attribution`] onto
    /// every request the removal issues, so the agent attributes it to its
    /// real origin.
    ///
    /// # Errors
    ///
    /// See [`Self::remove_tree`].
    pub async fn remove_tree_as(
        &self,
        path: RemotePath,
        attribution: impl Into<Attribution>,
        cancel: &CancelToken,
        on_progress: impl FnMut(TreeRemovalProgress),
    ) -> Result<TreeRemovalReport, SessionError> {
        let by = attribution.into();
        run_removal(
            |step| self.file_op_as(step, by.clone()),
            path,
            cancel,
            on_progress,
        )
        .await
    }
}

/// Runs one folder removal against `ask`, which sends one request to the
/// agent and returns its answer.
///
/// Every step names the folder by the identity the step before it answered
/// with, so the removal stays with the folder it began with. The removal
/// asks for at most [`MAX_TREE_REMOVAL_STEPS`] steps.
async fn run_removal<Ask, Answer>(
    mut ask: Ask,
    path: RemotePath,
    cancel: &CancelToken,
    mut on_progress: impl FnMut(TreeRemovalProgress),
) -> Result<TreeRemovalReport, SessionError>
where
    Ask: FnMut(Operation) -> Answer,
    Answer: Future<Output = Result<OpResult, SessionError>>,
{
    let mut report = TreeRemovalReport::started();
    // The folder the agent said it is removing, once a step has answered with
    // "more": the agent then shows the removal as in progress, and must be
    // told if it ends here.
    let mut folder: Option<B64Bytes> = None;
    let mut steps: u64 = 0;
    loop {
        if cancel.is_cancelled() {
            if folder.is_some() {
                end_removal(&mut ask, &path).await;
            }
            return Ok(report.ended(JobOutcome::Cancelled));
        }
        if steps >= MAX_TREE_REMOVAL_STEPS {
            // Every step left more to do, and something keeps it that way.
            // The agent shows the removal as in progress: it is told that it
            // ended. Never a success: the folder is still there.
            end_removal(&mut ask, &path).await;
            report.error = Some(TreeRemovalError {
                code: ErrorCode::LimitExceeded,
                message: format!(
                    "the folder was not removed within {MAX_TREE_REMOVAL_STEPS} steps"
                ),
            });
            return Ok(report.ended(JobOutcome::Failed));
        }
        steps += 1;
        let step = ask(Operation::RemoveTree {
            path: path.clone(),
            stop: false,
            folder_id: folder.clone(),
        })
        .await;
        let (state, failed, failed_more) = match step {
            Ok(OpResult::TreeRemoval {
                state,
                removed_files,
                removed_dirs,
                failed,
                failed_more,
                folder_id,
            }) => {
                if state == TreeState::More {
                    // A step that says more remains must have removed
                    // something — that is what makes the removal end — and
                    // must name the folder the next step goes on with.
                    if removed_files + removed_dirs == 0 || folder_id.is_none() {
                        return Err(SessionError::Unexpected {
                            context: "folder removal progress",
                        });
                    }
                    folder = folder_id;
                }
                report.removed_files = report.removed_files.saturating_add(removed_files);
                report.removed_dirs = report.removed_dirs.saturating_add(removed_dirs);
                on_progress(TreeRemovalProgress {
                    removed_files: report.removed_files,
                    removed_dirs: report.removed_dirs,
                });
                (state, failed, failed_more)
            }
            Ok(_) => {
                return Err(SessionError::Unexpected {
                    context: "folder removal",
                });
            }
            Err(SessionError::Remote { code, message }) => {
                report.error = Some(TreeRemovalError { code, message });
                return Ok(report.ended(JobOutcome::Failed));
            }
            Err(other) => return Err(other),
        };
        let nothing_failed = failed.is_empty() && failed_more == 0;
        match state {
            TreeState::More => {}
            TreeState::Removed if nothing_failed => {
                return Ok(report.ended(JobOutcome::Completed));
            }
            TreeState::Incomplete | TreeState::Blocked if !failed.is_empty() => {
                report.failed = failed;
                report.failed_more = failed_more;
                report.blocked = state == TreeState::Blocked;
                return Ok(report.ended(JobOutcome::Failed));
            }
            // A folder that is gone and yet has failures, one that
            // remains without a reason, or a step that stopped unasked.
            TreeState::Removed
            | TreeState::Incomplete
            | TreeState::Blocked
            | TreeState::Stopped => {
                return Err(SessionError::Unexpected {
                    context: "folder removal result",
                });
            }
        }
    }
}

/// Tells the agent that a removal it shows as in progress ended here, on the
/// caller's side. Best effort: the removal ends whether or not the agent can
/// still be told — a session that ended in the meantime ends the agent's view
/// of it too.
async fn end_removal<Ask, Answer>(ask: &mut Ask, path: &RemotePath)
where
    Ask: FnMut(Operation) -> Answer,
    Answer: Future<Output = Result<OpResult, SessionError>>,
{
    let _ = ask(Operation::RemoveTree {
        path: path.clone(),
        stop: true,
        folder_id: None,
    })
    .await;
}

#[cfg(test)]
mod tests;
