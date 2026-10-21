//! The typed audit writer and the chain verifier (ADR-0008).
//!
//! [`AuditWriter`] is the *only* thing that writes the `.jsonl`. It is
//! synchronous and never-lossy: each record is serialized to one line and
//! flushed before the call returns (REQ-S-017). There is no channel that could
//! drop, sample, or filter records. A write failure is surfaced as
//! [`AuditError`] — the writer never silently swallows it; the console decides
//! whether to continue the session without audit (an explicit technician
//! choice, per ADR-0008).

use std::collections::BTreeMap;
use std::fmt;
use std::io::{BufRead, Write};

use cretlink_protocol::SessionId;

use crate::audit::clock::Clock;
use crate::audit::record::{AUDIT_SCHEMA_VERSION, Actor, AuditRecord, Outcome, ZERO_PREV};
use crate::audit::sha256_hex;
use crate::audit::value::{AuditArgs, AuditValue};

/// A failure while writing or serializing an audit record.
///
/// The console treats this as a prominent, reportable condition: audit
/// completeness (REQ-S-017) means a write failure must never be hidden.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    /// The underlying sink (file or buffer) failed to accept the bytes.
    #[error("failed to write audit record to the sink")]
    Write(#[from] std::io::Error),
    /// The record could not be serialized to JSON. Should not happen for
    /// well-formed records, but is surfaced rather than panicked on.
    #[error("failed to serialize audit record")]
    Serialize(#[source] serde_json::Error),
}

/// Appends typed audit records to a sink as one JSON line each, maintaining the
/// SHA-256 hash chain.
///
/// Generic over the sink `W` so tests write to a `Vec<u8>` and production
/// writes to a `File`. The clock is injected (boxed) so the writer keeps a
/// single type parameter while still supporting deterministic tests.
///
/// Lifecycle: [`AuditWriter::new_session`] emits the header (seq 0, `prev` =
/// [`ZERO_PREV`]); [`AuditWriter::append`]/[`AuditWriter::append_with`] emit
/// events; [`AuditWriter::close`] emits the sealing record and must be called
/// explicitly (there is no `Drop` auto-close, to avoid writing during unwind or
/// hiding a final write error).
pub struct AuditWriter<W: Write> {
    sink: W,
    clock: Box<dyn Clock>,
    session: SessionId,
    /// The seq that the *next* appended record will carry.
    next_seq: u64,
    /// SHA-256 hex of the most recently written line (the next record's `prev`).
    prev: String,
    closed: bool,
}

impl<W: Write> fmt::Debug for AuditWriter<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Deliberately omit the sink and clock: the sink may hold captured
        // output and is not necessarily `Debug`.
        f.debug_struct("AuditWriter")
            .field("session", &self.session)
            .field("next_seq", &self.next_seq)
            .field("prev", &self.prev)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl<W: Write> AuditWriter<W> {
    /// Opens a new session log on `sink`, emitting the header record.
    ///
    /// The header is `actor = system`, `action = "session_opened"`, `seq = 0`,
    /// `prev = ZERO_PREV`, and records the schema version in its args. Returns
    /// the writer ready for [`AuditWriter::append`], or an [`AuditError`] if the
    /// header could not be written.
    pub fn new_session(
        sink: W,
        session: SessionId,
        clock: impl Clock + 'static,
    ) -> Result<Self, AuditError> {
        let mut writer = Self {
            sink,
            clock: Box::new(clock),
            session,
            next_seq: 0,
            prev: ZERO_PREV.to_string(),
            closed: false,
        };
        let header_args = AuditArgs::new().with(
            "schema_version",
            AuditValue::Int(i64::from(AUDIT_SCHEMA_VERSION)),
        );
        writer.emit(
            Actor::System,
            "session_opened",
            header_args,
            Outcome::Ok,
            BTreeMap::new(),
            None,
        )?;
        Ok(writer)
    }

    /// Appends an event record with no digests and no duration.
    ///
    /// The record's `seq`, `ts`, and `prev` are filled in automatically.
    pub fn append(
        &mut self,
        actor: Actor,
        action: impl Into<String>,
        args: AuditArgs,
        outcome: Outcome,
    ) -> Result<(), AuditError> {
        self.emit(actor, action, args, outcome, BTreeMap::new(), None)
    }

    /// Appends an event record with optional digests and duration.
    pub fn append_with(
        &mut self,
        actor: Actor,
        action: impl Into<String>,
        args: AuditArgs,
        outcome: Outcome,
        digests: BTreeMap<String, String>,
        duration_ms: Option<u64>,
    ) -> Result<(), AuditError> {
        self.emit(actor, action, args, outcome, digests, duration_ms)
    }

    /// Emits the sealing record and consumes the writer.
    ///
    /// The close record is `actor = system`, `action = "session_closed"`, and
    /// records the total record count (including itself) under the args key
    /// `record_count`. Because any prefix of a valid chain is itself a valid
    /// chain, plain [`verify_chain`] cannot notice a dropped tail; use
    /// [`verify_sealed`], which requires this trailing seal and checks the
    /// sealed count against the number of records verified, to detect a
    /// truncated tail.
    pub fn close(mut self) -> Result<(), AuditError> {
        // `next_seq` is the seq this close record will take; the total number
        // of records once it is written is `next_seq + 1` (records 0..=next_seq).
        let total = self.next_seq + 1;
        let args = AuditArgs::new().with(
            "record_count",
            AuditValue::Int(i64::try_from(total).unwrap_or(i64::MAX)),
        );
        self.emit(
            Actor::System,
            "session_closed",
            args,
            Outcome::Ok,
            BTreeMap::new(),
            None,
        )?;
        self.closed = true;
        Ok(())
    }

    /// The session this writer records for.
    #[must_use]
    pub fn session(&self) -> SessionId {
        self.session
    }

    /// Builds, serializes, writes, and flushes one record, then advances the
    /// chain state.
    fn emit(
        &mut self,
        actor: Actor,
        action: impl Into<String>,
        args: AuditArgs,
        outcome: Outcome,
        digests: BTreeMap<String, String>,
        duration_ms: Option<u64>,
    ) -> Result<(), AuditError> {
        let record = AuditRecord {
            v: AUDIT_SCHEMA_VERSION,
            seq: self.next_seq,
            ts: crate::audit::clock::format_rfc3339_millis(self.clock.now_unix_millis()),
            session: self.session,
            actor,
            action: action.into(),
            args,
            outcome,
            digests,
            duration_ms,
            prev: self.prev.clone(),
        };

        // Serialize to exactly one line: `serde_json` escapes control
        // characters, so the object never contains a raw newline.
        let line = serde_json::to_string(&record).map_err(AuditError::Serialize)?;

        // The chain hashes the exact serialized JSON bytes, excluding the
        // terminating newline. `verify_chain` reproduces this by hashing each
        // line's content without its `\n`.
        let this_hash = sha256_hex(line.as_bytes());

        self.sink.write_all(line.as_bytes())?;
        self.sink.write_all(b"\n")?;
        // Flush per record so a crash loses at most the current line
        // (REQ-S-017 never-lossy).
        self.sink.flush()?;

        self.prev = this_hash;
        self.next_seq += 1;
        Ok(())
    }
}

/// How a hash chain was found to be broken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainBreak {
    /// A line's `prev` did not match the hash of the preceding line.
    PrevMismatch {
        /// The hash the line should have carried.
        expected: String,
        /// The hash the line actually carried.
        found: String,
    },
    /// The header line's `prev` was not [`ZERO_PREV`].
    BadHeaderPrev {
        /// The non-zero value found on the header.
        found: String,
    },
    /// The line was not a well-formed audit record.
    Malformed {
        /// Parser detail.
        detail: String,
    },
    /// The `seq` was not the expected monotonic value.
    SeqGap {
        /// The seq that was expected.
        expected: u64,
        /// The seq that was found.
        found: u64,
    },
    /// The trailing `session_closed` seal's `record_count` did not equal the
    /// number of records verified — a truncated or forged tail. Reported only
    /// by [`verify_sealed`].
    CountMismatch {
        /// Number of records actually verified.
        expected: u64,
        /// The count the seal claimed, or `None` if it carried no
        /// `record_count`.
        found: Option<u64>,
    },
}

/// The result of verifying a chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainReport {
    /// Every line parsed and linked correctly. `records` counts the lines
    /// verified.
    Valid {
        /// Number of records verified.
        records: u64,
    },
    /// The chain broke; `line` is the 0-indexed line where it was first
    /// detected.
    Broken {
        /// 0-indexed line number of the first break.
        line: u64,
        /// Why the chain broke.
        reason: ChainBreak,
    },
    /// Every line linked correctly, but the log carries no trailing
    /// `session_closed` seal, so a dropped tail cannot be ruled out. Reported
    /// only by [`verify_sealed`]; a legitimately crashed session also lands
    /// here, so it is distinct from [`ChainReport::Broken`].
    Unsealed {
        /// Number of records verified.
        records: u64,
    },
}

/// The tail state a chain scan carries out of the loop for seal verification.
struct ChainScan {
    /// Number of records that linked correctly.
    records: u64,
    /// `action` of the last record seen (empty for a zero-record log).
    last_action: String,
    /// `record_count` argument of the last record, if it was an integer.
    last_record_count: Option<u64>,
}

/// Outcome of a single hash-chain pass over a reader.
enum ScanOutcome {
    /// Every line linked correctly; carries the tail state.
    Linked(ChainScan),
    /// A break was found at `line` (0-indexed).
    Broken {
        /// 0-indexed line number of the first break.
        line: u64,
        /// Why the chain broke.
        reason: ChainBreak,
    },
}

/// Recomputes the hash chain over `reader`, reporting the first broken line and
/// carrying out the tail state.
///
/// Each line is hashed over its exact bytes (excluding the terminating
/// newline), the same way [`AuditWriter`] wrote it. The header line must carry
/// [`ZERO_PREV`]; every subsequent line's `prev` must equal the hash of the
/// line before it, and `seq` must increase monotonically from 0. A blank line
/// is tolerated only as a single trailing newline at end of file; a blank line
/// with content after it is reported as [`ChainBreak::Malformed`] at its
/// physical position rather than silently skipped, so the reported break line
/// always maps to a real file line.
fn scan_chain<R: BufRead>(mut reader: R) -> Result<ScanOutcome, AuditError> {
    let mut prev = ZERO_PREV.to_string();
    let mut expected_seq: u64 = 0;
    let mut line_no: u64 = 0;
    let mut last_action = String::new();
    let mut last_record_count: Option<u64> = None;
    let mut buf: Vec<u8> = Vec::new();

    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        // Strip a single trailing newline; hash exactly what was written.
        let bytes: &[u8] = if buf.last() == Some(&b'\n') {
            &buf[..buf.len() - 1]
        } else {
            &buf[..]
        };
        if bytes.is_empty() {
            // A blank line is tolerated only as a single trailing newline at
            // end of file. If any bytes remain, the blank is interior
            // corruption, reported at this physical line.
            if reader.fill_buf()?.is_empty() {
                break;
            }
            return Ok(ScanOutcome::Broken {
                line: line_no,
                reason: ChainBreak::Malformed {
                    detail: "unexpected blank line before end of audit log".to_string(),
                },
            });
        }

        let this_hash = sha256_hex(bytes);

        let record: AuditRecord = match serde_json::from_slice(bytes) {
            Ok(record) => record,
            Err(err) => {
                return Ok(ScanOutcome::Broken {
                    line: line_no,
                    reason: ChainBreak::Malformed {
                        detail: err.to_string(),
                    },
                });
            }
        };

        if record.prev != prev {
            let reason = if line_no == 0 {
                ChainBreak::BadHeaderPrev { found: record.prev }
            } else {
                ChainBreak::PrevMismatch {
                    expected: prev,
                    found: record.prev,
                }
            };
            return Ok(ScanOutcome::Broken {
                line: line_no,
                reason,
            });
        }

        if record.seq != expected_seq {
            return Ok(ScanOutcome::Broken {
                line: line_no,
                reason: ChainBreak::SeqGap {
                    expected: expected_seq,
                    found: record.seq,
                },
            });
        }

        // Remember the tail so the seal (a `session_closed` with a
        // `record_count`) can be checked once EOF is reached.
        last_record_count = match record.args.0.get("record_count") {
            Some(AuditValue::Int(count)) => u64::try_from(*count).ok(),
            _ => None,
        };
        last_action = record.action;

        prev = this_hash;
        expected_seq += 1;
        line_no += 1;
    }

    Ok(ScanOutcome::Linked(ChainScan {
        records: line_no,
        last_action,
        last_record_count,
    }))
}

/// Recomputes the hash chain over `reader` and reports the first broken line
/// (ADR-0008).
///
/// The header line must carry [`ZERO_PREV`]; every subsequent line's `prev`
/// must equal the hash of the line before it, and `seq` must increase
/// monotonically from 0. A malformed line, a `prev` mismatch, or a `seq` gap
/// yields [`ChainReport::Broken`] at the first offending line. This is tamper
/// *evidence*, not tamper-proofing: an attacker who can rewrite the whole file
/// can also rebuild a valid chain.
///
/// This checks internal linkage only. Because any prefix of a valid chain is
/// itself a valid chain, it cannot detect a dropped tail — use
/// [`verify_sealed`] for that.
///
/// # Errors
///
/// Returns [`AuditError::Write`] if the underlying reader raises an I/O error.
pub fn verify_chain<R: BufRead>(reader: R) -> Result<ChainReport, AuditError> {
    Ok(match scan_chain(reader)? {
        ScanOutcome::Linked(scan) => ChainReport::Valid {
            records: scan.records,
        },
        ScanOutcome::Broken { line, reason } => ChainReport::Broken { line, reason },
    })
}

/// Verifies both the hash chain *and* the closing seal, so a truncated tail is
/// detected (ADR-0008).
///
/// After confirming the chain links correctly (as [`verify_chain`] does), the
/// last record must be `action = "session_closed"` and its `record_count`
/// argument must equal the number of records verified. A dropped tail leaves a
/// shorter — but internally consistent — chain whose last record is not the
/// seal, reported as [`ChainReport::Unsealed`]; a forged or truncated count is
/// reported as [`ChainBreak::CountMismatch`]. An unsealed result is expected
/// for a session that crashed before close, so callers distinguish it from a
/// hard break.
///
/// # Errors
///
/// Returns [`AuditError::Write`] if the underlying reader raises an I/O error.
pub fn verify_sealed<R: BufRead>(reader: R) -> Result<ChainReport, AuditError> {
    let scan = match scan_chain(reader)? {
        ScanOutcome::Linked(scan) => scan,
        ScanOutcome::Broken { line, reason } => {
            return Ok(ChainReport::Broken { line, reason });
        }
    };

    // No records, or a valid chain that does not end in the seal: the tail is
    // not proven complete.
    if scan.records == 0 || scan.last_action != "session_closed" {
        return Ok(ChainReport::Unsealed {
            records: scan.records,
        });
    }

    match scan.last_record_count {
        Some(count) if count == scan.records => Ok(ChainReport::Valid {
            records: scan.records,
        }),
        found => Ok(ChainReport::Broken {
            // The seal is the last record; its 0-indexed line is records - 1.
            line: scan.records.saturating_sub(1),
            reason: ChainBreak::CountMismatch {
                expected: scan.records,
                found,
            },
        }),
    }
}
