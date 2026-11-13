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
//!
//! # What is not read (ST-M006-020)
//!
//! - What lies in the staging area under the name of a session and is not
//!   what CretLink makes there — a link, a file, a session folder with a link
//!   in the place of its records — is residue too
//!   ([`ResourceKind::NotCretLinks`]). It is named, never read, and the
//!   recovery removes it whole.
//! - The folder in which releases up to 0.24.0 kept these files, where every
//!   account could write, is named ([`ResourceKind::EarlierRelease`]), never
//!   read, and the recovery removes it whole.
//!
//! # Two keepers
//!
//! A program with administrator rights keeps its files where only
//! administrators can write, a program without them in the folder of its
//! account, and neither works in the place of the other. So a scan and a
//! recovery cover the place of the kind of program that runs them. A program
//! without administrator rights cannot read the other place; where that place
//! exists, the scan says that there is something it cannot see, and never
//! calls the machine clean.

use std::path::{Path, PathBuf};

use cretlink_exec::{
    FileStore, OUTSIDE_STAGING_REASON, OwnFolder, StagingCensus, StartupSweepReport,
};
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
    /// Something in the staging area, under the name of a session, that is
    /// not what CretLink makes there: a link, a file, or a session folder
    /// with a link in the place of its records (ST-M006-020). It is named by
    /// its path and never read.
    NotCretLinks,
    /// The folder in which releases up to 0.24.0 kept the staging area and the
    /// reboot tickets, where every account could write (ST-M006-020). It is
    /// named by its path and never read.
    EarlierRelease,
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
    pub const ALL: [ResourceKind; 9] = [
        ResourceKind::StagedFiles,
        ResourceKind::RebootResumeTicket,
        ResourceKind::NotCretLinks,
        ResourceKind::EarlierRelease,
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
            ResourceKind::NotCretLinks => "entry that CretLink did not make",
            ResourceKind::EarlierRelease => "folder of an earlier release",
            ResourceKind::OsAutoStartEntry => "reboot-resume OS auto-start entry",
            ResourceKind::TemporaryService => "temporary service",
            ResourceKind::ScheduledTask => "scheduled task",
            ResourceKind::NetworkListener => "network listener",
            ResourceKind::TemporaryCredential => "temporary credential",
        }
    }

    /// Whether a scan of this build looks for residue of this kind: any
    /// feature can create it, or it can be found in CretLink's own folders. The
    /// kinds that return `false` are always asserted empty — the inventory never
    /// runs a real scanner for them, so it cannot false-positive on an unrelated
    /// system service or task that merely resembles one.
    #[must_use]
    fn is_creatable(self) -> bool {
        matches!(
            self,
            ResourceKind::StagedFiles
                | ResourceKind::RebootResumeTicket
                | ResourceKind::NotCretLinks
                | ResourceKind::EarlierRelease
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

/// Where a scan or a recovery looks.
#[derive(Debug, Clone)]
struct Places {
    /// The staging area of this process.
    staging: OwnFolder,
    /// The folder in which releases up to 0.24.0 kept their files, where
    /// there can be one.
    earlier_release: Option<PathBuf>,
    /// The folder of a program with administrator rights, when this process
    /// has none and so cannot read it.
    out_of_reach: Option<PathBuf>,
    /// Whether the place of an earlier release could not be told on a system
    /// where there can be one: then nothing is known about it.
    earlier_release_unknown: bool,
}

impl Places {
    /// A staging area alone: a sandbox.
    fn only(staging: OwnFolder) -> Self {
        Self {
            staging,
            earlier_release: None,
            out_of_reach: None,
            earlier_release_unknown: false,
        }
    }

    /// The places of this process.
    fn of_this_process() -> Self {
        let staging = OwnFolder::staging_for_this_process();
        let out_of_reach = if staging.is_private() {
            None
        } else {
            cretlink_exec::administrators_folder()
        };
        let earlier_release = cretlink_exec::earlier_release_folder();
        Self {
            staging,
            // Off Windows no earlier release kept anything elsewhere.
            earlier_release_unknown: cfg!(windows) && earlier_release.is_none(),
            earlier_release,
            out_of_reach,
        }
    }
}

/// How the reason starts for what a program with administrator rights kept
/// and this one, without them, cannot see.
const OUT_OF_REACH: &str = "what CretLink kept with administrator rights";

/// Whether something stands at `path`. What cannot be looked at counts as
/// being there: only "not found" says that nothing is.
async fn stands(path: &Path) -> bool {
    !matches!(
        tokio::fs::symlink_metadata(path).await,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

/// Scans the CretLink residue under `staging_base` and `tickets`, removing
/// nothing. Every [`ResourceKind`] is visited: kinds a feature can create are
/// enumerated, the rest are asserted empty.
pub async fn scan_at(staging_base: impl Into<OwnFolder>, tickets: &TicketStore) -> Inventory {
    scan_in(&Places::only(staging_base.into()), tickets).await
}

/// [`scan_at`] over every place a scan looks.
async fn scan_in(places: &Places, tickets: &TicketStore) -> Inventory {
    let mut inventory = Inventory::default();

    // 1. Staged files — session-scoped residue enumerated read-only.
    let census: StagingCensus = FileStore::census_all_sessions(&places.staging).await;
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
    // Where CretLink's records could not be read, what is staged is unknown:
    // never asserted empty (REQ-N-008).
    for path in &census.unread {
        inventory.unreadable.push(UnreadableKind {
            kind: ResourceKind::StagedFiles,
            reason: format!("the staging records in {path} could not be read"),
        });
    }
    // What a program with administrator rights kept cannot be seen without
    // those rights: where its folder exists, this scan does not know.
    let mut out_of_reach = false;
    if let Some(folder) = &places.out_of_reach
        && stands(folder).await
    {
        out_of_reach = true;
        inventory.unreadable.push(UnreadableKind {
            kind: ResourceKind::StagedFiles,
            reason: format!(
                "{OUT_OF_REACH}, in {}, can only be read and removed by this command run as an \
                 administrator",
                folder.display()
            ),
        });
    }
    if census.sessions.is_empty() && census.unread.is_empty() && !out_of_reach {
        inventory.asserted_empty.push(ResourceKind::StagedFiles);
    }
    // What CretLink did not make is named by its path, and nothing in it is
    // read (ST-M006-020).
    for path in &census.foreign {
        inventory.found.push(OwnedResource {
            kind: ResourceKind::NotCretLinks,
            description: path.clone(),
            session: None,
        });
    }
    if census.foreign.is_empty() {
        inventory.asserted_empty.push(ResourceKind::NotCretLinks);
    }

    // 2. The folder of an earlier release — named, never read (ST-M006-020).
    match &places.earlier_release {
        Some(folder) if stands(folder).await => inventory.found.push(OwnedResource {
            kind: ResourceKind::EarlierRelease,
            description: folder.to_string_lossy().into_owned(),
            session: None,
        }),
        // Where that folder is could not be told: nothing is known about it.
        None if places.earlier_release_unknown => inventory.unreadable.push(UnreadableKind {
            kind: ResourceKind::EarlierRelease,
            reason: "the folder in which an earlier release kept its files could not be told"
                .to_string(),
        }),
        _ => inventory.asserted_empty.push(ResourceKind::EarlierRelease),
    }

    // 3. Reboot-resume tickets — machine-scoped; a malformed store is surfaced
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

    // 4. Kinds no feature creates yet: asserted empty, honestly, so a later
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
pub async fn recover_at(
    staging_base: impl Into<OwnFolder>,
    tickets: &TicketStore,
) -> RecoveryReport {
    recover_in(&Places::only(staging_base.into()), tickets).await
}

/// [`recover_at`] over every place a recovery looks. It also removes, whole
/// and without reading it, the folder of an earlier release; and when it left
/// nothing, it removes CretLink's own folders too, as far as they are empty.
async fn recover_in(places: &Places, tickets: &TicketStore) -> RecoveryReport {
    let staging_base = &places.staging;
    let mut report = RecoveryReport::default();

    // Evidence first: record everything present before removing anything so the
    // report can never hide what was here (ST-M006-003 AC3).
    let before = scan_in(places, tickets).await;
    report.found = before.found.clone();

    // 1. Staged trees — the sweep removes and reports per-file failures, which
    //    it keeps for the next startup sweep to retry. It also removes, whole
    //    and without reading it, what CretLink did not make.
    let sweep: StartupSweepReport =
        FileStore::sweep_all_sessions(staging_base, CleanupTrigger::All).await;
    for failure in &sweep.failed {
        let kind = before
            .found
            .iter()
            .find(|resource| resource.description == failure.canonical_path)
            .map_or(ResourceKind::StagedFiles, |resource| resource.kind);
        // A manifest entry that named a path outside the staging area was
        // dropped and its path left alone: it is not CretLink's to remove.
        let remediation = if failure.reason.starts_with(OUTSIDE_STAGING_REASON) {
            "none: the path is not CretLink's and was not touched".to_string()
        } else {
            format!(
                "delete it manually, as an administrator: {}",
                failure.canonical_path
            )
        };
        report.failed.push(RecoveryFailure {
            kind,
            description: failure.canonical_path.clone(),
            reason: failure.reason.clone(),
            remediation,
        });
    }

    // 2. The folder of an earlier release — removed whole, without reading it
    //    and without following a link (ST-M006-020).
    if let Some(folder) = &places.earlier_release
        && before
            .found
            .iter()
            .any(|resource| resource.kind == ResourceKind::EarlierRelease)
        && let Some(reason) = crate::recovery::remove_earlier_release(
            folder.clone(),
            crate::recovery::Remover::Recovery,
            staging_base.is_private(),
            crate::recovery::EARLIER_RELEASE_RECOVERY_WAIT,
        )
        .await
    {
        report.failed.push(RecoveryFailure {
            kind: ResourceKind::EarlierRelease,
            description: folder.to_string_lossy().into_owned(),
            reason,
            remediation: format!(
                "close every CretLink window, then run this command again as an administrator, \
                 or delete the folder manually: {}",
                folder.display()
            ),
        });
    }

    // 3. Reboot-resume ticket store — one removal clears both readable tickets
    //    and an unreadable (malformed) store; a failure keeps the residue and
    //    names the exact file to delete by hand.
    let ticket_residue: Vec<&OwnedResource> = before
        .found
        .iter()
        .filter(|resource| resource.kind == ResourceKind::RebootResumeTicket)
        .collect();
    let store_unreadable = before
        .unreadable
        .iter()
        .any(|kind| kind.kind == ResourceKind::RebootResumeTicket);
    if (!ticket_residue.is_empty() || store_unreadable)
        && let Err(err) = tickets.remove()
    {
        let remediation = format!(
            "delete the sealed reboot-resume ticket store manually: {}",
            tickets.path().display()
        );
        if ticket_residue.is_empty() {
            report.failed.push(RecoveryFailure {
                kind: ResourceKind::RebootResumeTicket,
                description: format!("unreadable ticket store {}", tickets.path().display()),
                reason: err.to_string(),
                remediation,
            });
        } else {
            for resource in ticket_residue {
                report.failed.push(RecoveryFailure {
                    kind: ResourceKind::RebootResumeTicket,
                    description: resource.description.clone(),
                    reason: err.to_string(),
                    remediation: remediation.clone(),
                });
            }
        }
    }

    // 4. What the recovery removed is what is no longer there and what no
    //    step named as kept: a second look decides, not what a step said of
    //    itself. Something that is still there and that no step named is said
    //    to be still there (REQ-N-008).
    let after = scan_in(places, tickets).await;
    let still_there = |resource: &OwnedResource| {
        after
            .found
            .iter()
            .any(|left| left.kind == resource.kind && left.description == resource.description)
    };
    for resource in &before.found {
        let named_as_kept = report
            .failed
            .iter()
            .any(|failure| failure.description == resource.description);
        if named_as_kept {
            continue;
        }
        if !still_there(resource) {
            report.removed.push(resource.clone());
        } else if !report
            .failed
            .iter()
            .any(|failure| failure.kind == resource.kind)
        {
            report.failed.push(RecoveryFailure {
                kind: resource.kind,
                description: resource.description.clone(),
                reason: "still there after the recovery".to_string(),
                remediation: format!(
                    "delete it manually, as an administrator: {}",
                    resource.description
                ),
            });
        }
    }
    // What still cannot be read is unknown, unless a failure of that kind
    // already says what was kept. What lies out of this run's reach is said
    // in any case: no failure of this run is about it.
    report.unreadable = after
        .unreadable
        .into_iter()
        .filter(|unknown| {
            unknown.reason.starts_with(OUT_OF_REACH)
                || !report
                    .failed
                    .iter()
                    .any(|failure| failure.kind == unknown.kind)
        })
        .collect();

    // 5. A recovery that left nothing in this process's own folders leaves
    //    no empty folder of CretLink behind either.
    if report.failed.is_empty() && after.found.is_empty() {
        remove_empty_folders(staging_base, tickets).await;
    }

    report
}

/// Removes the lock of the staging area, unless a CretLink program holds it,
/// and then the staging area, the folder of the ticket store and the folder
/// above them, as far as they are empty.
///
/// A program that has opened the lock and has not yet taken its share at this
/// moment is not seen: it then holds a lock that no later program finds. The
/// recovery is therefore run when no CretLink window is open or starting (see
/// the residual risks in `docs/threat-model.md`).
async fn remove_empty_folders(staging: &OwnFolder, tickets: &TicketStore) {
    let staging = staging.clone();
    let tickets = tickets.folder().cloned();
    let _ = tokio::task::spawn_blocking(move || {
        let lock = staging.path().join(crate::recovery::IN_USE_LOCK);
        if cretlink_exec::Standing::of_file(&lock) == cretlink_exec::Standing::Ours
            && let Ok(file) = std::fs::File::open(&lock)
        {
            // A program that runs holds a share of the lock: its folders stay.
            if file.try_lock().is_err() {
                return;
            }
            let _ = std::fs::remove_file(&lock);
        }
        if let Some(tickets) = tickets {
            tickets.remove_if_empty();
        }
        staging.remove_if_empty();
    })
    .await;
}

/// Scans the platform-default CretLink residue (the production entry point).
pub async fn scan(tickets: &TicketStore) -> Inventory {
    scan_in(&Places::of_this_process(), tickets).await
}

/// Recovers the platform-default CretLink residue (the production entry point).
pub async fn recover(tickets: &TicketStore) -> RecoveryReport {
    recover_in(&Places::of_this_process(), tickets).await
}

#[cfg(test)]
mod tests;
