//! The customer computer as the explorer's transfers and changes reach it
//! (ST-M010-002).
//!
//! [`FileTarget`] is what the transfer queue and the changes ask of the
//! customer computer: list a folder, send a file into one, copy a file here,
//! make a folder, rename, delete, remove a folder with its content, and look
//! whether an unfinished part of a transfer is still there. [`LiveFiles`] is
//! the one real implementation: every call goes through
//! [`cretlink_console::TechnicianFiles`], so it is the technician's, runs on
//! the consented session only, and is recorded in the audit trail before and
//! after it. The trait exists so that the queue can be tested against a
//! customer computer held in memory: the wire accepts a path only when it is
//! rooted on a drive, which a test off Windows cannot serve.
//!
//! Every path here is a path on the customer computer. None is ever opened
//! on this computer.

use std::future::Future;
use std::path::PathBuf;

use cretlink_console::{
    ChannelAudit, ConsoleSessionService, FolderEvent, ListingEnd, ProgressSink, SendToFolder,
    TechnicianFiles,
};
use cretlink_mcp::{DownloadReport, TargetError, UploadReport};
use cretlink_protocol::{ErrorStage, FileEntry};
use cretlink_session::{
    CancelToken, RemotePath, TransferProgress, TreeRemovalProgress, TreeRemovalReport,
    activity::error_text,
};

/// A folder of the customer computer as one listing gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listed {
    /// The entries, in the order the customer computer lists them.
    pub(crate) entries: Vec<FileEntry>,
    /// Whether these are all entries of the folder. `false` when the listing
    /// stopped at the most entries the explorer lists.
    pub(crate) complete: bool,
}

/// What the explorer's transfers and changes ask of the customer computer.
/// See the module documentation.
pub(crate) trait FileTarget: Send + Sync + 'static {
    /// Lists the folder `folder` to its end, or to the explorer's bound.
    fn list(
        &self,
        folder: &str,
        cancel: &CancelToken,
    ) -> impl Future<Output = Result<Listed, TargetError>> + Send;

    /// Sends a file of this computer into a folder.
    fn send(
        &self,
        send: SendToFolder,
        cancel: CancelToken,
        progress: ProgressSink<TransferProgress>,
    ) -> impl Future<Output = Result<UploadReport, TargetError>> + Send;

    /// Copies the file `source` to `dest` on this computer.
    fn download(
        &self,
        source: &str,
        dest: PathBuf,
        cancel: CancelToken,
        progress: ProgressSink<TransferProgress>,
    ) -> impl Future<Output = Result<DownloadReport, TargetError>> + Send;

    /// Makes the folder `path`; the folder that holds it must be there.
    fn create_folder(&self, path: &str) -> impl Future<Output = Result<(), TargetError>> + Send;

    /// Renames `from` to `to`; a name that is taken is refused.
    fn rename(&self, from: &str, to: &str) -> impl Future<Output = Result<(), TargetError>> + Send;

    /// Deletes the file or the link `path`; a link as the link.
    fn delete(&self, path: &str) -> impl Future<Output = Result<(), TargetError>> + Send;

    /// Removes the folder `path` with everything in it.
    fn remove_tree(
        &self,
        path: &str,
        cancel: CancelToken,
        progress: ProgressSink<TreeRemovalProgress>,
    ) -> impl Future<Output = Result<TreeRemovalReport, TargetError>> + Send;

    /// Whether an unfinished part of a transfer to `path` is still there.
    fn part_left(&self, path: &str) -> impl Future<Output = Result<bool, TargetError>> + Send;
}

/// A path on the customer computer as the wire names it.
fn remote(path: &str) -> RemotePath {
    RemotePath::Absolute {
        path: path.to_owned(),
    }
}

/// The customer computer of the live session: the session's service and its
/// audit sink, as [`super::super::AppState::files`] hands them out while a
/// session is under full control.
#[derive(Debug, Clone)]
pub(crate) struct LiveFiles {
    service: ConsoleSessionService,
    audit: ChannelAudit,
}

impl LiveFiles {
    pub(crate) fn new((service, audit): (ConsoleSessionService, ChannelAudit)) -> Self {
        Self { service, audit }
    }

    fn files(&self) -> TechnicianFiles<'_> {
        TechnicianFiles::new(&self.service, &self.audit)
    }
}

impl FileTarget for LiveFiles {
    async fn list(&self, folder: &str, cancel: &CancelToken) -> Result<Listed, TargetError> {
        let mut entries = Vec::new();
        let report = {
            let mut sink = |event: FolderEvent| match event {
                FolderEvent::Opened { entries: page, .. }
                | FolderEvent::Page { entries: page, .. } => {
                    entries.extend(page);
                }
            };
            self.files()
                .list_folder(remote(folder), cancel.clone(), &mut sink)
                .await?
        };
        Ok(Listed {
            entries,
            complete: report.end == ListingEnd::Complete,
        })
    }

    async fn send(
        &self,
        send: SendToFolder,
        cancel: CancelToken,
        progress: ProgressSink<TransferProgress>,
    ) -> Result<UploadReport, TargetError> {
        self.files()
            .send_to_folder(send, cancel, Some(progress))
            .await
    }

    async fn download(
        &self,
        source: &str,
        dest: PathBuf,
        cancel: CancelToken,
        progress: ProgressSink<TransferProgress>,
    ) -> Result<DownloadReport, TargetError> {
        self.files()
            .download(remote(source), dest, cancel, Some(progress))
            .await
    }

    async fn create_folder(&self, path: &str) -> Result<(), TargetError> {
        self.files().create_folder(remote(path)).await
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), TargetError> {
        self.files().rename(remote(from), remote(to)).await
    }

    async fn delete(&self, path: &str) -> Result<(), TargetError> {
        self.files().delete(remote(path)).await
    }

    async fn remove_tree(
        &self,
        path: &str,
        cancel: CancelToken,
        progress: ProgressSink<TreeRemovalProgress>,
    ) -> Result<TreeRemovalReport, TargetError> {
        self.files()
            .remove_tree(remote(path), cancel, Some(progress))
            .await
    }

    async fn part_left(&self, path: &str) -> Result<bool, TargetError> {
        Ok(self
            .files()
            .look_up(remote(path))
            .await?
            .partial_len
            .is_some())
    }
}

/// How a session that no longer carries a request ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ended {
    /// No session is under full control: it was stopped or revoked.
    Stopped,
    /// The connection was lost before the answer.
    Lost,
}

impl Ended {
    /// The end of the session behind `error`, when that is what it says.
    pub(crate) fn of(error: &TargetError) -> Option<Self> {
        match error {
            TargetError::NoConsentedSession => Some(Self::Stopped),
            TargetError::Disconnected => Some(Self::Lost),
            TargetError::Remote { .. } | TargetError::Unexpected => None,
        }
    }

    /// What a transfer or a removal that the session's end stopped says.
    pub(crate) fn words(self) -> &'static str {
        match self {
            Self::Stopped => "The session ended before this was complete.",
            Self::Lost => {
                "The connection to the customer computer was lost before this was complete."
            }
        }
    }
}

/// Why a request failed, in the words the application uses for a failed
/// action: a refusal of the customer computer in the words the activity has
/// for its category, a refusal made on this computer in its own words, which
/// name what stood in the way.
pub(crate) fn failure_words(error: &TargetError) -> String {
    match error {
        TargetError::Remote {
            message,
            stage: ErrorStage::Validation,
            ..
        } => message.clone(),
        TargetError::Remote { code, .. } => error_text(*code).to_owned(),
        TargetError::NoConsentedSession => "no session is under full control".to_owned(),
        TargetError::Disconnected => {
            "the connection to the customer computer was lost before it answered".to_owned()
        }
        TargetError::Unexpected => {
            "the customer computer gave an answer that CretLink did not expect".to_owned()
        }
    }
}
