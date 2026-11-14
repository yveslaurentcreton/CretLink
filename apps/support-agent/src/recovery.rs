//! The text of the offline residual-recovery verb and of the startup sweep
//! (ST-M006-003, ST-M009-010).
//!
//! `recover [--remove]` is a verb of every program that can act as the
//! customer side: the development binary `cretlink-support-agent`, and the
//! shipped `cretlink` and `cretlink-cli` (including a `CretLink-Support*` copy
//! of `cretlink`). They all print the same report from this module, apart from
//! the program name the customer typed, and exit with the same fail-closed
//! code from [`run_recover`]:
//!
//! - inspect (`recover`) removes nothing and always succeeds, so a customer can
//!   look first;
//! - remove (`recover --remove`) succeeds only when the machine was proved
//!   clean; anything kept or unreadable fails, so a script never mistakes kept
//!   residue for success (REQ-N-008, REQ-F-019).
//!
//! The verb opens no network connection, joins no session and grants no
//! control.
//!
//! The shipped programs know residue that this crate does not: the profile
//! folders that the application's window left (ST-M009-014). They hand it to
//! [`run_recover_with`] as an [`OtherResidue`], and it becomes part of the
//! same report and the same exit code. The development binary has no window
//! and looks for none.
//!
//! A program with administrator rights and a program without them keep their
//! files in different places, and each covers its own (ST-M006-020; see
//! [`crate::inventory`]). The report of a run with administrator rights says
//! so, and [`removal_needs_administrator`] tells a start which kind of run
//! removes what it kept.

use std::fmt::Write as _;
use std::path::PathBuf;

/// The startup sweep's report, re-exported for the application's customer
/// role, which maps it to its notice.
pub use cretlink_exec::StartupSweepReport;
use cretlink_exec::{FileStore, OwnFolder, Standing};
use cretlink_protocol::{CleanupFailure, CleanupTrigger};

use crate::inventory::{Inventory, RecoveryReport};
use crate::privilege::EffectiveIdentity;
use crate::reboot_resume::TicketStore;

/// What one run of the recovery verb printed and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverRun {
    /// The report, as printed to standard output.
    pub text: String,
    /// Whether the process exits successfully: always for an inspection, and
    /// for a removal only when the machine was proved clean.
    pub success: bool,
}

/// Residue of a kind that the program which runs the verb knows and this
/// crate does not: the profile folders of the application's webview
/// (ST-M009-014). What it adds is printed and counted like everything else:
/// kept or unreadable residue is never a success (REQ-N-008).
pub trait OtherResidue {
    /// Adds what is there to the inventory, removing nothing: what was found,
    /// what could not be read, and the kinds that were looked for and are
    /// empty.
    fn scan(&self, inventory: &mut Inventory);

    /// Removes what is there, and adds to the report what was found, what
    /// was removed, what was kept with the reason, and what could not be
    /// read.
    fn recover(&self, report: &mut RecoveryReport);
}

/// Runs the recovery verb on the platform defaults. Without `remove` it
/// inspects and lists the CretLink-owned residue, removing nothing; with
/// `remove` it removes what it can and reports honestly what was kept.
/// `program` is the command the customer typed (`cretlink`, `cretlink-cli`,
/// `cretlink-support-agent`, or a `CretLink-Support*` copy) and names the
/// follow-up command in the report.
pub async fn run_recover(program: &str, remove: bool, tickets: &TicketStore) -> RecoverRun {
    run_recover_with(program, remove, tickets, None).await
}

/// [`run_recover`] with the residue that only the program which runs the verb
/// knows, as part of the same report and the same exit code.
pub async fn run_recover_with(
    program: &str,
    remove: bool,
    tickets: &TicketStore,
    other: Option<&(dyn OtherResidue + Sync)>,
) -> RecoverRun {
    let with_administrator_rights = OwnFolder::staging_for_this_process().is_private();
    let run = if remove {
        let mut report = crate::inventory::recover(tickets).await;
        if let Some(other) = other {
            other.recover(&mut report);
        }
        RecoverRun {
            text: recovery_report_text(&report),
            success: report.ok(),
        }
    } else {
        let mut inventory = crate::inventory::scan(tickets).await;
        if let Some(other) = other {
            other.scan(&mut inventory);
        }
        RecoverRun {
            text: inventory_text(program, &inventory),
            success: true,
        }
    };
    RecoverRun {
        text: with_scope_note(run.text, with_administrator_rights),
        success: run.success,
    }
}

/// What the report of a run with administrator rights does not cover. (A run
/// without them that cannot see the administrators' folder says so itself,
/// as something it could not read.)
const OTHER_KEEPER_NOTE: &str = concat!(
    "  Note: this covers what CretLink keeps when it runs with administrator rights.\n",
    "  What it kept while it ran without them lies in the folder of that account;\n",
    "  run this command there without administrator rights to see or remove it.\n",
);

/// A report, with what a run with administrator rights does not cover said
/// after it.
fn with_scope_note(mut text: String, with_administrator_rights: bool) -> String {
    if with_administrator_rights {
        text.push_str(OTHER_KEEPER_NOTE);
    }
    text
}

/// Whether removing what a start kept takes a run with administrator rights:
/// this process has them, so what it keeps lies where only administrators can
/// write; or the folder of an earlier release is among what was kept. Without
/// either, what was kept lies in the folder of this account, and a run with
/// administrator rights would not look there.
#[must_use]
pub fn removal_needs_administrator(report: &StartupSweepReport) -> bool {
    needs_administrator(report, OwnFolder::staging_for_this_process().is_private())
}

/// [`removal_needs_administrator`] for a process that is elevated or not.
fn needs_administrator(report: &StartupSweepReport, elevated: bool) -> bool {
    elevated
        || report
            .failed
            .iter()
            .any(|failure| failure.reason.starts_with(REASON_EARLIER_RELEASE))
}

/// The command that removes the residue: `<program> recover --remove`.
#[must_use]
pub fn recover_command(program: &str) -> String {
    format!("{program} recover --remove")
}

/// The effective-identity header every customer-side command line prints first
/// (REQ-S-011): whether the process is elevated matters for what recovery can
/// remove.
#[must_use]
pub fn identity_text(program: &str, identity: &EffectiveIdentity) -> String {
    let mut text = String::new();
    let _ = writeln!(
        text,
        "{program} {} — effective identity (REQ-S-011):",
        cretlink_shared::RELEASE_VERSION
    );
    let _ = writeln!(text, "  account:   {}", identity.username);
    if let Some(domain) = identity.domain.as_deref() {
        let _ = writeln!(text, "  domain:    {domain}");
    }
    let _ = writeln!(text, "  windows:   {}", identity.is_windows);
    let _ = writeln!(text, "  elevated:  {}", identity.elevated);
    let _ = writeln!(text, "  integrity: {}", identity.integrity);
    if let Some(note) = identity.note.as_deref() {
        let _ = writeln!(text, "  note:      {note}");
    }
    if !identity.elevated {
        let _ = writeln!(
            text,
            "  (not elevated — if UAC was declined, the agent gains no admin capability, REQ-F-002)"
        );
    }
    text.push('\n');
    text
}

/// An owned-resource inventory in secret-free terms (never key material).
#[must_use]
pub fn inventory_text(program: &str, inventory: &Inventory) -> String {
    let mut text = String::new();
    text.push_str("CretLink owned-resource inventory (ST-M006-003):\n");
    if inventory.found.is_empty() {
        text.push_str("  No CretLink-owned resources were found on this machine.\n");
    } else {
        let _ = writeln!(
            text,
            "  Found {} resource(s) still on this machine:",
            inventory.found.len()
        );
        for resource in &inventory.found {
            match resource.session {
                Some(session) => {
                    let _ = writeln!(
                        text,
                        "  - {}: {}  (session {session})",
                        resource.kind.label(),
                        resource.description
                    );
                }
                None => {
                    let _ = writeln!(
                        text,
                        "  - {}: {}",
                        resource.kind.label(),
                        resource.description
                    );
                }
            }
        }
    }
    for unknown in &inventory.unreadable {
        let _ = writeln!(
            text,
            "  ! Could not read {} ({}) — treating it as possibly present.",
            unknown.kind.label(),
            unknown.reason
        );
    }
    if !inventory.asserted_empty.is_empty() {
        let checked: Vec<&str> = inventory
            .asserted_empty
            .iter()
            .map(|kind| kind.label())
            .collect();
        let _ = writeln!(text, "  Checked and empty: {}.", checked.join(", "));
    }
    if inventory.is_clean() {
        text.push_str("  This machine is clean — nothing to remove.\n");
    } else {
        let _ = writeln!(
            text,
            "  To remove the resources above, run:  {}",
            recover_command(program)
        );
    }
    text
}

/// An honest recovery report: removals, kept failures with remediation, and
/// any state that could not be read (REQ-N-008).
#[must_use]
pub fn recovery_report_text(report: &RecoveryReport) -> String {
    let mut text = String::new();
    text.push_str("CretLink residual recovery (ST-M006-003):\n");
    // A store that could not be read is not in `found`, but a failure to
    // remove it is still residue: never "nothing was present" (REQ-N-008).
    if report.found.is_empty() && report.unreadable.is_empty() && report.failed.is_empty() {
        text.push_str("  Nothing owned by CretLink was present — nothing to remove.\n");
        return text;
    }
    let _ = writeln!(
        text,
        "  Removed {} of {} resource(s) found.",
        report.removed.len(),
        report.found.len()
    );
    for resource in &report.removed {
        let _ = writeln!(
            text,
            "  - removed {}: {}",
            resource.kind.label(),
            resource.description
        );
    }
    if !report.failed.is_empty() {
        let _ = writeln!(
            text,
            "  KEPT {} resource(s) that could NOT be removed (a later start will retry):",
            report.failed.len()
        );
        for failure in &report.failed {
            let _ = writeln!(
                text,
                "  - {}: {}",
                failure.kind.label(),
                failure.description
            );
            let _ = writeln!(text, "      reason:      {}", failure.reason);
            let _ = writeln!(text, "      remediation: {}", failure.remediation);
        }
    }
    for unknown in &report.unreadable {
        let _ = writeln!(
            text,
            "  ! Could not read {} ({}) — state unknown.",
            unknown.kind.label(),
            unknown.reason
        );
    }
    if report.ok() {
        text.push_str("  Recovery complete — this machine is clean.\n");
    } else {
        text.push_str("  Recovery did NOT fully complete — see the kept/unknown items above.\n");
    }
    text
}

/// The file in the staging base that every customer-side process holds a
/// shared lock on for as long as it runs (ST-M009-010).
pub const IN_USE_LOCK: &str = ".in-use.lock";

/// How long a start waits for its share of the staging base while another
/// process sweeps. A sweep is short; a lock that is held longer is not a
/// sweep, and must not keep this program from starting.
const SHARE_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// Why a start did not sweep: something else holds the lock for good.
const REASON_LOCK_HELD: &str = "the staging area is held by another program; nothing was swept";

/// Why a start did not sweep: the lock of a private staging area could not be
/// made or opened.
const REASON_NO_LOCK: &str = "the lock of the staging area could not be opened; nothing was swept";

/// Why a start did not sweep and the program stages nothing: its staging area
/// could not be made private.
const REASON_NOT_PRIVATE: &str = "the staging area could not be made private";

/// How long a start waits for the folder of an earlier release to be removed.
/// What such a release left is removed in a moment; a folder that takes longer
/// was filled by something else, and must not keep this program from starting.
const EARLIER_RELEASE_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

/// How long `recover --remove` waits for the same removal. Longer than a
/// start: whoever runs the command asked for it. Not without end: a program
/// can hold an entry of the folder in a way that keeps a removal waiting for
/// as long as that program likes, and must not keep the command from ending.
pub(crate) const EARLIER_RELEASE_RECOVERY_WAIT: std::time::Duration =
    std::time::Duration::from_secs(120);

/// How every reason starts for a folder of a release up to 0.24.0 that a start
/// did not remove.
pub(crate) const REASON_EARLIER_RELEASE: &str = "left by an earlier release";

/// This process's share of the staging base, held for as long as the process
/// may run a session: while any process holds one, no other process's startup
/// sweep removes a staging tree, so a second **Get help** window can never
/// delete the files of a session that is live in the first. The operating
/// system releases it when the process ends, a crash included, so it cannot be
/// stranded.
#[derive(Debug)]
pub struct StagingInUse {
    _file: std::fs::File,
}

/// Opens the in-use lock file, creating the staging base and the file when
/// they do not exist yet.
///
/// A base that must be private is prepared first (ST-M006-020): the lock is
/// then made in a folder that only administrators can write. A link or a
/// folder under the lock's name is removed and never opened. In a base of the
/// account, a process that may not write there opens the lock for reading,
/// which is enough to lock it.
fn open_in_use(base: &OwnFolder) -> std::io::Result<std::fs::File> {
    let path = base.path().join(IN_USE_LOCK);
    let create = || {
        if Standing::of_file(&path) == Standing::Foreign {
            cretlink_exec::remove_whole(&path)?;
        }
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
    };
    if base.is_private() {
        base.prepare()?;
        return create();
    }
    base.prepare()
        .and_then(|()| create())
        .or_else(|_| std::fs::File::open(&path))
}

/// Sweeps the staging trees earlier runs left behind (crash recovery, and the
/// retry of a removal that failed), as every customer-side program does before
/// it connects (ST-M003-005, ST-M009-010). It never errors; failures are in
/// the report and are retried on the next start.
///
/// The sweep runs only when no other customer-side process holds the staging
/// base ([`StagingInUse`]); otherwise it is skipped — nothing removed, nothing
/// claimed — and a later start retries. Either way the returned share must be
/// kept for as long as this process may run a session. `None` when the lock
/// could not be taken: then the sweep was skipped too, and this process's own
/// staging is not protected from another window's sweep.
///
/// The staging base is the one of this process: for an elevated program the
/// folder that only administrators can write, which is prepared before
/// anything in it is read (ST-M006-020). When that cannot be done, nothing is
/// read and the report says so. The start that sweeps also removes, whole and
/// without reading it, the folder in which releases up to 0.24.0 kept these
/// files, where every account could write.
///
/// Before anything else an elevated program leaves the working directory it
/// was started in, which whoever started it chose
/// ([`cretlink_exec::leave_the_starters_folder`]).
pub async fn sweep_stale_staging() -> (StartupSweepReport, Option<StagingInUse>) {
    cretlink_exec::leave_the_starters_folder();
    sweep_and_share(
        OwnFolder::staging_for_this_process(),
        cretlink_exec::earlier_release_folder(),
        SHARE_WAIT,
    )
    .await
}

/// [`sweep_stale_staging`] with an explicit staging base, for tests.
pub async fn sweep_stale_staging_in(
    base: impl Into<OwnFolder>,
) -> (StartupSweepReport, Option<StagingInUse>) {
    sweep_and_share(base.into(), None, SHARE_WAIT).await
}

/// The sweep and the share of [`sweep_stale_staging`]: with the folder of an
/// earlier release to remove, if there is one, and waiting at most
/// `share_wait` for the share.
async fn sweep_and_share(
    base: OwnFolder,
    earlier: Option<PathBuf>,
    share_wait: std::time::Duration,
) -> (StartupSweepReport, Option<StagingInUse>) {
    let lock = base.path().join(IN_USE_LOCK);
    let Ok(file) = open_in_use(&base) else {
        // Whether another window runs a session cannot be told: leave every
        // tree for a later start. A private base that could not be prepared
        // is not usable at all, and one whose lock could not be opened was
        // not swept: the customer is told which.
        let mut report = StartupSweepReport::default();
        if base.is_private() {
            let failure = if let Err(error) = base.prepare() {
                // With the cause, so that an administrator can put it right.
                CleanupFailure {
                    canonical_path: base.path().to_string_lossy().into_owned(),
                    reason: format!("{REASON_NOT_PRIVATE}: {error}"),
                }
            } else {
                CleanupFailure {
                    canonical_path: lock.to_string_lossy().into_owned(),
                    reason: REASON_NO_LOCK.to_string(),
                }
            };
            report.failed.push(failure);
        }
        return (report, None);
    };
    let private = base.is_private();
    let (mut report, swept) = match file.try_lock() {
        Ok(()) => {
            let report = FileStore::sweep_all_sessions(base, CleanupTrigger::Shutdown).await;
            let _ = file.unlock();
            (report, true)
        }
        // Another process runs (or sweeps): its trees may be live.
        Err(_) => (StartupSweepReport::default(), false),
    };
    // The start that swept also removes the folder of an earlier release:
    // after the lock is given back, so that no other window waits for it.
    if swept
        && let Some(folder) = earlier
        && let Some(why) = remove_earlier_release(
            folder.clone(),
            Remover::Start,
            private,
            EARLIER_RELEASE_WAIT,
        )
        .await
    {
        report.failed.push(CleanupFailure {
            canonical_path: folder.to_string_lossy().into_owned(),
            reason: format!("{REASON_EARLIER_RELEASE}; {why}"),
        });
    }
    // Waits only while another process is sweeping, never for a session, and
    // not without end.
    let held = tokio::task::spawn_blocking(move || {
        let deadline = std::time::Instant::now() + share_wait;
        loop {
            match file.try_lock_shared() {
                Ok(()) => return Some(file),
                Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(_) => return None,
            }
        }
    })
    .await;
    match held {
        Ok(Some(file)) => (report, Some(StagingInUse { _file: file })),
        _ => {
            // Nothing was swept and nothing is held: said, not passed over.
            report.failed.push(CleanupFailure {
                canonical_path: lock.to_string_lossy().into_owned(),
                reason: REASON_LOCK_HELD.to_string(),
            });
            (report, None)
        }
    }
}

/// Who removes the folder of an earlier release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Remover {
    /// A start that swept, unasked.
    Start,
    /// `recover --remove`, which whoever runs it asked for.
    Recovery,
}

/// Why a start leaves the folder of an earlier release where it is, when it
/// does. `private` says that this program runs with administrator rights.
///
/// A program of such a release that still runs loses nothing to a start. And
/// a start with administrator rights removes unasked only a folder that the
/// administrators own: every account can make that folder again and move into
/// it what it may move but not remove, and those rights would then remove it
/// for that account. Such a folder is named and left to `recover --remove`,
/// or to a start from the account it belongs to.
fn start_leaves(private: bool, in_use: bool, administrators_own: bool) -> Option<&'static str> {
    if in_use {
        Some("a program of that release still uses it")
    } else if private && !administrators_own {
        Some("it belongs to an account, not to the administrators, and is removed only on request")
    } else {
        None
    }
}

/// Removes the folder in which releases up to 0.24.0 kept the staging area and
/// the reboot tickets (ST-M006-020). Every account could write there, so
/// nothing in it is read: it is removed whole, without following a link.
///
/// A start leaves the folder where [`start_leaves`] says so; a recovery
/// removes it whoever owns it. `private` says that this program runs with
/// administrator rights. The work is waited for only as long as `wait`
/// ([`within`]); a later start finishes what this one could not.
///
/// Returns why the folder is still there; `None` when it is gone or was never
/// there.
pub(crate) async fn remove_earlier_release(
    folder: PathBuf,
    by: Remover,
    private: bool,
    wait: std::time::Duration,
) -> Option<String> {
    let work = move || {
        match std::fs::symlink_metadata(&folder) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            // What cannot be looked at is not known to be gone.
            Err(error) => return Err(format!("it could not be looked at ({})", error.kind())),
        }
        if by == Remover::Start
            && let Some(why) = start_leaves(
                private,
                cretlink_exec::earlier_release_in_use(&folder),
                cretlink_exec::administrators_own(&folder),
            )
        {
            return Err(why.to_string());
        }
        match cretlink_exec::remove_whole(&folder) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Err(if private {
                "it could not be removed, also not with administrator rights".to_string()
            } else {
                "removing it needs administrator rights".to_string()
            }),
            Err(error) => Err(format!("could not be removed ({})", error.kind())),
        }
    };
    match within(wait, work).await {
        Some(Ok(())) => None,
        Some(Err(why)) => Some(why),
        None => Some("its removal was not finished in time".to_string()),
    }
}

/// Runs `work` on a thread of its own and waits for it at most `wait`; `None`
/// when it was not done by then.
///
/// The work is on the file system, where another program can hold an entry in
/// a way that keeps a call waiting for as long as that program likes. So it
/// runs where it keeps nothing else waiting: not the caller after `wait`, and
/// not the end of this program, which a thread of its own does not hold up.
/// The work goes on after the wait for as long as this program runs.
async fn within<T: Send + 'static>(
    wait: std::time::Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (done, result) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("cretlink-earlier-release".to_string())
        .spawn(move || {
            let _ = done.send(work());
        })
        .ok()?;
    tokio::time::timeout(wait, result).await.ok()?.ok()
}

/// The startup sweep in words for a command line, or `None` for a clean run
/// with nothing to do. Failures are the ones a later start retries; they are
/// never dressed up as success (REQ-N-008, REQ-F-019).
#[must_use]
pub fn sweep_text(report: &StartupSweepReport) -> Option<String> {
    if report.sessions_removed == 0 && report.artifacts_removed == 0 && report.ok() {
        return None;
    }
    let mut text = String::new();
    let _ = writeln!(
        text,
        "Startup cleanup: reclaimed {} stale session area(s), removed {} leftover artifact(s).",
        report.sessions_removed, report.artifacts_removed
    );
    if !report.ok() {
        let _ = writeln!(
            text,
            "  {} item(s) could NOT be removed and will be looked at again on the next start:",
            report.failed.len()
        );
        for failure in &report.failed {
            let _ = writeln!(text, "  - {} ({})", failure.canonical_path, failure.reason);
        }
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use cretlink_protocol::SessionId;

    use super::*;
    use crate::inventory::{OwnedResource, RecoveryFailure, ResourceKind, UnreadableKind};
    use crate::privilege::IntegrityLevel;

    fn staged(path: &str) -> OwnedResource {
        OwnedResource {
            kind: ResourceKind::StagedFiles,
            description: path.to_owned(),
            session: Some(SessionId::new()),
        }
    }

    #[test]
    fn an_inspection_names_the_removal_command_of_the_program_typed() {
        let inventory = Inventory {
            found: vec![staged("C:/Program Files/CretLink Data/staging/x/a.txt")],
            asserted_empty: vec![ResourceKind::RebootResumeTicket],
            unreadable: vec![],
        };
        for program in ["cretlink", "cretlink-cli", "CretLink-Support"] {
            let text = inventory_text(program, &inventory);
            assert!(
                text.ends_with(&format!(
                    "  To remove the resources above, run:  {program} recover --remove\n"
                )),
                "{text}"
            );
            assert!(text.contains("Found 1 resource(s)"));
            assert!(text.contains("Checked and empty: reboot-resume ticket."));
        }
    }

    #[test]
    fn a_clean_inspection_offers_nothing_to_remove() {
        let text = inventory_text("cretlink", &Inventory::default());
        assert!(text.contains("No CretLink-owned resources were found"));
        assert!(text.contains("This machine is clean"));
        assert!(!text.contains("recover --remove"));
    }

    #[test]
    fn an_unreadable_kind_is_never_called_clean() {
        let inventory = Inventory {
            unreadable: vec![UnreadableKind {
                kind: ResourceKind::RebootResumeTicket,
                reason: "denied".to_owned(),
            }],
            ..Inventory::default()
        };
        let text = inventory_text("cretlink", &inventory);
        assert!(text.contains("! Could not read reboot-resume ticket (denied)"));
        assert!(text.contains("run:  cretlink recover --remove"));
        assert!(!text.contains("is clean"));
    }

    #[test]
    fn a_partial_recovery_keeps_and_names_the_failure() {
        let kept = staged("C:/Program Files/CretLink Data/staging/x/b.txt");
        let report = RecoveryReport {
            found: vec![staged("a"), kept.clone()],
            removed: vec![staged("a")],
            failed: vec![RecoveryFailure {
                kind: ResourceKind::StagedFiles,
                description: kept.description.clone(),
                reason: "in use".to_owned(),
                remediation: format!("delete the file manually: {}", kept.description),
            }],
            unreadable: vec![],
        };
        assert!(!report.ok());
        let text = recovery_report_text(&report);
        assert!(text.contains("Removed 1 of 2 resource(s) found."));
        assert!(text.contains("KEPT 1 resource(s) that could NOT be removed"));
        assert!(text.contains("      reason:      in use\n"));
        assert!(
            text.ends_with("Recovery did NOT fully complete — see the kept/unknown items above.\n")
        );
    }

    #[test]
    fn a_complete_recovery_says_clean() {
        let report = RecoveryReport {
            found: vec![staged("a")],
            removed: vec![staged("a")],
            ..RecoveryReport::default()
        };
        assert!(
            recovery_report_text(&report)
                .ends_with("  Recovery complete — this machine is clean.\n")
        );
        assert!(recovery_report_text(&RecoveryReport::default()).contains("nothing to remove"));
    }

    #[test]
    fn an_unreadable_store_that_stays_is_not_nothing() {
        let report = RecoveryReport {
            failed: vec![RecoveryFailure {
                kind: ResourceKind::RebootResumeTicket,
                description: "unreadable ticket store C:/x/tickets.json".to_owned(),
                reason: "denied".to_owned(),
                remediation: "delete it manually".to_owned(),
            }],
            ..RecoveryReport::default()
        };
        let text = recovery_report_text(&report);
        assert!(!text.contains("nothing to remove"), "{text}");
        assert!(text.contains("KEPT 1 resource(s)"), "{text}");
        assert!(text.contains("did NOT fully complete"), "{text}");
    }

    #[test]
    fn the_identity_header_names_the_program() {
        let identity = EffectiveIdentity {
            username: "ann".to_owned(),
            domain: Some("PC-7".to_owned()),
            elevated: false,
            integrity: IntegrityLevel::Medium,
            is_windows: true,
            note: None,
        };
        let text = identity_text("cretlink", &identity);
        assert!(text.starts_with(&format!(
            "cretlink {} — effective identity",
            cretlink_shared::RELEASE_VERSION
        )));
        assert!(text.contains("  domain:    PC-7\n"));
        assert!(text.contains("not elevated"));
    }

    #[test]
    fn a_clean_sweep_is_silent_and_a_failed_one_is_not() {
        assert_eq!(sweep_text(&StartupSweepReport::default()), None);
        let report = StartupSweepReport {
            sessions_scanned: 1,
            sessions_removed: 0,
            artifacts_removed: 2,
            foreign_removed: 0,
            failed: vec![CleanupFailure {
                canonical_path: "C:/x/a.txt".to_owned(),
                reason: "in use".to_owned(),
            }],
        };
        let text = sweep_text(&report).unwrap_or_default();
        assert!(text.contains("removed 2 leftover artifact(s)"));
        assert!(text.contains("1 item(s) could NOT be removed"));
        assert!(text.contains("  - C:/x/a.txt (in use)\n"));
    }

    /// A staging tree as a crashed session leaves it: an orphaned part file.
    fn stale_tree(base: &Path) -> PathBuf {
        let root = base.join(SessionId::new().to_string());
        let parts = root.join(".parts");
        std::fs::create_dir_all(&parts).unwrap_or_default();
        std::fs::write(parts.join("orphan.part"), b"x").unwrap_or_default();
        root
    }

    #[tokio::test]
    async fn a_second_window_never_sweeps_the_staging_of_a_running_one() {
        let base = std::env::temp_dir().join(format!(
            "cretlink-in-use-{}-{}",
            std::process::id(),
            SessionId::new()
        ));
        // The first window sweeps the leftovers of a crash, then holds the base.
        let crashed = stale_tree(&base);
        let (report, first) = sweep_stale_staging_in(base.clone()).await;
        assert!(first.is_some(), "the first window holds the base");
        assert_eq!(report.sessions_removed, 1);
        assert!(!crashed.exists());

        // Its live session stages files; a second window starts meanwhile.
        let live = stale_tree(&base);
        let (report, second) = sweep_stale_staging_in(base.clone()).await;
        assert_eq!(
            report,
            StartupSweepReport::default(),
            "the sweep was skipped"
        );
        assert!(live.exists(), "the live session's tree is untouched");
        assert!(second.is_some(), "the second window holds the base too");

        // Once no window runs any more, the next start sweeps again.
        drop(first);
        drop(second);
        let (report, third) = sweep_stale_staging_in(base.clone()).await;
        assert_eq!(report.sessions_removed, 1);
        assert!(!live.exists());
        drop(third);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A lock that something holds for good, as a sweep holds it for a moment,
    /// does not keep the program from starting: the start says that it did not
    /// sweep, and goes on without its share.
    #[tokio::test]
    async fn a_lock_held_for_good_does_not_keep_the_program_from_starting() {
        let base = std::env::temp_dir().join(format!(
            "cretlink-lock-held-{}-{}",
            std::process::id(),
            SessionId::new()
        ));
        let crashed = stale_tree(&base);
        let lock = base.join(IN_USE_LOCK);
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock);
        assert!(holder.as_ref().is_ok_and(|file| file.lock().is_ok()));

        let started = std::time::Instant::now();
        let (report, share) = sweep_and_share(
            OwnFolder::same_account(&base),
            None,
            std::time::Duration::from_millis(300),
        )
        .await;

        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert!(share.is_none());
        assert!(crashed.exists(), "nothing was swept");
        assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
        assert_eq!(report.failed[0].canonical_path, lock.to_string_lossy());
        assert_eq!(report.failed[0].reason, REASON_LOCK_HELD);
        drop(holder);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A sandbox folder that stands for the data folder of an elevated
    /// program, with its staging area private from there down (ST-M006-020).
    #[cfg(unix)]
    fn private_staging(name: &str) -> (PathBuf, OwnFolder) {
        let root = std::env::temp_dir().join(format!(
            "cretlink-{name}-{}-{}",
            std::process::id(),
            SessionId::new()
        ));
        let data = root.join("CretLink Data");
        let staging = OwnFolder::administrators_only(&data, data.join("staging"));
        (root, staging)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_link_under_the_name_of_the_lock_is_replaced_and_never_opened() {
        let (root, base) = private_staging("lock-link");
        std::fs::create_dir_all(base.path()).unwrap_or_default();
        let theirs = root.join("theirs.txt");
        std::fs::write(&theirs, b"theirs").unwrap_or_default();
        let lock = base.path().join(IN_USE_LOCK);
        std::os::unix::fs::symlink(&theirs, &lock).unwrap_or_default();

        let (report, held) = sweep_stale_staging_in(base.clone()).await;

        assert!(report.ok(), "{:?}", report.failed);
        assert!(held.is_some(), "the base is held through a lock of its own");
        let kind = std::fs::symlink_metadata(&lock).map(|facts| facts.file_type());
        assert!(
            kind.is_ok_and(|kind| kind.is_file()),
            "the lock is a file of its own now"
        );
        assert_eq!(std::fs::read(&theirs).unwrap_or_default(), b"theirs");
        drop(held);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_private_base_that_cannot_be_prepared_is_named_and_not_swept() {
        let (root, _) = private_staging("behind-a-link");
        // A crashed session's tree lies elsewhere, and a link stands where
        // the data folder belongs.
        let elsewhere = root.join("elsewhere");
        let crashed = stale_tree(&elsewhere.join("staging"));
        let data = root.join("CretLink Data");
        std::os::unix::fs::symlink(&elsewhere, &data).unwrap_or_default();
        let base = OwnFolder::administrators_only(&data, data.join("staging"));

        let (report, held) = sweep_stale_staging_in(base.clone()).await;

        assert!(held.is_none());
        assert!(crashed.exists(), "nothing behind the link was swept");
        assert!(
            !elsewhere.join("staging").join(IN_USE_LOCK).exists(),
            "no lock was made behind it"
        );
        assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
        assert_eq!(
            report.failed[0].canonical_path,
            base.path().to_string_lossy()
        );
        // The reason says why, so that an administrator can put it right.
        assert!(
            report.failed[0]
                .reason
                .starts_with("the staging area could not be made private: "),
            "{}",
            report.failed[0].reason
        );
        let text = sweep_text(&report).unwrap_or_default();
        assert!(text.contains("could not be made private"), "{text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A sandbox with a staging base of the account and, beside it, the
    /// folder of an earlier release with what such a release left.
    fn with_an_earlier_release(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "cretlink-{name}-{}-{}",
            std::process::id(),
            SessionId::new()
        ));
        let base = root.join("staging");
        let earlier = root.join("earlier").join("CretLink");
        stale_tree(&earlier.join("staging"));
        std::fs::write(earlier.join("staging").join(IN_USE_LOCK), b"").unwrap_or_default();
        (root, base, earlier)
    }

    const A_MOMENT: std::time::Duration = std::time::Duration::from_secs(20);

    /// The start that sweeps also removes the folder of an earlier release,
    /// whole, and says nothing when that worked.
    #[tokio::test]
    async fn a_start_that_sweeps_removes_the_folder_of_an_earlier_release() {
        let (root, base, earlier) = with_an_earlier_release("earlier");
        assert!(earlier.join("staging").is_dir());

        let (report, share) = sweep_and_share(
            OwnFolder::same_account(&base),
            Some(earlier.clone()),
            A_MOMENT,
        )
        .await;

        assert_eq!(report, StartupSweepReport::default());
        assert_eq!(sweep_text(&report), None);
        assert!(share.is_some());
        assert!(!earlier.exists(), "the folder is gone, with all it held");
        assert!(
            earlier.parent().is_some_and(Path::is_dir),
            "the folder above it is not CretLink's"
        );
        assert!(!needs_administrator(&report, false));
        drop(share);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A window that does not sweep, because another one runs, leaves the
    /// folder of an earlier release alone too.
    #[tokio::test]
    async fn a_window_that_does_not_sweep_leaves_the_folder_of_an_earlier_release() {
        let (root, base, earlier) = with_an_earlier_release("earlier-second");
        let (_, first) = sweep_and_share(OwnFolder::same_account(&base), None, A_MOMENT).await;
        assert!(first.is_some(), "the first window holds the base");

        let (report, second) = sweep_and_share(
            OwnFolder::same_account(&base),
            Some(earlier.clone()),
            A_MOMENT,
        )
        .await;

        assert_eq!(report, StartupSweepReport::default());
        assert!(second.is_some());
        assert!(earlier.join("staging").is_dir(), "nothing was removed");
        drop(first);
        drop(second);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A folder of an earlier release that this account may not remove is
    /// named, with what removes it, and the start goes on.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_folder_of_an_earlier_release_that_cannot_be_removed_is_named_at_the_start() {
        use std::os::unix::fs::PermissionsExt;

        let (root, base, earlier) = with_an_earlier_release("earlier-kept");
        let closed = earlier.join("staging");
        let _ = std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o555));

        let (report, share) = sweep_and_share(
            OwnFolder::same_account(&base),
            Some(earlier.clone()),
            A_MOMENT,
        )
        .await;

        let _ = std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755));
        assert!(share.is_some(), "the start goes on");
        if report.ok() {
            // An account that the permissions do not hold for (root) removed
            // it.
            assert!(!earlier.exists());
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        assert!(closed.is_dir());
        assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
        assert_eq!(report.failed[0].canonical_path, earlier.to_string_lossy());
        assert_eq!(
            report.failed[0].reason,
            "left by an earlier release; removing it needs administrator rights"
        );
        // What was kept is not in this account's own folder: the removal
        // takes a run as administrator.
        assert!(needs_administrator(&report, false));
        let text = sweep_text(&report).unwrap_or_default();
        assert!(text.contains("1 item(s) could NOT be removed"), "{text}");
        drop(share);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A program of an earlier release that still runs holds the lock of its
    /// staging area: its folder is left to a later start, and the start says
    /// so.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_folder_that_an_earlier_release_still_uses_is_left_and_named() {
        let (root, base, earlier) = with_an_earlier_release("earlier-in-use");
        let lock = earlier.join("staging").join(IN_USE_LOCK);
        let running = std::fs::File::open(&lock);
        assert!(
            running
                .as_ref()
                .is_ok_and(|file| file.lock_shared().is_ok())
        );

        let (report, share) = sweep_and_share(
            OwnFolder::same_account(&base),
            Some(earlier.clone()),
            A_MOMENT,
        )
        .await;

        assert!(share.is_some());
        assert!(lock.exists(), "nothing of it was removed");
        assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
        assert_eq!(report.failed[0].canonical_path, earlier.to_string_lossy());
        assert_eq!(
            report.failed[0].reason,
            "left by an earlier release; a program of that release still uses it"
        );
        // Once that program has ended, the next start that sweeps removes it.
        drop(running);
        drop(share);
        let (report, share) = sweep_and_share(
            OwnFolder::same_account(&base),
            Some(earlier.clone()),
            A_MOMENT,
        )
        .await;
        assert!(report.ok(), "{:?}", report.failed);
        assert!(!earlier.exists());
        drop(share);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The removal of the folder of an earlier release is not waited for
    /// without end: a start that did not see it finish says so, and the
    /// removal goes on.
    #[tokio::test]
    async fn the_removal_of_an_earlier_release_is_not_waited_for_without_end() {
        let (root, _, earlier) = with_an_earlier_release("earlier-slow");

        let kept = remove_earlier_release(
            earlier.clone(),
            Remover::Start,
            false,
            std::time::Duration::ZERO,
        )
        .await;

        // Whether the removal was already done when the wait ended depends on
        // the machine; what is said when it was not does not.
        if let Some(why) = kept {
            assert_eq!(why, "its removal was not finished in time");
        }
        let deadline = std::time::Instant::now() + A_MOMENT;
        while earlier.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(!earlier.exists(), "the removal went on");
        // Nothing is there any more: nothing to say.
        assert_eq!(
            remove_earlier_release(earlier, Remover::Start, false, A_MOMENT).await,
            None
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// What a start leaves of an earlier release, and why.
    #[test]
    fn a_start_removes_unasked_only_what_no_program_uses_and_what_administrators_own() {
        // Without administrator rights no boundary is crossed: whose the
        // folder is does not matter.
        assert_eq!(start_leaves(false, false, false), None);
        assert_eq!(start_leaves(false, false, true), None);
        // With them, only a folder that the administrators own is removed
        // unasked: every account can make that folder and fill it.
        assert_eq!(start_leaves(true, false, true), None);
        assert!(
            start_leaves(true, false, false)
                .is_some_and(|why| why.starts_with("it belongs to an account"))
        );
        // A program of that release that still runs keeps its folder.
        for private in [false, true] {
            for administrators_own in [false, true] {
                assert_eq!(
                    start_leaves(private, true, administrators_own),
                    Some("a program of that release still uses it")
                );
            }
        }
    }

    /// Work that does not end is waited for only so long, and work that ends
    /// is waited for.
    #[tokio::test]
    async fn work_on_a_thread_of_its_own_is_waited_for_only_so_long() {
        let (let_go, held) = std::sync::mpsc::channel::<()>();
        let hung = within(std::time::Duration::from_millis(50), move || {
            // Ends only when the test lets go.
            let _ = held.recv();
        })
        .await;
        assert_eq!(hung, None);
        drop(let_go);

        assert_eq!(within(A_MOMENT, || 7).await, Some(7));
    }

    /// A removal that is refused to a program with administrator rights is
    /// not blamed on missing rights, and a recovery removes the folder
    /// whoever owns it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_refused_removal_is_said_as_it_is_and_a_recovery_removes_what_it_can() {
        use std::os::unix::fs::PermissionsExt;

        let (root, _, earlier) = with_an_earlier_release("earlier-refused");
        let closed = earlier.join("staging");
        let _ = std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o555));

        let kept = remove_earlier_release(earlier.clone(), Remover::Recovery, true, A_MOMENT).await;

        let _ = std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755));
        // An account that the permissions do not hold for (root) removed it.
        if earlier.exists() {
            assert_eq!(
                kept.as_deref(),
                Some("it could not be removed, also not with administrator rights")
            );
        }
        assert_eq!(
            remove_earlier_release(earlier.clone(), Remover::Recovery, true, A_MOMENT).await,
            None
        );
        assert!(!earlier.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn what_a_start_kept_is_removed_by_a_run_of_its_own_kind() {
        let own = StartupSweepReport {
            failed: vec![CleanupFailure {
                canonical_path: "C:/Users/ann/AppData/Local/CretLink/staging/x/a.txt".to_owned(),
                reason: "in use".to_owned(),
            }],
            ..StartupSweepReport::default()
        };
        // A program with administrator rights keeps its files where only
        // administrators can write.
        assert!(needs_administrator(&own, true));
        // A program without them keeps its files in the folder of its
        // account, where a run as administrator does not look.
        assert!(!needs_administrator(&own, false));
        // The folder of an earlier release is removed by an administrator,
        // whoever found it.
        let mut earlier = own.clone();
        earlier.failed.push(CleanupFailure {
            canonical_path: "C:/ProgramData/CretLink".to_owned(),
            reason: format!("{REASON_EARLIER_RELEASE}; removing it needs administrator rights"),
        });
        assert!(needs_administrator(&earlier, false));
    }

    #[test]
    fn a_run_with_administrator_rights_says_what_it_does_not_cover() {
        let report = recovery_report_text(&RecoveryReport::default());
        let with_rights = with_scope_note(report.clone(), true);
        assert!(with_rights.starts_with(&report));
        assert!(
            with_rights.ends_with(
                "  run this command there without administrator rights to see or remove it.\n"
            ),
            "{with_rights}"
        );
        assert!(with_rights.contains("  Note: this covers what CretLink keeps when it runs"));
        // A run without them covers the folder of its account, and says what
        // it cannot see as something it could not read.
        assert_eq!(with_scope_note(report.clone(), false), report);
    }
}
