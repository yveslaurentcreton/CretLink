//! The owned-resource inventory and its recovery verb (ST-M006-003, ADR-0014
//! §6).
//!
//! CretLink deliberately owns very little on the customer machine, and every
//! kind of thing it *can* own is enumerated here — not just the kinds a feature
//! creates today. [`ResourceKind`] lists staged file trees and sealed
//! reboot-resume tickets (both real today) alongside the kinds no feature
//! creates yet (a temporary service, a scheduled task, a network listener, a
//! temporary credential, and the reboot-resume OS auto-start entry). The kinds
//! nothing creates are *asserted empty* rather than omitted, so a later feature
//! that starts creating one cannot silently escape the inventory.
//!
//! Two operations sit on top of the inventory:
//!
//! * [`scan_at`] reads what residue exists, removing nothing, so a customer can
//!   inspect before deciding — evidence is never hidden.
//! * [`recover_at`] removes the known residue (staged trees with a full-strength
//!   sweep, and the sealed reboot-resume ticket store) and reports honestly:
//!   what was found, what was removed, and — for anything that could not be
//!   removed — the residue kept, the reason, and safe manual remediation, so a
//!   failure is never dressed up as success (REQ-F-019, REQ-N-008).
//!
//! The functions take an explicit staging base and [`TicketStore`] so the whole
//! surface is exercised in-process on every host; [`scan`] and [`recover`] are
//! the production wrappers over the platform defaults.

use std::path::Path;

use cretlink_exec::{FileStore, StagingCensus, StartupSweepReport, default_staging_base};
use cretlink_protocol::{CleanupTrigger, SessionId};

use crate::reboot_resume::TicketStore;

/// A kind of resource CretLink can own on the customer machine.
///
/// The list is exhaustive on purpose: kinds nothing creates yet are still
/// members so the inventory can assert them empty and keep itself honest as
/// later milestones add the features that populate them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    /// Staged file trees under the agent's staging root — created today by file
    /// transfer (M003) and session-scoped.
    StagedFiles,
    /// Sealed, single-use reboot-resume continuity tickets at rest — created
    /// today by arming (ST-M006-001) and machine-scoped.
    RebootResumeTicket,
    /// The OS auto-start entry that would relaunch the agent after a reboot for
    /// reboot-resume. Nothing in this codebase registers it yet, so it is
    /// asserted absent.
    OsAutoStartEntry,
    /// A temporary Windows service. No feature creates one yet; asserted absent.
    TemporaryService,
    /// A scheduled task. No feature creates one yet; asserted absent.
    ScheduledTask,
    /// A network listener held open past a session's life. No feature creates one
    /// yet; asserted absent.
    NetworkListener,
    /// A temporary credential minted on the customer machine. No feature creates
    /// one yet; asserted absent.
    TemporaryCredential,
}

impl ResourceKind {
    /// Every kind, in inventory order. Iterated so a scan visits all of them and
    /// none is forgotten.
    pub const ALL: [ResourceKind; 7] = [
        ResourceKind::StagedFiles,
        ResourceKind::RebootResumeTicket,
        ResourceKind::OsAutoStartEntry,
        ResourceKind::TemporaryService,
        ResourceKind::ScheduledTask,
        ResourceKind::NetworkListener,
        ResourceKind::TemporaryCredential,
    ];

    /// A short, stable, secret-free label for reports.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ResourceKind::StagedFiles => "staged files",
            ResourceKind::RebootResumeTicket => "reboot-resume ticket",
            ResourceKind::OsAutoStartEntry => "reboot-resume OS auto-start entry",
            ResourceKind::TemporaryService => "temporary service",
            ResourceKind::ScheduledTask => "scheduled task",
            ResourceKind::NetworkListener => "network listener",
            ResourceKind::TemporaryCredential => "temporary credential",
        }
    }

    /// Whether any feature in this build can create residue of this kind. The
    /// kinds that return `false` are always asserted empty — the inventory never
    /// runs a real scanner for them, so it cannot false-positive on an unrelated
    /// system service or task that merely resembles one.
    #[must_use]
    fn is_creatable(self) -> bool {
        matches!(
            self,
            ResourceKind::StagedFiles | ResourceKind::RebootResumeTicket
        )
    }
}

/// One residue the inventory found, named in secret-free terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedResource {
    /// The kind of resource.
    pub kind: ResourceKind,
    /// A secret-free identifier — a path, a ticket-id fingerprint — a customer
    /// can act on. Never key material: the reboot-resume secret stays sealed.
    pub description: String,
    /// The owning session where the resource is session-scoped (REQ-S-017);
    /// `None` for machine-scoped residue such as a reboot-resume ticket.
    pub session: Option<SessionId>,
}

/// A kind whose state could not be read at all — surfaced as unknown rather than
/// silently assumed empty (REQ-N-008); an unreadable store is still residue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadableKind {
    /// The kind whose state could not be determined.
    pub kind: ResourceKind,
    /// A secret-free reason.
    pub reason: String,
}

/// A read-only census of the CretLink residue on this machine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    /// Every residue found, across kinds.
    pub found: Vec<OwnedResource>,
    /// Kinds scanned and confirmed to hold nothing — the honest empty
    /// assertions that keep the inventory total.
    pub asserted_empty: Vec<ResourceKind>,
    /// Kinds whose state could not be read.
    pub unreadable: Vec<UnreadableKind>,
}

impl Inventory {
    /// Whether the machine is clean: nothing found and nothing unreadable.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.found.is_empty() && self.unreadable.is_empty()
    }
}

/// A residue that could not be removed: kept, with a named remediation, and left
/// for the next startup sweep to retry (REQ-F-019, REQ-N-008).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryFailure {
    /// The kind of resource that could not be removed.
    pub kind: ResourceKind,
    /// The secret-free identifier of the residue.
    pub description: String,
    /// A secret-free reason the removal failed.
    pub reason: String,
    /// Safe manual remediation a customer can apply.
    pub remediation: String,
}

/// The honest result of a recovery: what was found, what was removed, and what
/// was kept because it could not be removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// The residue present before removal — evidence, never hidden.
    pub found: Vec<OwnedResource>,
    /// The residue successfully removed.
    pub removed: Vec<OwnedResource>,
    /// The residue that could not be removed.
    pub failed: Vec<RecoveryFailure>,
    /// Kinds whose state could not be read.
    pub unreadable: Vec<UnreadableKind>,
}

impl RecoveryReport {
    /// Whether recovery left the machine clean: nothing failed and nothing
    /// unreadable. A recovery that could not prove a clean result reports
    /// `false`, never a hopeful success (REQ-N-008).
    #[must_use]
    pub fn ok(&self) -> bool {
        self.failed.is_empty() && self.unreadable.is_empty()
    }
}

/// Scans the CretLink residue under `staging_base` and `tickets`, removing
/// nothing. Every [`ResourceKind`] is visited: kinds a feature can create are
/// enumerated, the rest are asserted empty.
pub async fn scan_at(staging_base: &Path, tickets: &TicketStore) -> Inventory {
    let mut inventory = Inventory::default();

    // 1. Staged files — session-scoped residue enumerated read-only.
    let census: StagingCensus = FileStore::census_all_sessions(staging_base.to_path_buf()).await;
    for session in &census.sessions {
        for entry in &session.staged {
            inventory.found.push(OwnedResource {
                kind: ResourceKind::StagedFiles,
                description: entry.canonical_path.clone(),
                session: Some(session.session),
            });
        }
        for _ in 0..session.orphaned_parts {
            inventory.found.push(OwnedResource {
                kind: ResourceKind::StagedFiles,
                description: format!("orphaned upload part under session {}", session.session),
                session: Some(session.session),
            });
        }
    }
    if census.is_empty() {
        inventory.asserted_empty.push(ResourceKind::StagedFiles);
    }

    // 2. Reboot-resume tickets — machine-scoped; a malformed store is surfaced
    //    as unreadable rather than assumed empty.
    match tickets.load() {
        Ok(list) if list.is_empty() => {
            inventory
                .asserted_empty
                .push(ResourceKind::RebootResumeTicket);
        }
        Ok(list) => {
            for ticket in list {
                inventory.found.push(OwnedResource {
                    kind: ResourceKind::RebootResumeTicket,
                    description: format!(
                        "reboot-resume ticket {} (expires at {} unix-ms)",
                        ticket.ticket_id, ticket.not_after_unix_millis
                    ),
                    session: None,
                });
            }
        }
        Err(err) => inventory.unreadable.push(UnreadableKind {
            kind: ResourceKind::RebootResumeTicket,
            reason: err.to_string(),
        }),
    }

    // 3. Kinds no feature creates yet: asserted empty, honestly, so a later
    //    milestone that starts creating one cannot escape the inventory.
    for kind in ResourceKind::ALL {
        if !kind.is_creatable() {
            inventory.asserted_empty.push(kind);
        }
    }

    inventory
}

/// Removes the known CretLink residue under `staging_base` and `tickets`, and
/// reports honestly. Staged trees are removed with a full-strength
/// [`CleanupTrigger::All`] sweep (recovery clears everything, including
/// keep-until-shutdown artifacts); the sealed reboot-resume ticket store is
/// deleted. Evidence is recorded before anything is touched.
pub async fn recover_at(staging_base: &Path, tickets: &TicketStore) -> RecoveryReport {
    let mut report = RecoveryReport::default();

    // Evidence first: record everything present before removing anything so the
    // report can never hide what was here (ST-M006-003 AC3). Unreadable kinds are
    // carried through *except* the reboot-resume ticket store, whose outcome the
    // removal step below decides precisely: a malformed-but-removable store is
    // resolved (not a failure), while a store that cannot be removed becomes a
    // kept failure. Any other unreadable kind (none exist yet) recovery does not
    // act on, so it stays unknown and keeps `ok()` false.
    let before = scan_at(staging_base, tickets).await;
    report.found = before.found.clone();
    report.unreadable = before
        .unreadable
        .iter()
        .filter(|kind| kind.kind != ResourceKind::RebootResumeTicket)
        .cloned()
        .collect();

    // 1. Staged trees — the sweep removes and reports per-file failures, which
    //    it keeps for the next startup sweep to retry.
    let sweep: StartupSweepReport =
        FileStore::sweep_all_sessions(staging_base.to_path_buf(), CleanupTrigger::All).await;
    let failed_paths: std::collections::BTreeSet<&str> = sweep
        .failed
        .iter()
        .map(|failure| failure.canonical_path.as_str())
        .collect();
    for resource in before
        .found
        .iter()
        .filter(|resource| resource.kind == ResourceKind::StagedFiles)
    {
        if !failed_paths.contains(resource.description.as_str()) {
            report.removed.push(resource.clone());
        }
    }
    for failure in &sweep.failed {
        report.failed.push(RecoveryFailure {
            kind: ResourceKind::StagedFiles,
            description: failure.canonical_path.clone(),
            reason: failure.reason.clone(),
            remediation: format!("delete the file manually: {}", failure.canonical_path),
        });
    }

    // 2. Reboot-resume ticket store — one removal clears both readable tickets
    //    and an unreadable (malformed) store; a failure keeps the residue and
    //    names the exact file to delete by hand.
    let ticket_residue: Vec<OwnedResource> = before
        .found
        .iter()
        .filter(|resource| resource.kind == ResourceKind::RebootResumeTicket)
        .cloned()
        .collect();
    let store_unreadable = before
        .unreadable
        .iter()
        .any(|kind| kind.kind == ResourceKind::RebootResumeTicket);
    if !ticket_residue.is_empty() || store_unreadable {
        let remediation = format!(
            "delete the sealed reboot-resume ticket store manually: {}",
            tickets.path().display()
        );
        match tickets.remove() {
            Ok(_) => report.removed.extend(ticket_residue),
            Err(err) if ticket_residue.is_empty() => report.failed.push(RecoveryFailure {
                kind: ResourceKind::RebootResumeTicket,
                description: format!("unreadable ticket store {}", tickets.path().display()),
                reason: err.to_string(),
                remediation,
            }),
            Err(err) => {
                for resource in ticket_residue {
                    report.failed.push(RecoveryFailure {
                        kind: ResourceKind::RebootResumeTicket,
                        description: resource.description,
                        reason: err.to_string(),
                        remediation: remediation.clone(),
                    });
                }
            }
        }
    }

    report
}

/// Scans the platform-default CretLink residue (the production entry point).
pub async fn scan(tickets: &TicketStore) -> Inventory {
    scan_at(&default_staging_base(), tickets).await
}

/// Recovers the platform-default CretLink residue (the production entry point).
pub async fn recover(tickets: &TicketStore) -> RecoveryReport {
    recover_at(&default_staging_base(), tickets).await
}

#[cfg(test)]
mod tests;
