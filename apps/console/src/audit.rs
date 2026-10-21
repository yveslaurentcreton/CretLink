//! The console's unified per-session audit service (ST-M003-006, ADR-0008).
//!
//! One drain owns the single-threaded, non-`Send` `cretlink-shared`
//! [`AuditWriter`] and multiplexes **three** producers over one bounded
//! channel (REQ-N-006):
//!
//! - session lifecycle events from the runtime's `SessionAudit` hooks
//!   (pairing, consent, cleanup) — the console hosts with
//!   `SessionHost::host_with_audit` and passes the same [`ChannelAudit`];
//! - MCP bridge calls recorded by the IPC server ([`crate::IpcServer`],
//!   actor `mcp`), each as a capture-time start record plus a completion
//!   record sharing a console-minted `call_id` (ADR-0008 Appendix A);
//! - manual console commands from the interactive CLI loop (actor `console`,
//!   tracked by [`ManualActivity`]).
//!
//! Attribution always comes from the producing site, never from the agent
//! side. All three land in the **same** hash-chained `.jsonl` file, appended
//! and flushed per record — with an `fsync` at the ADR-0008 consent, stop,
//! cleanup, and close boundaries; when every sender is gone the log is sealed
//! so a truncated tail is detectable (`verify_sealed`).
//!
//! # No audit without an explicit choice (ADR-0008)
//!
//! ADR-0008 is explicit: *"continuing a session without audit requires an
//! explicit technician choice."* The console therefore opens the session log
//! **before control begins** ([`open_session_log`], called when the session id
//! is adopted, ahead of consent); if that fails, the console binary asks the
//! technician explicitly and defaults to failing the session closed — remote
//! control is never granted silently without a durable record. A degradation
//! that appears *mid-session* (a failing append, a stalling disk) cannot be
//! un-granted retroactively; it is surfaced with a prominent banner, counted,
//! and recorded as a final `audit_degraded` record.
//!
//! # Honest limits
//!
//! - Producers **never block** on audit (REQ-S-004: the stop path must stay
//!   reachable). The channel bound is generous, so entries overflow only when
//!   the drain's file I/O has genuinely stalled; overflowing entries are
//!   dropped, counted, and surfaced — never silently.
//! - Records are timestamped when written by the drain, marginally after
//!   capture. Pre-session lifecycle entries (pairing) wait until the session
//!   id is known, because the per-session file is named by it.
//! - A session that never reaches a negotiated session id (a failed pairing)
//!   produces no durable file — there is no session to name it after; the
//!   drain reports how many pre-session records were discarded.
//! - Above the per-session soft cap the drain warns once and elides the
//!   **large payload fields** with an explicit `elided` marker while always
//!   keeping the action record and its small correlation fields — `call_id`,
//!   `job_id`, canonical paths, `exit_code`, digests — so a destructive
//!   action never loses its canonical path (ST-M003-004) and
//!   request → job → result stays reconstructable (REQ-N-006, ADR-0008).

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use cretlink_protocol::{JobId, JobOutcome, SessionId};
use cretlink_session::SessionAudit;
use cretlink_shared::{
    Actor, AuditArgs, AuditFileMeta, AuditValue, AuditWriter, Clock,
    DEFAULT_SESSION_SOFT_CAP_BYTES, Outcome, RetentionPolicy, SystemClock, files_to_prune,
    redact_no_digest, session_file_name, truncate_field,
};

use crate::ipc_server::{AuditEntry, AuditReceiver, ChannelAudit};

/// Bound of the audit entry channel (REQ-N-006). Generous on purpose: sends
/// are non-blocking (`try_send`, REQ-S-004), so the bound is only ever reached
/// when the drain's file I/O has genuinely stalled — normal sessions stay far
/// below it and lose nothing.
pub const AUDIT_CHANNEL_CAPACITY: usize = 8192;

/// Args keys preserved when the per-session soft cap elides payload detail:
/// the small correlation fields that keep request → job → transfer → result
/// reconstructable and destructive actions identified by canonical path
/// (ST-M003-004). Everything else (command text, env maps, other large
/// values) is elided.
const ELISION_KEEP_KEYS: &[&str] = &[
    "call_id",
    "job_id",
    "path",
    "to",
    "len",
    "input_len",
    "exit_code",
    "resumed_from",
    "restarted",
    "reason",
    "policy",
    "trigger",
    "removed",
    "failed",
    "dropped",
];

/// Actions after which the drain syncs the file to disk (ADR-0008: fsync at
/// the consent, stop, and cleanup boundaries; the close boundary is synced at
/// seal time). Bounded: everything else stays flush-per-record.
const FSYNC_BOUNDARY_ACTIONS: &[&str] = &[
    "consent_requested",
    "consent_granted",
    "revoked",
    "cleanup",
    "fs_cleanup",
];

/// Configuration of the audit service; injectable so tests can point it at a
/// temporary directory and shrink the caps.
#[derive(Debug, Clone)]
pub struct AuditServiceConfig {
    /// The audit directory holding one `.jsonl` per session.
    pub dir: PathBuf,
    /// Whole-file retention policy applied at console start and after seal.
    pub retention: RetentionPolicy,
    /// Per-session soft cap in bytes; above it, large payload args are elided
    /// while every action record and its correlation fields are still
    /// written. `0` disables the cap.
    pub soft_cap_bytes: u64,
}

impl Default for AuditServiceConfig {
    fn default() -> Self {
        Self {
            dir: default_audit_dir(),
            retention: RetentionPolicy::default(),
            soft_cap_bytes: DEFAULT_SESSION_SOFT_CAP_BYTES,
        }
    }
}

/// The per-user CretLink audit directory. Uses the platform's per-user data
/// location so audit logs are not world-readable, falling back to the temp dir
/// only if no home/data location is discoverable.
#[must_use]
pub fn default_audit_dir() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("APPDATA"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("CretLink").join("audit")
}

/// The honest result of one retention prune pass (REQ-N-006, REQ-S-018).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PruneReport {
    /// File names removed.
    pub removed: Vec<String>,
    /// File names selected for removal whose deletion failed (kept for the
    /// next pass; failures are reported, never hidden).
    pub failed: Vec<String>,
}

/// Applies the whole-file retention policy to the audit directory, deleting
/// expired or over-cap session files oldest-first (ADR-0008). `exempt` names
/// the active session's file, which is never considered for pruning. Runs at
/// console start and after each seal.
#[must_use]
pub fn prune_audit_dir(dir: &Path, policy: &RetentionPolicy, exempt: Option<&str>) -> PruneReport {
    let mut report = PruneReport::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // A missing directory means there is nothing to prune yet.
        Err(_) => return report,
    };
    let mut files: Vec<AuditFileMeta> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".jsonl") || Some(name.as_str()) == exempt {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let modified_unix_millis = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|since| i64::try_from(since.as_millis()).ok())
            .unwrap_or(0);
        files.push(AuditFileMeta {
            name,
            size_bytes: meta.len(),
            modified_unix_millis,
        });
    }
    let now = SystemClock.now_unix_millis();
    for name in files_to_prune(&files, now, policy) {
        match std::fs::remove_file(dir.join(&name)) {
            Ok(()) => report.removed.push(name),
            Err(_) => report.failed.push(name),
        }
    }
    report
}

/// A sink wrapper counting the bytes written, so the drain can enforce the
/// per-session soft cap without asking the writer.
struct CountingWriter<W: Write> {
    inner: W,
    written: Arc<AtomicU64>,
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl SessionAudit for ChannelAudit {
    /// Adapts the session runtime's lifecycle hooks onto the unified channel.
    /// The runtime supplies its own attribution (console/agent/system); the
    /// per-session writer stamps the session id. Non-blocking, like every
    /// audit send (REQ-S-004).
    fn record(&self, actor: Actor, action: &'static str, outcome: Outcome) {
        self.record_entry(AuditEntry::event(actor, None, action, outcome));
    }
}

/// The opened, named session log the drain writes to: created **before**
/// control begins so refusing to continue without audit is still possible
/// (ADR-0008 explicit-choice requirement).
#[derive(Debug)]
pub struct SessionAuditStart {
    /// The negotiated session the log belongs to.
    pub session: SessionId,
    /// The created (empty) log file.
    pub file: std::fs::File,
    /// The file's name inside the audit directory (`<UTC>-<session>.jsonl`),
    /// used to exempt it from its own retention pass.
    pub file_name: String,
}

/// Creates the audit directory and the per-session log file (ADR-0008
/// `<UTC-start>-<session>.jsonl`), returning the handle the drain writes to.
///
/// Called by the console the moment the session id is adopted — before
/// consent — so a failure can still be answered with an explicit technician
/// choice (continue without audit, or fail the session closed) instead of a
/// silent degradation.
///
/// # Errors
///
/// A human-readable, secret-free description of why the directory or file
/// could not be created.
pub fn open_session_log(
    config: &AuditServiceConfig,
    session: SessionId,
) -> Result<SessionAuditStart, String> {
    std::fs::create_dir_all(&config.dir).map_err(|error| {
        format!(
            "could not create the audit directory {}: {error}",
            config.dir.display()
        )
    })?;
    let file_name = session_file_name(SystemClock.now_unix_millis(), session);
    let path = config.dir.join(&file_name);
    let file = std::fs::File::create(&path).map_err(|error| {
        format!(
            "could not create the audit file {}: {error}",
            path.display()
        )
    })?;
    Ok(SessionAuditStart {
        session,
        file,
        file_name,
    })
}

/// Spawns the audit drain: the single owner of the per-session
/// [`AuditWriter`].
///
/// The drain first waits for the opened session log on `start_rx` (entries
/// queue in the bounded channel meanwhile), writes and flushes every entry in
/// arrival order — syncing to disk at the ADR-0008 consent/stop/cleanup
/// boundaries — appends an honest `audit_degraded` record if any entries were
/// dropped or any writes failed, seals the log when every [`ChannelAudit`]
/// sender is gone, syncs the seal, and finally runs the retention prune with
/// the fresh file exempt. If `start_rx` is dropped without a log (the session
/// never paired, or the technician chose to continue without audit), the
/// queued entries are discarded with an honest note.
///
/// Every failure path is surfaced prominently on stderr, never swallowed
/// (ADR-0008: an audit write failure must be visible).
pub fn spawn_audit_service(
    config: AuditServiceConfig,
    entries: AuditReceiver,
    start_rx: tokio::sync::oneshot::Receiver<SessionAuditStart>,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let Ok(start) = start_rx.blocking_recv() else {
            // No log will exist: either pairing never yielded a session id,
            // or the technician explicitly chose to continue without audit.
            // Drain, count, and say so honestly.
            let mut discarded: u64 = 0;
            while entries.recv().is_ok() {
                discarded += 1;
            }
            discarded += entries.dropped();
            if discarded > 0 {
                eprintln!(
                    "note: {discarded} audit record(s) were not persisted (no session log was \
                     opened for this run)"
                );
            }
            return;
        };
        // A second handle to the same file so boundary records and the seal
        // can be fsynced (ADR-0008) while the writer owns the primary handle.
        let sync_handle = start.file.try_clone().ok();
        if sync_handle.is_none() {
            eprintln!("note: could not clone the audit file handle; boundary fsync is disabled");
        }
        let written = Arc::new(AtomicU64::new(0));
        let sink = CountingWriter {
            inner: start.file,
            written: Arc::clone(&written),
        };
        let mut writer = match AuditWriter::new_session(sink, start.session, SystemClock) {
            Ok(writer) => writer,
            Err(error) => {
                audit_unavailable(&entries, &format!("could not write the header: {error}"));
                return;
            }
        };

        let mut eliding = false;
        let mut failed_writes: u64 = 0;
        while let Ok(entry) = entries.recv() {
            // Per-session soft cap (ADR-0008, REQ-N-006): above the cap, the
            // large payload fields are replaced by an explicit `elided`
            // marker, but every action record — and its small correlation
            // fields (call_id, job_id, canonical paths, exit_code, digests) —
            // is still written. Warned exactly once.
            if !eliding
                && config.soft_cap_bytes > 0
                && written.load(Ordering::Relaxed) > config.soft_cap_bytes
            {
                eliding = true;
                eprintln!(
                    "WARNING: the session audit log exceeded the {} byte soft cap; large payload \
                     details are now elided while every action and its correlation fields are \
                     still recorded (ADR-0008)",
                    config.soft_cap_bytes
                );
            }
            let args = if eliding {
                elide_payload(entry.args)
            } else {
                entry.args
            };
            // Digests are small correlation data (transfer verification) and
            // are always kept.
            if let Err(error) = writer.append_with(
                entry.actor,
                entry.action,
                args,
                entry.outcome,
                entry.digests,
                entry.duration_ms,
            ) {
                failed_writes += 1;
                if failed_writes == 1 {
                    eprintln!(
                        "\n==================== AUDIT DEGRADED ====================\n\
                         WARNING: an audit record could not be written ({error}).\n\
                         The session continues, but actions from this point may\n\
                         not be durably recorded (REQ-S-017). Failures are\n\
                         counted and reported at session end.\n\
                         ========================================================"
                    );
                } else {
                    eprintln!("WARNING: another audit record write failed ({error})");
                }
            }
            // ADR-0008 boundary fsync: consent, stop (revoked), and cleanup
            // records reach the disk, not just the OS cache.
            if FSYNC_BOUNDARY_ACTIONS.contains(&entry.action)
                && let Some(file) = &sync_handle
                && let Err(error) = file.sync_all()
            {
                eprintln!("note: audit boundary fsync failed ({error})");
            }
        }
        // Every sender dropped. First surface any degradation honestly as a
        // record of its own (drops under a drain stall, failed writes), then
        // seal so a truncated tail is detectable (verify_sealed), sync the
        // seal (ADR-0008 close boundary), and apply retention with this fresh
        // file exempt (never the active file).
        let dropped = entries.dropped();
        if dropped > 0 || failed_writes > 0 {
            let mut args = AuditArgs::new().with(
                "reason",
                truncate_field(&format!(
                    "{dropped} audit entries dropped under I/O stall; {failed_writes} record \
                     writes failed"
                )),
            );
            args.insert(
                "dropped",
                AuditValue::Int(i64::try_from(dropped).unwrap_or(i64::MAX)),
            );
            if let Err(error) = writer.append_with(
                Actor::System,
                "audit_degraded",
                args,
                Outcome::Failed,
                BTreeMap::new(),
                None,
            ) {
                eprintln!("WARNING: could not record the audit degradation itself ({error})");
            }
            eprintln!(
                "WARNING: audit was degraded this session: {dropped} entries dropped, \
                 {failed_writes} writes failed"
            );
        }
        if let Err(error) = writer.close() {
            eprintln!("WARNING: failed to seal the session audit log ({error})");
        }
        if let Some(file) = &sync_handle
            && let Err(error) = file.sync_all()
        {
            eprintln!("note: audit close fsync failed ({error})");
        }
        report_prune(prune_audit_dir(
            &config.dir,
            &config.retention,
            Some(&start.file_name),
        ));
    })
}

/// Reduces an args map to its small correlation fields plus the explicit
/// `elided` marker (ADR-0008 soft cap): large payloads go, the fields that
/// keep the action reconstructable and destructive paths identified stay.
fn elide_payload(args: AuditArgs) -> AuditArgs {
    let mut kept = AuditArgs::new();
    for (key, value) in args.0 {
        if ELISION_KEEP_KEYS.contains(&key.as_str()) {
            kept.insert(key, value);
        }
    }
    kept.insert("elided", AuditValue::Bool(true));
    kept
}

/// Degraded mode when the session log could not be initialised at all: a
/// prominent banner, then a discard drain so channel memory stays bounded.
///
/// ADR-0008 requires that continuing without audit be an explicit technician
/// choice; the console binary asks that question **before** control begins
/// (see [`open_session_log`] and the consent-time prompt in `main.rs`). This
/// path covers only the residual failure after that point (e.g. the header
/// write failing on a just-created file), which cannot be un-consented — so
/// it is surfaced as loudly as possible instead.
fn audit_unavailable(entries: &AuditReceiver, detail: &str) {
    eprintln!(
        "\n==================== AUDIT DEGRADED ====================\n\
         WARNING: the session audit log is unavailable ({detail}).\n\
         Session actions are NOT being durably recorded (REQ-S-017,\n\
         ADR-0008). Stop the session if a record is required.\n\
         ========================================================"
    );
    while entries.recv().is_ok() {}
}

/// Prints an honest one-line summary of a prune pass when it did anything.
pub fn report_prune(report: PruneReport) {
    if !report.removed.is_empty() {
        println!(
            "Audit retention: removed {} expired session log(s).",
            report.removed.len()
        );
    }
    for name in &report.failed {
        eprintln!("WARNING: audit retention could not remove {name}; will retry at next start");
    }
}

// --- Manual console command activity (actor `console`, ST-M003-006) ---

/// The interactive CLI's manual-command bookkeeping and audit producer.
///
/// This is the **production path** `main.rs` drives (and tests drive
/// directly): it records `console_exec_start`/`console_exec_finish` pairs
/// sharing a `job_id` — with a real wall-clock duration on finish — honors
/// the `redact` toggle at capture time (REQ-S-018), audits cancellations, and
/// answers which jobs are manual so live activity lines carry the right
/// `[console]`/`[ai]` origin (REQ-F-017). Attribution comes from this source
/// site: a bridge job never enters this set.
#[derive(Debug, Default)]
pub struct ManualActivity {
    jobs: HashMap<JobId, Instant>,
    /// When on, manually typed command text is recorded as a `[REDACTED]`
    /// marker instead of the literal text (REQ-S-018).
    pub redact_commands: bool,
}

impl ManualActivity {
    /// Fresh state: no manual jobs, redaction off (the CLI shows the state).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `job` was started by the console CLI.
    #[must_use]
    pub fn is_manual(&self, job: JobId) -> bool {
        self.jobs.contains_key(&job)
    }

    /// Records the start of a manual command and begins tracking its job.
    pub fn command_started(
        &mut self,
        audit: &ChannelAudit,
        session: SessionId,
        job: JobId,
        command: &str,
    ) {
        audit.record_entry(console_exec_started(
            session,
            job,
            command,
            self.redact_commands,
        ));
        self.jobs.insert(job, Instant::now());
    }

    /// Records a manual command the runtime refused to start.
    pub fn command_refused(
        &self,
        audit: &ChannelAudit,
        session: SessionId,
        command: &str,
        reason: &str,
    ) {
        audit.record_entry(console_exec_refused(
            session,
            command,
            self.redact_commands,
            reason,
        ));
    }

    /// Records a manual cancellation request with its honest outcome.
    pub fn cancel_requested(&self, audit: &ChannelAudit, session: SessionId, job: JobId, ok: bool) {
        audit.record_entry(console_cancel(session, job, ok));
    }

    /// Records the finish of a **manual** job (sharing the start record's
    /// `job_id`, with the observed duration) and stops tracking it. Returns
    /// `false` — and records nothing — for jobs this CLI did not start:
    /// bridge jobs are audited by their routed call.
    pub fn job_finished(
        &mut self,
        audit: &ChannelAudit,
        session: SessionId,
        job: JobId,
        exit_code: Option<i32>,
        outcome: JobOutcome,
    ) -> bool {
        let Some(started_at) = self.jobs.remove(&job) else {
            return false;
        };
        let duration = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        audit.record_entry(console_exec_finished(
            session,
            job,
            exit_code,
            outcome,
            Some(duration),
        ));
        true
    }
}

/// Entry for a manual command the CLI successfully started. When the `redact`
/// toggle is on, the command text becomes a digest-free `[REDACTED]` marker
/// (REQ-S-018); otherwise it is truncated to the audit field bound.
#[must_use]
pub fn console_exec_started(
    session: SessionId,
    job: JobId,
    command: &str,
    redact_command: bool,
) -> AuditEntry {
    let mut entry = AuditEntry::event(
        Actor::Console,
        Some(session),
        "console_exec_start",
        Outcome::Ok,
    );
    entry.args = AuditArgs::new()
        .with("command", manual_command_value(command, redact_command))
        .with("job_id", job_id_value(job));
    entry
}

/// Entry for a manual command the runtime refused to start (no job exists).
#[must_use]
pub fn console_exec_refused(
    session: SessionId,
    command: &str,
    redact_command: bool,
    reason: &str,
) -> AuditEntry {
    let mut entry = AuditEntry::event(
        Actor::Console,
        Some(session),
        "console_exec_start",
        Outcome::Failed,
    );
    entry.args = AuditArgs::new()
        .with("command", manual_command_value(command, redact_command))
        .with("reason", truncate_field(reason));
    entry
}

/// Entry for a manual job's terminal event, sharing the start record's
/// `job_id` and carrying the exit code, the wall-clock duration observed by
/// the console, and the honest outcome (REQ-N-008).
#[must_use]
pub fn console_exec_finished(
    session: SessionId,
    job: JobId,
    exit_code: Option<i32>,
    outcome: JobOutcome,
    duration_ms: Option<u64>,
) -> AuditEntry {
    let mut entry = AuditEntry::event(
        Actor::Console,
        Some(session),
        "console_exec_finish",
        match outcome {
            JobOutcome::Completed => Outcome::Ok,
            JobOutcome::Failed => Outcome::Failed,
            JobOutcome::Cancelled => Outcome::Cancelled,
            JobOutcome::TimedOut => Outcome::Timeout,
            JobOutcome::Unknown => Outcome::Unknown,
        },
    );
    entry.args = AuditArgs::new().with("job_id", job_id_value(job));
    if let Some(code) = exit_code {
        entry
            .args
            .insert("exit_code", AuditValue::Int(i64::from(code)));
    }
    entry.duration_ms = duration_ms;
    entry
}

/// Entry for a manual cancellation request.
#[must_use]
pub fn console_cancel(session: SessionId, job: JobId, ok: bool) -> AuditEntry {
    let mut entry = AuditEntry::event(
        Actor::Console,
        Some(session),
        "console_cancel",
        if ok { Outcome::Ok } else { Outcome::Failed },
    );
    entry.args = AuditArgs::new().with("job_id", job_id_value(job));
    entry
}

/// The audit value for a manually typed command, honoring the CLI `redact`
/// toggle: a digest-free marker (a typed command may embed a low-entropy
/// secret) or the truncated text.
fn manual_command_value(command: &str, redact_command: bool) -> AuditValue {
    if redact_command {
        redact_no_digest(
            "command marked sensitive by the console redact toggle",
            command.len(),
        )
    } else {
        truncate_field(command)
    }
}

/// The Appendix A `job_id` value.
fn job_id_value(job: JobId) -> AuditValue {
    AuditValue::Int(i64::try_from(job.0).unwrap_or(i64::MAX))
}
