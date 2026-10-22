//! Console-side file-transfer engine (ST-M003-005, ADR-0012).
//!
//! [`HostHandle::upload_file`] and [`HostHandle::download_file`] compose the
//! typed file-operation primitive ([`HostHandle::file_op`]) and its non-blocking
//! sibling `submit_op` into resumable, verified, cancellable transfers:
//!
//! - **Pipelining (AC1).** Chunk operations are kept in flight up to
//!   [`MAX_INFLIGHT_FILE_OPS`] and awaited FIFO, so the console gets genuine
//!   backpressure without buffering the whole file (REQ-N-006). Progress,
//!   cancellation, and the terminal outcome are reported in the shared
//!   [`JobOutcome`] vocabulary.
//! - **Structural verification (AC2).** An upload finishes only through
//!   `FsCommit`, which the agent verifies (length **and** SHA-256) before the
//!   atomic rename, so an unverified completion is unrepresentable (REQ-N-007).
//!   A download verifies the whole-file digest (`FsDigest`) against the rolling
//!   console hash before renaming the local temp into place.
//! - **Resume (AC3).** A download resumes from a local temp only after its
//!   prefix `FsDigest` matches the remote source, and restarts on a mismatch. An
//!   upload resumes from the agent's `FsStat.partial_len`; because a pending part
//!   is not addressable for a prefix digest, the whole-file commit check is the
//!   guard — a resumed prefix that turns out not to be ours fails the commit with
//!   [`ErrorCode::IntegrityMismatch`], which triggers exactly one audited
//!   abort-and-restart from zero rather than a silent bad publish.
//!
//! Cancellation and remote failures are honest: a cancelled upload issues an
//! idempotent `FsAbort`, a cancelled download keeps its temp for a later resume,
//! and a genuine failure never reads as success.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cretlink_protocol::limits::{MAX_CHUNK_BYTES, MAX_INFLIGHT_FILE_OPS, MAX_TRANSFER_BYTES};
use cretlink_protocol::{
    B64Bytes, CleanupPolicy, DigestAlgo, ErrorCode, OpResult, Operation, RemotePath,
};
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::oneshot;

use crate::{Attribution, HostHandle, JobOutcome, Origin, SessionError};

/// The most raw bytes one file-op message may carry over the secure channel.
///
/// A single Noise transport record holds at most 65519 plaintext bytes and the
/// channel fails closed rather than fragment an oversized record (record-layer
/// chunking is future work). Base64 inflates payload bytes by 4/3 and the JSON
/// envelope adds a little more, so a full [`MAX_CHUNK_BYTES`] chunk cannot fit;
/// 32 KiB leaves a wide margin for the envelope, a long staging name, and a
/// large request id while still filling the pipelining window.
const WIRE_CHUNK_BYTES: usize = 32 * 1024;

// The wire chunk must never exceed the protocol's per-operation payload cap.
const _: () = assert!(WIRE_CHUNK_BYTES <= MAX_CHUNK_BYTES);

/// The reply channel one queued file operation resolves on.
type Reply = oneshot::Receiver<Result<OpResult, SessionError>>;

/// A cooperative cancellation flag shared with an in-flight transfer.
///
/// Cloning shares the same flag: hold one clone and pass another to
/// [`HostHandle::upload_file`]/[`HostHandle::download_file`]. Setting it from any
/// clone makes the running transfer stop issuing new chunks, deal with the
/// partial artifact honestly, and report [`JobOutcome::Cancelled`].
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A fresh, un-cancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation. Idempotent and safe to call from any clone.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// A progress snapshot delivered to a transfer's `on_progress` callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferProgress {
    /// Bytes confirmed transferred so far, including any resumed prefix.
    pub transferred: u64,
    /// Total bytes in the artifact.
    pub total: u64,
}

/// The agent-reported error that ended a failed transfer (secret-free).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferError {
    /// The machine-readable category the agent returned.
    pub code: ErrorCode,
    /// The agent's human-readable, secret-free description.
    pub message: String,
    /// The transfer stage the failure occurred in (`stat`, `write`, `read`,
    /// `commit`, `digest`, or `verify`).
    pub stage: &'static str,
}

/// The honest terminal report of a transfer (ST-M003-005 AC1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferReport {
    /// Terminal outcome: `Completed`, `Failed`, or `Cancelled`.
    pub outcome: JobOutcome,
    /// Total bytes in the artifact.
    pub total: u64,
    /// Bytes confirmed transferred **this run** (excludes a resumed prefix).
    pub sent: u64,
    /// The verified SHA-256 digest on `Completed`; empty otherwise.
    pub digest: Vec<u8>,
    /// For a completed upload, the agent's resolved absolute committed path.
    pub canonical_path: Option<String>,
    /// Bytes already present that a verified resume reused.
    pub resumed_from: u64,
    /// Whether a mismatch forced an abort-and-restart from zero.
    pub restarted: bool,
    /// The agent error that ended a `Failed` transfer, if any.
    pub error: Option<TransferError>,
}

/// A request to upload a local file to the target and commit it (ST-M003-005).
#[derive(Debug, Clone)]
pub struct UploadRequest {
    /// The local file to read.
    pub source: PathBuf,
    /// The eventual destination on the target (staging or an absolute path).
    pub dest: RemotePath,
    /// The technician-side origin label bound into the staging manifest.
    /// **Required** for a [`RemotePath::Staging`] destination (REQ-S-017).
    pub source_label: String,
    /// The per-artifact cleanup policy recorded at commit.
    pub cleanup: CleanupPolicy,
    /// Whether an existing destination may be replaced; otherwise a present
    /// destination fails the transfer with [`ErrorCode::AlreadyExists`].
    pub overwrite: bool,
}

/// A request to download a target file to a local path (ST-M003-005).
#[derive(Debug, Clone)]
pub struct DownloadRequest {
    /// The source file on the target.
    pub source: RemotePath,
    /// The local path to create. Bytes land in a sibling temp file that is
    /// renamed into place only after the whole-file digest verifies.
    pub dest: PathBuf,
}

/// One outstanding upload write, kept in the in-flight window.
#[derive(Debug)]
struct InFlight {
    /// Bytes this write appended, credited to progress once acknowledged.
    bytes: u64,
    /// The reply channel for this write's `WriteAccepted`.
    rx: Reply,
}

/// The result of awaiting one pipelined upload write.
enum WriteStep {
    /// The agent accepted the write; the given bytes are now on the target.
    Accepted(u64),
    /// The agent refused the write; fold this into a `Failed` report.
    Failed(TransferError),
}

/// Where an upload should begin, decided by [`HostHandle::upload_resume_point`].
enum UploadStart {
    /// Begin at `start`; `restarted` records an abort-and-restart from zero.
    At { start: u64, restarted: bool },
    /// The transfer cannot proceed; return this report unchanged.
    Fail(TransferReport),
}

/// The result of streaming an upload's tail, before the commit decision.
enum TailOutcome {
    /// The whole source was streamed; carries the bytes sent and the full digest.
    Streamed { sent: u64, digest: Vec<u8> },
    /// A write was refused mid-stream; the part is left for a later resume.
    WriteFailed { sent: u64, error: TransferError },
    /// Cancellation stopped the stream after `sent` bytes.
    Cancelled { sent: u64 },
}

impl HostHandle {
    /// Uploads `request.source` to the target and commits it, returning an honest
    /// [`TransferReport`]. Pipelines writes with a bounded window, resumes a
    /// pending part, and finishes only through the agent's verified `FsCommit`
    /// (ST-M003-005 AC1-3).
    ///
    /// `on_progress` is called with a [`TransferProgress`] as each write is
    /// acknowledged. Returns `Err` only when the transfer could not run (session
    /// not active, disconnected, or a local file error); an operation the agent
    /// refused is reported as a `Failed` [`TransferReport`].
    pub async fn upload_file(
        &self,
        request: UploadRequest,
        cancel: &CancelToken,
        on_progress: impl FnMut(TransferProgress),
    ) -> Result<TransferReport, SessionError> {
        self.upload_file_as(request, Origin::Console, cancel, on_progress)
            .await
    }

    /// Like [`Self::upload_file`], but stamps an explicit [`Attribution`] onto
    /// every request the transfer issues, so the agent attributes an AI-driven
    /// upload to the AI and shows the caller's stated purpose beside the commit
    /// (ST-M009-007, REQ-F-026).
    ///
    /// # Errors
    ///
    /// See [`Self::upload_file`].
    pub async fn upload_file_as(
        &self,
        request: UploadRequest,
        attribution: impl Into<Attribution>,
        cancel: &CancelToken,
        mut on_progress: impl FnMut(TransferProgress),
    ) -> Result<TransferReport, SessionError> {
        let attribution = attribution.into();
        let by = &attribution;
        let UploadRequest {
            source,
            dest,
            source_label,
            cleanup,
            overwrite,
        } = request;

        // A staging destination MUST carry a non-empty source label (REQ-S-017).
        let label = (!source_label.is_empty()).then_some(source_label);
        if matches!(dest, RemotePath::Staging { .. }) && label.is_none() {
            return Err(SessionError::Invalid {
                // The same category the agent's own commit check answers with.
                code: cretlink_protocol::ErrorCode::Unsupported,
                message: "a staging upload requires a non-empty source label".to_string(),
            });
        }

        let total = fs::metadata(&source).await.map_err(SessionError::Io)?.len();
        if total > MAX_TRANSFER_BYTES {
            return Err(SessionError::Invalid {
                code: cretlink_protocol::ErrorCode::LimitExceeded,
                message: format!(
                    "source is {total} bytes, over the {MAX_TRANSFER_BYTES}-byte transfer cap"
                ),
            });
        }

        // Cancelled before anything is staged: abort idempotently and report.
        if cancel.is_cancelled() {
            self.abort_part(&dest, by).await?;
            return Ok(cancelled_report(total, 0, 0, false));
        }

        let (mut start, mut restarted) = match self
            .upload_resume_point(&dest, total, overwrite, by)
            .await?
        {
            UploadStart::Fail(report) => return Ok(report),
            UploadStart::At { start, restarted } => (start, restarted),
        };

        // Stream, then commit. A resumed prefix that turns out not to be ours
        // fails the commit's whole-file digest check; that triggers exactly one
        // audited abort-and-restart from zero (AC3).
        loop {
            let resumed_from = start;
            let (sent, digest) = match self
                .stream_upload_tail(&source, &dest, total, start, cancel, by, &mut on_progress)
                .await?
            {
                TailOutcome::Cancelled { sent } => {
                    self.abort_part(&dest, by).await?;
                    return Ok(cancelled_report(total, sent, resumed_from, restarted));
                }
                TailOutcome::WriteFailed { sent, error } => {
                    // Leave the part in place so a later upload can resume it.
                    return Ok(failed_transfer(total, sent, resumed_from, restarted, error));
                }
                TailOutcome::Streamed { sent, digest } => (sent, digest),
            };

            match self
                .file_op_as(
                    Operation::FsCommit {
                        dest: dest.clone(),
                        len: total,
                        algo: DigestAlgo::Sha256,
                        digest: B64Bytes::new(digest.clone()),
                        overwrite,
                        source: label.clone(),
                        cleanup,
                    },
                    by.clone(),
                )
                .await
            {
                Ok(OpResult::Committed {
                    canonical_path,
                    len,
                    digest: got,
                    ..
                }) => {
                    if len != total || got.as_bytes() != digest.as_slice() {
                        return Err(SessionError::Unexpected {
                            context: "upload commit verification",
                        });
                    }
                    return Ok(TransferReport {
                        outcome: JobOutcome::Completed,
                        total,
                        sent,
                        digest,
                        canonical_path: Some(canonical_path),
                        resumed_from,
                        restarted,
                        error: None,
                    });
                }
                Ok(_) => {
                    return Err(SessionError::Unexpected {
                        context: "upload commit",
                    });
                }
                Err(SessionError::Remote { code, .. })
                    if code == ErrorCode::IntegrityMismatch && start > 0 && !restarted =>
                {
                    // The resumed prefix was not ours: discard it and resend whole.
                    self.abort_part(&dest, by).await?;
                    start = 0;
                    restarted = true;
                }
                Err(SessionError::Remote { code, message }) => {
                    return Ok(failed_transfer(
                        total,
                        sent,
                        resumed_from,
                        restarted,
                        TransferError {
                            code,
                            message,
                            stage: "commit",
                        },
                    ));
                }
                Err(other) => return Err(other),
            }
        }
    }

    /// Downloads `request.source` from the target to `request.dest`, returning an
    /// honest [`TransferReport`]. Pipelines reads with a bounded window, resumes a
    /// matching local temp, verifies the whole-file digest against the rolling
    /// console hash, and only then renames the temp into place (AC1-3).
    pub async fn download_file(
        &self,
        request: DownloadRequest,
        cancel: &CancelToken,
        on_progress: impl FnMut(TransferProgress),
    ) -> Result<TransferReport, SessionError> {
        self.download_file_as(request, Origin::Console, cancel, on_progress)
            .await
    }

    /// Like [`Self::download_file`], but stamps an explicit [`Attribution`] onto
    /// every request the transfer issues (ST-M009-007, REQ-F-026).
    ///
    /// # Errors
    ///
    /// See [`Self::download_file`].
    pub async fn download_file_as(
        &self,
        request: DownloadRequest,
        attribution: impl Into<Attribution>,
        cancel: &CancelToken,
        mut on_progress: impl FnMut(TransferProgress),
    ) -> Result<TransferReport, SessionError> {
        let attribution = attribution.into();
        let by = &attribution;
        let DownloadRequest { source, dest } = request;
        let temp = download_temp_path(&dest);

        // Total + existence from a single stat (never errors on absence).
        let total = match self
            .file_op_as(
                Operation::FsStat {
                    path: source.clone(),
                },
                by.clone(),
            )
            .await
        {
            Ok(OpResult::PathStat {
                entry: Some(entry), ..
            }) => entry.len,
            Ok(OpResult::PathStat { entry: None, .. }) => {
                return Ok(failed_report(
                    0,
                    TransferError {
                        code: ErrorCode::PathNotFound,
                        message: "source does not exist".to_string(),
                        stage: "stat",
                    },
                ));
            }
            Ok(_) => {
                return Err(SessionError::Unexpected {
                    context: "download stat",
                });
            }
            Err(SessionError::Remote { code, message }) => {
                return Ok(failed_report(
                    0,
                    TransferError {
                        code,
                        message,
                        stage: "stat",
                    },
                ));
            }
            Err(other) => return Err(other),
        };

        if cancel.is_cancelled() {
            return Ok(cancelled_report(total, 0, 0, false));
        }

        // --- Resume from a matching local temp (AC3) ---
        let mut hasher = Sha256::new();
        let (mut start, mut restarted) = (0u64, false);
        if let Ok(meta) = fs::metadata(&temp).await {
            let have = meta.len();
            if have > 0 && have <= total {
                feed_prefix(&mut hasher, &temp, have)
                    .await
                    .map_err(SessionError::Io)?;
                let local = hasher.clone().finalize().to_vec();
                let remote = self.prefix_digest(&source, have, by).await?;
                if remote == local {
                    start = have;
                } else {
                    hasher = Sha256::new();
                    restarted = true;
                }
            } else if have > total {
                restarted = true;
            }
        }

        let mut temp_file = open_temp(&temp, start).await.map_err(SessionError::Io)?;
        let mut transferred = start;
        let mut next = start; // next offset to request
        let mut inflight: VecDeque<Reply> = VecDeque::new();
        let mut failure: Option<TransferError> = None;
        let mut cancelled = false;

        on_progress(TransferProgress { transferred, total });

        loop {
            while inflight.len() < MAX_INFLIGHT_FILE_OPS && next < total {
                if cancel.is_cancelled() {
                    cancelled = true;
                    break;
                }
                let want = (total - next).min(WIRE_CHUNK_BYTES as u64) as u32;
                let rx = self.submit_op(
                    Operation::FsRead {
                        path: source.clone(),
                        offset: next,
                        len: want,
                    },
                    by.clone(),
                )?;
                inflight.push_back(rx);
                next += u64::from(want);
            }
            if cancelled {
                break;
            }
            let Some(rx) = inflight.pop_front() else {
                break; // window empty and nothing left to request: done
            };
            match recv(rx).await {
                Ok(OpResult::FileData { data, .. }) => {
                    let bytes = data.as_bytes();
                    hasher.update(bytes);
                    temp_file.write_all(bytes).await.map_err(SessionError::Io)?;
                    transferred += bytes.len() as u64;
                    on_progress(TransferProgress { transferred, total });
                }
                Ok(_) => {
                    let _ = fs::remove_file(&temp).await;
                    return Err(SessionError::Unexpected {
                        context: "download read",
                    });
                }
                Err(SessionError::Remote { code, message }) => {
                    failure = Some(TransferError {
                        code,
                        message,
                        stage: "read",
                    });
                    break;
                }
                Err(other) => {
                    let _ = fs::remove_file(&temp).await;
                    return Err(other);
                }
            }
        }
        inflight.clear();

        if cancelled {
            // Keep the temp so a later download can resume from it.
            temp_file.flush().await.map_err(SessionError::Io)?;
            return Ok(cancelled_report(
                total,
                transferred - start,
                start,
                restarted,
            ));
        }
        if let Some(error) = failure {
            drop(temp_file);
            let _ = fs::remove_file(&temp).await;
            return Ok(failed_transfer(
                total,
                transferred - start,
                start,
                restarted,
                error,
            ));
        }

        // --- Verify the whole-file digest before renaming into place (AC2) ---
        let local_digest = hasher.finalize().to_vec();
        let remote_digest = match self
            .file_op_as(
                Operation::FsDigest {
                    path: source,
                    prefix_len: None,
                },
                by.clone(),
            )
            .await
        {
            Ok(OpResult::FileDigest { digest, .. }) => digest.into_inner(),
            Ok(_) => {
                let _ = fs::remove_file(&temp).await;
                return Err(SessionError::Unexpected {
                    context: "download digest",
                });
            }
            Err(SessionError::Remote { code, message }) => {
                let _ = fs::remove_file(&temp).await;
                return Ok(failed_transfer(
                    total,
                    transferred - start,
                    start,
                    restarted,
                    TransferError {
                        code,
                        message,
                        stage: "digest",
                    },
                ));
            }
            Err(other) => {
                let _ = fs::remove_file(&temp).await;
                return Err(other);
            }
        };

        if local_digest != remote_digest {
            let _ = fs::remove_file(&temp).await;
            return Ok(failed_transfer(
                total,
                transferred - start,
                start,
                restarted,
                TransferError {
                    code: ErrorCode::IntegrityMismatch,
                    message: "download digest did not match the source".to_string(),
                    stage: "verify",
                },
            ));
        }

        temp_file.sync_all().await.map_err(SessionError::Io)?;
        drop(temp_file);
        fs::rename(&temp, &dest).await.map_err(SessionError::Io)?;
        Ok(TransferReport {
            outcome: JobOutcome::Completed,
            total,
            sent: transferred - start,
            digest: local_digest,
            canonical_path: None,
            resumed_from: start,
            restarted,
            error: None,
        })
    }

    /// Streams the source tail from `start`, hashing the whole file for the
    /// commit and reporting progress per acknowledged write.
    // The attribution joins the transfer's existing inputs (ST-M009-007);
    // bundling them into a one-use struct would only rename the arguments.
    #[allow(clippy::too_many_arguments)]
    async fn stream_upload_tail<F: FnMut(TransferProgress)>(
        &self,
        source: &Path,
        dest: &RemotePath,
        total: u64,
        start: u64,
        cancel: &CancelToken,
        by: &Attribution,
        on_progress: &mut F,
    ) -> Result<TailOutcome, SessionError> {
        let mut file = fs::File::open(source).await.map_err(SessionError::Io)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; WIRE_CHUNK_BYTES];
        let mut pos: u64 = 0;
        let mut sent: u64 = 0;
        let mut transferred = start;
        let mut inflight: VecDeque<InFlight> = VecDeque::new();

        on_progress(TransferProgress { transferred, total });

        loop {
            let n = read_chunk(&mut file, &mut buf)
                .await
                .map_err(SessionError::Io)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            let chunk_start = pos;
            pos += n as u64;
            if pos <= start {
                continue; // wholly within the resumed prefix
            }
            if cancel.is_cancelled() {
                inflight.clear();
                return Ok(TailOutcome::Cancelled { sent });
            }

            // Send only the part of this chunk at or beyond `start`.
            let skip = start.saturating_sub(chunk_start) as usize;
            let wire_offset = chunk_start + skip as u64;
            let data = buf[skip..n].to_vec();
            let bytes = data.len() as u64;

            if inflight.len() >= MAX_INFLIGHT_FILE_OPS {
                match await_write(&mut inflight).await? {
                    WriteStep::Accepted(b) => {
                        transferred += b;
                        sent += b;
                        on_progress(TransferProgress { transferred, total });
                    }
                    WriteStep::Failed(error) => {
                        inflight.clear();
                        return Ok(TailOutcome::WriteFailed { sent, error });
                    }
                }
            }

            let rx = self.submit_op(
                Operation::FsWrite {
                    dest: dest.clone(),
                    offset: wire_offset,
                    data: B64Bytes::new(data),
                },
                by.clone(),
            )?;
            inflight.push_back(InFlight { bytes, rx });
        }

        while !inflight.is_empty() {
            match await_write(&mut inflight).await? {
                WriteStep::Accepted(b) => {
                    transferred += b;
                    sent += b;
                    on_progress(TransferProgress { transferred, total });
                }
                WriteStep::Failed(error) => {
                    inflight.clear();
                    return Ok(TailOutcome::WriteFailed { sent, error });
                }
            }
        }

        Ok(TailOutcome::Streamed {
            sent,
            digest: hasher.finalize().to_vec(),
        })
    }

    /// Decides where an upload should begin: a pending part is trusted as the
    /// resume point (the commit verifies it), a part longer than the source is
    /// aborted, and an existing final file with overwrite off is a `Failed`
    /// report (AC3).
    async fn upload_resume_point(
        &self,
        dest: &RemotePath,
        total: u64,
        overwrite: bool,
        by: &Attribution,
    ) -> Result<UploadStart, SessionError> {
        let (entry_exists, partial) = match self
            .file_op_as(Operation::FsStat { path: dest.clone() }, by.clone())
            .await
        {
            Ok(OpResult::PathStat { entry, partial_len }) => {
                (entry.is_some(), partial_len.unwrap_or(0))
            }
            Ok(_) => {
                return Err(SessionError::Unexpected {
                    context: "upload stat",
                });
            }
            Err(SessionError::Remote { code, message }) => {
                return Ok(UploadStart::Fail(failed_report(
                    total,
                    TransferError {
                        code,
                        message,
                        stage: "stat",
                    },
                )));
            }
            Err(other) => return Err(other),
        };

        if entry_exists && !overwrite {
            return Ok(UploadStart::Fail(failed_report(
                total,
                TransferError {
                    code: ErrorCode::AlreadyExists,
                    message: "destination already exists and overwrite is false".to_string(),
                    stage: "stat",
                },
            )));
        }

        if partial > total {
            // The part is longer than our source: it cannot be a prefix.
            self.abort_part(dest, by).await?;
            return Ok(UploadStart::At {
                start: 0,
                restarted: true,
            });
        }
        Ok(UploadStart::At {
            start: partial,
            restarted: false,
        })
    }

    /// Discards a pending part, best-effort and idempotent. A remote refusal is
    /// swallowed (the part may simply not exist); only fatal errors propagate.
    async fn abort_part(&self, dest: &RemotePath, by: &Attribution) -> Result<(), SessionError> {
        match self
            .file_op_as(Operation::FsAbort { dest: dest.clone() }, by.clone())
            .await
        {
            Ok(OpResult::Ack {}) => Ok(()),
            Ok(_) => Err(SessionError::Unexpected { context: "abort" }),
            Err(SessionError::Remote { .. }) => Ok(()),
            Err(other) => Err(other),
        }
    }

    /// The agent's digest of the first `prefix_len` bytes of `path`.
    async fn prefix_digest(
        &self,
        path: &RemotePath,
        prefix_len: u64,
        by: &Attribution,
    ) -> Result<Vec<u8>, SessionError> {
        match self
            .file_op_as(
                Operation::FsDigest {
                    path: path.clone(),
                    prefix_len: Some(prefix_len),
                },
                by.clone(),
            )
            .await?
        {
            OpResult::FileDigest { digest, .. } => Ok(digest.into_inner()),
            _ => Err(SessionError::Unexpected {
                context: "prefix digest",
            }),
        }
    }
}

/// Awaits the oldest in-flight write, crediting accepted bytes or folding a
/// remote refusal into a [`WriteStep::Failed`]. A fatal channel error propagates.
async fn await_write(inflight: &mut VecDeque<InFlight>) -> Result<WriteStep, SessionError> {
    let Some(front) = inflight.pop_front() else {
        return Err(SessionError::Unexpected {
            context: "empty write window",
        });
    };
    match recv(front.rx).await {
        Ok(OpResult::WriteAccepted { .. }) => Ok(WriteStep::Accepted(front.bytes)),
        Ok(_) => Err(SessionError::Unexpected {
            context: "write reply",
        }),
        Err(SessionError::Remote { code, message }) => Ok(WriteStep::Failed(TransferError {
            code,
            message,
            stage: "write",
        })),
        Err(other) => Err(other),
    }
}

/// Awaits one reply, mapping a dropped sender to an honest disconnect.
async fn recv(rx: Reply) -> Result<OpResult, SessionError> {
    match rx.await {
        Ok(inner) => inner,
        Err(_) => Err(SessionError::Disconnected),
    }
}

/// Reads up to `buf.len()` bytes, coalescing short reads into a full chunk so the
/// wire carries [`WIRE_CHUNK_BYTES`] frames until the final partial one. Returns
/// the count read (0 at end of file).
async fn read_chunk(file: &mut fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = file.read(&mut buf[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// Feeds the first `len` bytes of `path` into `hasher`.
async fn feed_prefix(hasher: &mut Sha256, path: &Path, len: u64) -> std::io::Result<()> {
    let mut file = fs::File::open(path).await?;
    let mut buf = vec![0u8; WIRE_CHUNK_BYTES];
    let mut remaining = len;
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let n = file.read(&mut buf[..want]).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        remaining -= n as u64;
    }
    Ok(())
}

/// Opens the download temp for writing at `start`: truncated fresh when
/// `start == 0`, or opened and positioned just past the verified prefix (with
/// any bytes beyond it dropped) when resuming.
async fn open_temp(path: &Path, start: u64) -> std::io::Result<fs::File> {
    if start == 0 {
        return fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .await;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .read(true)
        .open(path)
        .await?;
    file.set_len(start).await?;
    file.seek(std::io::SeekFrom::Start(start)).await?;
    Ok(file)
}

/// The sibling temp path a download streams into before the verified rename.
fn download_temp_path(dest: &Path) -> PathBuf {
    let mut name = dest
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(".cretlink-part");
    match dest.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(name),
        _ => PathBuf::from(name),
    }
}

/// A `Cancelled` report with no committed artifact.
fn cancelled_report(total: u64, sent: u64, resumed_from: u64, restarted: bool) -> TransferReport {
    TransferReport {
        outcome: JobOutcome::Cancelled,
        total,
        sent,
        digest: Vec::new(),
        canonical_path: None,
        resumed_from,
        restarted,
        error: None,
    }
}

/// A `Failed` report carrying the agent error that ended the transfer.
fn failed_transfer(
    total: u64,
    sent: u64,
    resumed_from: u64,
    restarted: bool,
    error: TransferError,
) -> TransferReport {
    TransferReport {
        outcome: JobOutcome::Failed,
        total,
        sent,
        digest: Vec::new(),
        canonical_path: None,
        resumed_from,
        restarted,
        error: Some(error),
    }
}

/// A `Failed` report for a failure before any bytes were sent.
fn failed_report(total: u64, error: TransferError) -> TransferReport {
    failed_transfer(total, 0, 0, false, error)
}
