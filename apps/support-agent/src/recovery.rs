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

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use cretlink_exec::FileStore;
/// The startup sweep's report, re-exported for the application's customer
/// role, which maps it to its notice.
pub use cretlink_exec::StartupSweepReport;
use cretlink_protocol::CleanupTrigger;

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

/// Runs the recovery verb on the platform defaults. Without `remove` it
/// inspects and lists the CretLink-owned residue, removing nothing; with
/// `remove` it removes what it can and reports honestly what was kept.
/// `program` is the command the customer typed (`cretlink`, `cretlink-cli`,
/// `cretlink-support-agent`, or a `CretLink-Support*` copy) and names the
/// follow-up command in the report.
pub async fn run_recover(program: &str, remove: bool, tickets: &TicketStore) -> RecoverRun {
    if remove {
        let report = crate::inventory::recover(tickets).await;
        RecoverRun {
            text: recovery_report_text(&report),
            success: report.ok(),
        }
    } else {
        let inventory = crate::inventory::scan(tickets).await;
        RecoverRun {
            text: inventory_text(program, &inventory),
            success: true,
        }
    }
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
/// they do not exist yet. A process that may not write there (an unelevated
/// window after an elevated one created it) opens it for reading, which is
/// enough to lock it.
fn open_in_use(base: &Path) -> std::io::Result<std::fs::File> {
    let path = base.join(IN_USE_LOCK);
    let created = std::fs::create_dir_all(base).and_then(|()| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
    });
    created.or_else(|_| std::fs::File::open(&path))
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
pub async fn sweep_stale_staging() -> (StartupSweepReport, Option<StagingInUse>) {
    sweep_stale_staging_in(cretlink_exec::default_staging_base()).await
}

/// [`sweep_stale_staging`] with an explicit staging base, for tests.
pub async fn sweep_stale_staging_in(base: PathBuf) -> (StartupSweepReport, Option<StagingInUse>) {
    let Ok(file) = open_in_use(&base) else {
        // Whether another window runs a session cannot be told: leave every
        // tree for a later start.
        return (StartupSweepReport::default(), None);
    };
    let report = match file.try_lock() {
        Ok(()) => {
            let report = FileStore::sweep_all_sessions(base, CleanupTrigger::Shutdown).await;
            let _ = file.unlock();
            report
        }
        // Another process runs (or sweeps): its trees may be live.
        Err(_) => StartupSweepReport::default(),
    };
    // Waits only while another process is sweeping, never for a session.
    let held = tokio::task::spawn_blocking(move || file.lock_shared().map(|()| file)).await;
    match held {
        Ok(Ok(file)) => (report, Some(StagingInUse { _file: file })),
        _ => (report, None),
    }
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
            "  {} artifact(s) could NOT be removed and will be retried on the next start:",
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
    use cretlink_protocol::{CleanupFailure, SessionId};

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
            found: vec![staged("C:/ProgramData/CretLink/staging/x/a.txt")],
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
        let kept = staged("C:/ProgramData/CretLink/staging/x/b.txt");
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
            failed: vec![CleanupFailure {
                canonical_path: "C:/x/a.txt".to_owned(),
                reason: "in use".to_owned(),
            }],
        };
        let text = sweep_text(&report).unwrap_or_default();
        assert!(text.contains("removed 2 leftover artifact(s)"));
        assert!(text.contains("1 artifact(s) could NOT be removed"));
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
}
