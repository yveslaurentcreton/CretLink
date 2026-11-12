//! The agent-owned, persistent staging manifest (ADR-0012, REQ-F-019).
//!
//! Every committed **staging** artifact is recorded here with its verified
//! length and digest, its technician-side source label, and its per-artifact
//! cleanup policy. The manifest is a small JSON document stored at the root of
//! the session staging directory so the record survives an agent restart: an
//! `OnShutdown` artifact can therefore still be swept the next time the agent
//! starts, and a `StagingCleanup` reports honest per-artifact outcomes
//! (REQ-N-008).
//!
//! The manifest is deliberately stateless in memory: each mutation reads the
//! file, changes it, and writes it back. A session serves its file operations
//! sequentially (see [`crate::FileStore`]), so there is no in-process race, and
//! nothing is cached that could drift from what is on disk.

use std::path::{Path, PathBuf};

use cretlink_protocol::{CleanupPolicy, CleanupTrigger, StagedEntry};
use serde::{Deserialize, Serialize};

/// The manifest file name, kept inside the session staging root.
pub(crate) const MANIFEST_FILE: &str = "manifest.json";

/// The largest manifest that is read. A manifest holds at most
/// [`cretlink_protocol::limits::MAX_DIR_PAGE`] records of bounded size, far
/// below this; a larger file is not one this program wrote, and reading it
/// whole would let whoever put it there use up the program's memory
/// (REQ-N-006, ST-M006-020).
const MAX_MANIFEST_BYTES: u64 = 2 * 1024 * 1024;

/// The on-disk manifest document. The `version` field lets a future agent
/// recognise and migrate an older layout rather than silently misread it.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    /// The manifest schema version (currently `1`).
    #[serde(default = "one")]
    version: u32,
    /// The recorded staged artifacts, in commit order.
    #[serde(default)]
    staged: Vec<StagedEntry>,
}

fn one() -> u32 {
    1
}

impl Manifest {
    /// Reads the manifest at `path`, returning an empty manifest when the file
    /// is absent. A corrupt manifest is surfaced as an error so the caller can
    /// fail closed rather than silently lose the record of what was staged.
    ///
    /// At most [`MAX_MANIFEST_BYTES`] are read; a larger file is an error.
    pub(crate) async fn load(path: &Path) -> std::io::Result<Self> {
        use tokio::io::AsyncReadExt;

        let file = match tokio::fs::File::open(path).await {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    version: 1,
                    staged: Vec::new(),
                });
            }
            Err(e) => return Err(e),
        };
        let mut bytes = Vec::new();
        file.take(MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the manifest is larger than a manifest can be",
            ));
        }
        serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Writes the manifest to `path` durably: the bytes are written to a
    /// sibling temp file, flushed, and renamed into place, so a crash mid-write
    /// never leaves a half-written manifest (the same integrity-by-construction
    /// discipline the artifacts themselves get).
    pub(crate) async fn store(&self, path: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = tmp_sibling(path);
        {
            use tokio::io::AsyncWriteExt;
            // Whatever stands under the temporary name is removed as what it
            // is, and the file is then made new: nothing is ever written
            // through a link or into a file that someone else put there
            // (ST-M006-020).
            let stale = tmp.clone();
            let cleared = tokio::task::spawn_blocking(move || crate::own::remove_whole(&stale))
                .await
                .map_err(std::io::Error::other)?;
            match cleared {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .await?;
            file.write_all(&bytes).await?;
            file.sync_all().await?;
        }
        tokio::fs::rename(&tmp, path).await
    }

    /// The recorded artifacts.
    pub(crate) fn entries(&self) -> &[StagedEntry] {
        &self.staged
    }

    /// The number of recorded artifacts.
    pub(crate) fn len(&self) -> usize {
        self.staged.len()
    }

    /// Records a freshly committed artifact, replacing any prior record for the
    /// same canonical path (an overwrite commit re-registers the same name).
    pub(crate) fn insert(&mut self, entry: StagedEntry) {
        self.staged
            .retain(|e| e.canonical_path != entry.canonical_path);
        self.staged.push(entry);
    }

    /// Removes the record for `canonical_path`, if present. Returns whether a
    /// record was removed.
    pub(crate) fn remove(&mut self, canonical_path: &str) -> bool {
        let before = self.staged.len();
        self.staged.retain(|e| e.canonical_path != canonical_path);
        self.staged.len() != before
    }

    /// The artifacts a cleanup `trigger` selects, by their per-artifact policy.
    ///
    /// `All` purges everything (including [`CleanupPolicy::Keep`]); `Shutdown`
    /// takes `OnShutdown` and `OnSessionEnd`; `SessionEnd` takes only
    /// `OnSessionEnd`. The returned paths are owned so the caller can delete and
    /// then re-borrow the manifest to drop the records.
    pub(crate) fn selected(&self, trigger: CleanupTrigger) -> Vec<String> {
        self.staged
            .iter()
            .filter(|e| policy_selected(e.cleanup, trigger))
            .map(|e| e.canonical_path.clone())
            .collect()
    }
}

/// Whether a cleanup `trigger` removes an artifact with the given `policy`.
fn policy_selected(policy: CleanupPolicy, trigger: CleanupTrigger) -> bool {
    match trigger {
        CleanupTrigger::All => true,
        CleanupTrigger::Shutdown => {
            matches!(
                policy,
                CleanupPolicy::OnShutdown | CleanupPolicy::OnSessionEnd
            )
        }
        CleanupTrigger::SessionEnd => matches!(policy, CleanupPolicy::OnSessionEnd),
    }
}

/// A hidden temp sibling path for a durable write of `path`.
fn tmp_sibling(path: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(
        path.file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("manifest")),
    );
    name.push(".tmp");
    match path.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}
